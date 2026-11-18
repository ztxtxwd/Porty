//! A resilient SSH client session built on [`russh`].
//!
//! The session owns its own background task and reconnects with exponential
//! backoff. Callers never touch the [`russh`] handle directly: they ask for the
//! current handle or use the `exec` / `open_direct_tcpip` helpers, so a
//! reconnect transparently heals running forwarders.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use russh::client::{self, AuthResult, Handle};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::{current_username, AuthKind, HostConfig};
use crate::CoreEvent;

type DynAgent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

/// Observable connection state of a host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionState {
    #[default]
    Disconnected,
    Connecting,
    Authenticating,
    Connected,
    Reconnecting {
        attempt: u32,
    },
    Error {
        message: String,
    },
}

impl SessionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, SessionState::Connected)
    }

    pub fn is_transitional(&self) -> bool {
        matches!(
            self,
            SessionState::Connecting | SessionState::Authenticating
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            SessionState::Disconnected => "Disconnected",
            SessionState::Connecting => "Connecting",
            SessionState::Authenticating => "Authenticating",
            SessionState::Connected => "Connected",
            SessionState::Reconnecting { .. } => "Reconnecting",
            SessionState::Error { .. } => "Error",
        }
    }
}

/// Static configuration for one connection.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub host_id: String,
    pub host_name: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthKind,
    pub identity_files: Vec<std::path::PathBuf>,
    pub password: Option<String>,
    pub keepalive: Duration,
    pub connect_timeout: Duration,
}

impl SessionConfig {
    pub fn from_host(host: &HostConfig) -> Self {
        let user = host
            .user
            .clone()
            .filter(|user| !user.trim().is_empty())
            .unwrap_or_else(current_username);
        Self {
            host_id: host.id.clone(),
            host_name: host.host_name.clone(),
            port: host.port,
            user,
            auth: host.auth,
            identity_files: host.candidate_keys(),
            password: host.password.clone(),
            keepalive: Duration::from_secs(20),
            connect_timeout: Duration::from_secs(15),
        }
    }

    fn target(&self) -> String {
        format!("{}@{}:{}", self.user, self.host_name, self.port)
    }
}

/// Host-key verifier. Behaves like OpenSSH's `StrictHostKeyChecking=accept-new`:
/// known hosts are verified, unknown hosts are recorded on first use.
pub struct ClientHandler {
    host: String,
    port: u16,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = server_public_key.public_key();
        match russh::keys::check_known_hosts(&self.host, self.port, &key) {
            Ok(true) => Ok(true),
            Ok(false) => {
                if let Err(err) =
                    russh::keys::known_hosts::learn_known_hosts(&self.host, self.port, &key)
                {
                    warn!(host = %self.host, "could not update known_hosts: {err}");
                }
                Ok(true)
            }
            Err(err) => {
                // A missing/unreadable known_hosts file must not block use of
                // the app in constrained environments.
                debug!(host = %self.host, "known_hosts check skipped: {err}");
                Ok(true)
            }
        }
    }
}

/// One SSH connection to a host, with automatic reconnect.
pub struct SshSession {
    config: SessionConfig,
    handle: tokio::sync::RwLock<Option<Arc<Handle<ClientHandler>>>>,
    state_tx: watch::Sender<SessionState>,
    rtt_ms: AtomicU32,
    cancel: CancellationToken,
    events: tokio::sync::broadcast::Sender<CoreEvent>,
}

impl SshSession {
    pub fn new(config: SessionConfig, events: tokio::sync::broadcast::Sender<CoreEvent>) -> Self {
        let (state_tx, _) = watch::channel(SessionState::Disconnected);
        Self {
            config,
            handle: tokio::sync::RwLock::new(None),
            state_tx,
            rtt_ms: AtomicU32::new(0),
            cancel: CancellationToken::new(),
            events,
        }
    }

    pub fn host_id(&self) -> &str {
        &self.config.host_id
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    pub fn state(&self) -> SessionState {
        self.state_tx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<SessionState> {
        self.state_tx.subscribe()
    }

    pub fn rtt_ms(&self) -> Option<u32> {
        match self.rtt_ms.load(Ordering::Relaxed) {
            0 => None,
            rtt => Some(rtt),
        }
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// The current transport, if authenticated.
    pub async fn current_handle(&self) -> Option<Arc<Handle<ClientHandler>>> {
        self.handle.read().await.clone()
    }

    /// Start the connect/reconnect loop. Must be called from within a Tokio runtime.
    pub fn start(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move { this.run().await });
    }

    /// Stop the reconnect loop and close the transport.
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    fn set_state(&self, state: SessionState, detail: Option<String>) {
        self.state_tx.send_replace(state.clone());
        let _ = self.events.send(CoreEvent::HostStateChanged {
            host_id: self.config.host_id.clone(),
            state,
            detail,
        });
    }

    async fn run(self: Arc<Self>) {
        let mut attempt: u32 = 0;
        loop {
            if self.cancel.is_cancelled() {
                break;
            }

            let state = if attempt == 0 {
                SessionState::Connecting
            } else {
                SessionState::Reconnecting { attempt }
            };
            self.set_state(state, None);

            match self.connect_once().await {
                Ok(handle) => {
                    *self.handle.write().await = Some(handle.clone());
                    self.set_state(SessionState::Connected, None);
                    info!(target = %self.config.target(), "ssh connected");

                    let reason = self.keepalive_loop(handle.clone()).await;
                    *self.handle.write().await = None;
                    self.rtt_ms.store(0, Ordering::Relaxed);
                    let _ = self.events.send(CoreEvent::HostMetrics {
                        host_id: self.config.host_id.clone(),
                        rtt_ms: None,
                    });

                    if self.cancel.is_cancelled() {
                        break;
                    }
                    // Disconnected unexpectedly: retry from the first backoff step.
                    attempt = 1;
                    self.set_state(
                        SessionState::Reconnecting { attempt },
                        Some(reason.unwrap_or_else(|| "connection closed".to_string())),
                    );
                }
                Err(err) => {
                    if self.cancel.is_cancelled() {
                        break;
                    }
                    attempt = attempt.saturating_add(1);
                    let message = format!("{err:#}");
                    warn!(target = %self.config.target(), "ssh connect failed: {message}");
                    self.set_state(SessionState::Reconnecting { attempt }, Some(message));
                }
            }

            // 2, 4, 8, 16, 30, 30 ... seconds.
            let backoff = Duration::from_secs(1u64 << attempt.min(4));
            let jitter = Duration::from_millis((attempt as u64 * 137) % 500);
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                _ = tokio::time::sleep(backoff + jitter) => {}
            }
        }

        *self.handle.write().await = None;
        self.rtt_ms.store(0, Ordering::Relaxed);
        self.set_state(SessionState::Disconnected, None);
        info!(target = %self.config.target(), "ssh session stopped");
    }

    /// Ping every `keepalive`; returns the reason the transport was lost.
    async fn keepalive_loop(&self, handle: Arc<Handle<ClientHandler>>) -> Option<String> {
        let interval = self.config.keepalive.max(Duration::from_secs(5));
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    let _ = handle
                        .disconnect(russh::Disconnect::ByApplication, "", "en")
                        .await;
                    return None;
                }
                _ = ticker.tick() => {
                    let started = Instant::now();
                    match tokio::time::timeout(Duration::from_secs(10), handle.send_ping()).await {
                        Ok(Ok(())) => {
                            let rtt = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
                            self.rtt_ms.store(rtt.max(1), Ordering::Relaxed);
                            let _ = self.events.send(CoreEvent::HostMetrics {
                                host_id: self.config.host_id.clone(),
                                rtt_ms: Some(rtt.max(1)),
                            });
                        }
                        Ok(Err(err)) => return Some(format!("keepalive failed: {err}")),
                        Err(_) => return Some("keepalive timed out".to_string()),
                    }
                }
            }
        }
    }

    async fn connect_once(&self) -> Result<Arc<Handle<ClientHandler>>> {
        let config = Arc::new(client::Config {
            keepalive_interval: Some(self.config.keepalive),
            keepalive_max: 3,
            nodelay: true,
            ..Default::default()
        });
        let handler = ClientHandler {
            host: self.config.host_name.clone(),
            port: self.config.port,
        };
        let connect = client::connect(
            config,
            (self.config.host_name.as_str(), self.config.port),
            handler,
        );

        let mut handle = match tokio::time::timeout(self.config.connect_timeout, connect).await {
            Ok(Ok(handle)) => handle,
            Ok(Err(err)) => {
                return Err(anyhow!(
                    "could not connect to {}: {err}",
                    self.config.target()
                ))
            }
            Err(_) => bail!(
                "connection to {} timed out after {:?}",
                self.config.target(),
                self.config.connect_timeout
            ),
        };

        self.set_state(SessionState::Authenticating, None);
        self.authenticate(&mut handle).await?;
        Ok(Arc::new(handle))
    }

    async fn authenticate(&self, handle: &mut Handle<ClientHandler>) -> Result<()> {
        let methods: [AuthKind; 3] = match self.config.auth {
            AuthKind::Agent => [AuthKind::Agent, AuthKind::Key, AuthKind::Password],
            AuthKind::Key => [AuthKind::Key, AuthKind::Agent, AuthKind::Password],
            AuthKind::Password => [AuthKind::Password, AuthKind::Key, AuthKind::Agent],
        };

        let mut notes: Vec<String> = Vec::new();
        for method in methods {
            let result = match method {
                AuthKind::Agent => self.try_agent(handle).await,
                AuthKind::Key => self.try_keys(handle).await,
                AuthKind::Password => self.try_password(handle).await,
            };
            match result {
                Ok(true) => {
                    info!(target = %self.config.target(), "authenticated via {}", method.label());
                    return Ok(());
                }
                Ok(false) => notes.push(format!("{}: no working credentials", method.label())),
                Err(err) => notes.push(format!("{}: {err:#}", method.label())),
            }
        }

        bail!(
            "authentication failed for {} [{}]",
            self.config.target(),
            notes.join("; ")
        )
    }

    async fn try_agent(&self, handle: &mut Handle<ClientHandler>) -> Result<bool> {
        let Some(mut agent) = connect_agent().await else {
            return Ok(false);
        };
        let identities = agent
            .request_identities()
            .await
            .map_err(|err| anyhow!("listing agent identities: {err}"))?;
        if identities.is_empty() {
            return Ok(false);
        }
        let hash_alg = handle
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .flatten();
        for identity in identities {
            let public_key = identity.public_key().into_owned();
            match handle
                .authenticate_publickey_with(
                    self.config.user.clone(),
                    public_key,
                    hash_alg,
                    &mut agent,
                )
                .await
            {
                Ok(AuthResult::Success) => return Ok(true),
                Ok(AuthResult::Failure { .. }) => continue,
                Err(err) => {
                    debug!(target = %self.config.target(), "agent identity rejected: {err}");
                    continue;
                }
            }
        }
        Ok(false)
    }

    async fn try_keys(&self, handle: &mut Handle<ClientHandler>) -> Result<bool> {
        if self.config.identity_files.is_empty() {
            return Ok(false);
        }
        for path in &self.config.identity_files {
            if !path.exists() {
                continue;
            }
            let key = match russh::keys::load_secret_key(path, None) {
                Ok(key) => key,
                Err(err) => {
                    debug!(path = %path.display(), "skipping key: {err}");
                    continue;
                }
            };
            let hash_alg = if key.algorithm().is_rsa() {
                handle
                    .best_supported_rsa_hash()
                    .await
                    .ok()
                    .flatten()
                    .flatten()
            } else {
                None
            };
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
            match handle
                .authenticate_publickey(self.config.user.clone(), key)
                .await
            {
                Ok(AuthResult::Success) => return Ok(true),
                Ok(AuthResult::Failure { .. }) => continue,
                Err(err) => {
                    debug!(path = %path.display(), "key rejected: {err}");
                    continue;
                }
            }
        }
        Ok(false)
    }

    async fn try_password(&self, handle: &mut Handle<ClientHandler>) -> Result<bool> {
        let Some(password) = self.config.password.clone() else {
            return Ok(false);
        };
        match handle
            .authenticate_password(self.config.user.clone(), password)
            .await
        {
            Ok(AuthResult::Success) => Ok(true),
            Ok(AuthResult::Failure { .. }) => Ok(false),
            Err(err) => Err(anyhow!("password authentication error: {err}")),
        }
    }

    /// Run a command on the remote and capture stdout. Returns an error when
    /// the command exits non-zero.
    pub async fn exec(&self, command: &str) -> Result<String> {
        let handle = self
            .current_handle()
            .await
            .ok_or_else(|| anyhow!("host is not connected"))?;
        let mut channel = handle.channel_open_session().await?;
        channel.exec(true, command).await?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;
        while let Some(message) = channel.wait().await {
            match message {
                russh::ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                russh::ChannelMsg::ExtendedData { data, ext: 1 } => stderr.extend_from_slice(&data),
                russh::ChannelMsg::ExitStatus { exit_status } => exit_code = Some(exit_status),
                russh::ChannelMsg::Eof | russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
        let _ = channel.close().await;

        match exit_code {
            Some(0) | None => Ok(String::from_utf8_lossy(&stdout).into_owned()),
            Some(code) => bail!(
                "remote command exited with status {code}: {}",
                String::from_utf8_lossy(&stderr).trim()
            ),
        }
    }

    /// Open a `direct-tcpip` channel through the multiplexed connection.
    pub async fn open_direct_tcpip(
        &self,
        remote_host: &str,
        remote_port: u16,
        originator_address: &str,
        originator_port: u16,
    ) -> Result<russh::Channel<client::Msg>> {
        let handle = self
            .current_handle()
            .await
            .ok_or_else(|| anyhow!("host is not connected"))?;
        let channel = handle
            .channel_open_direct_tcpip(
                remote_host.to_string(),
                remote_port as u32,
                originator_address.to_string(),
                originator_port as u32,
            )
            .await?;
        Ok(channel)
    }
}

/// Connect to the platform SSH agent, normalising every backend to one type.
async fn connect_agent() -> Option<DynAgent> {
    #[cfg(unix)]
    {
        match AgentClient::connect_env().await {
            Ok(agent) => Some(agent.dynamic()),
            Err(err) => {
                debug!("ssh agent unavailable: {err}");
                None
            }
        }
    }
    #[cfg(windows)]
    {
        // OpenSSH for Windows exposes the agent as a named pipe, and some
        // setups still run PuTTY's Pageant.
        if let Some(sock) = std::env::var_os("SSH_AUTH_SOCK") {
            match AgentClient::connect_named_pipe(&sock).await {
                Ok(agent) => return Some(agent.dynamic()),
                Err(err) => debug!("named pipe agent unavailable: {err}"),
            }
        }
        match AgentClient::connect_pageant().await {
            Ok(agent) => Some(agent.dynamic()),
            Err(err) => {
                debug!("pageant unavailable: {err}");
                None
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}
