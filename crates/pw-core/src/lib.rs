//! Porty core: a UI-agnostic SSH port forwarding and discovery engine.
//!
//! The crate has no UI dependencies and no global state. A [`Porty`]
//! service owns the Tokio tasks for every host session, local listener and
//! discovery loop, and broadcasts [`CoreEvent`]s to whichever frontend is
//! attached (the GPUI desktop app today, a CLI or iOS app tomorrow).
//!
//! ```no_run
//! use std::sync::Arc;
//! use pw_core::{Porty, config};
//!
//! # async fn demo() -> anyhow::Result<()> {
//! let rt = tokio::runtime::Handle::current();
//! let service = Porty::open(rt, config::default_settings_path())?;
//! let mut events = service.subscribe();
//! service.connect("my-server")?;
//! while let Ok(event) = events.recv().await {
//!     println!("{event:?}");
//! }
//! # Ok(())
//! # }
//! ```

pub mod config;
pub mod discovery;
pub mod forwarder;
pub mod ssh_session;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Hard ceiling on auto-created rules per host. Protects against pathological
/// environments (hundreds of short-lived test servers) and feedback loops.
const MAX_AUTO_FORWARDED_PORTS: usize = 128;

pub use config::{
    AuthKind, HostConfig, HostId, HostSource, PortSpec, RuleConfig, RuleId, Settings,
    ThemePreference,
};
pub use discovery::{DiscoveryFilter, ListeningPort};
pub use forwarder::{ForwardRule, RuleState, RuleView};
pub use ssh_session::{SessionConfig, SessionState, SshSession};

/// Severity for [`CoreEvent::Log`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// Events emitted by the engine. Consumers should treat them as hints: the
/// authoritative state is always available through the snapshot methods on
/// [`Porty`].
#[derive(Clone, Debug)]
pub enum CoreEvent {
    HostStateChanged {
        host_id: HostId,
        state: SessionState,
        detail: Option<String>,
    },
    HostMetrics {
        host_id: HostId,
        rtt_ms: Option<u32>,
    },
    RemotePortDiscovered {
        host_id: HostId,
        port: ListeningPort,
    },
    RemotePortClosed {
        host_id: HostId,
        port: u16,
    },
    RuleUpdated(RuleView),
    RuleRemoved {
        id: RuleId,
    },
    HostsChanged,
    Log {
        level: LogLevel,
        message: String,
    },
}

/// A host plus its live connection state, as consumed by the UI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostView {
    pub config: HostConfig,
    pub state: SessionState,
    pub rtt_ms: Option<u32>,
}

/// The Porty engine.
///
/// All methods are cheap, synchronous mutations of in-memory state; the actual
/// I/O happens on the provided Tokio runtime. This keeps GUI calls instant and
/// makes the service easy to drive from a CLI or tests.
pub struct Porty {
    rt: tokio::runtime::Handle,
    settings_path: PathBuf,
    settings: RwLock<Settings>,
    sessions: DashMap<HostId, Arc<SshSession>>,
    discovery: DashMap<HostId, CancellationToken>,
    /// Last successful discovery snapshot per host, used when the user enables
    /// auto-forward after ports have already been discovered.
    discovered: DashMap<HostId, Vec<ListeningPort>>,
    rules: DashMap<RuleId, Arc<ForwardRule>>,
    events: broadcast::Sender<CoreEvent>,
    next_rule_id: AtomicU64,
    /// Serialises auto-forward decisions so two concurrent discovery paths
    /// cannot create duplicate rules for the same port.
    auto_forward_lock: parking_lot::Mutex<()>,
}

impl Porty {
    /// Load settings from `settings_path`, import `~/.ssh/config` and start an
    /// empty service. Call [`Porty::connect`] to establish sessions.
    pub fn open(rt: tokio::runtime::Handle, settings_path: PathBuf) -> Result<Arc<Self>> {
        let mut settings = match Settings::load(&settings_path) {
            Ok(settings) => settings,
            Err(err) => {
                warn!("could not read settings ({}), starting fresh", err);
                Settings::default()
            }
        };
        if let Some(ssh_config) = config::ssh_config_path() {
            match config::merge_ssh_hosts(&mut settings, &ssh_config) {
                Ok(true) => {
                    if let Err(err) = settings.save(&settings_path) {
                        warn!("could not persist imported hosts: {err}");
                    }
                }
                Ok(false) => {}
                Err(err) => warn!("could not import {}: {err}", ssh_config.display()),
            }
        }
        Ok(Self::from_settings(rt, settings_path, settings))
    }

    /// Build the service from an explicit [`Settings`] value. Useful for tests,
    /// headless daemons and embedding.
    pub fn from_settings(
        rt: tokio::runtime::Handle,
        settings_path: PathBuf,
        settings: Settings,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        let next_rule_id = settings
            .rules
            .iter()
            .map(|rule| rule.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);

        let service = Arc::new(Self {
            rt,
            settings_path,
            settings: RwLock::new(settings),
            sessions: DashMap::new(),
            discovery: DashMap::new(),
            discovered: DashMap::new(),
            rules: DashMap::new(),
            events,
            next_rule_id: AtomicU64::new(next_rule_id),
            auto_forward_lock: parking_lot::Mutex::new(()),
        });

        for rule in service.settings.read().rules.clone() {
            service
                .rules
                .insert(rule.id, Arc::new(ForwardRule::from_config(&rule)));
        }

        service
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.events.subscribe()
    }

    pub fn settings_path(&self) -> &Path {
        &self.settings_path
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().clone()
    }

    pub fn theme_preference(&self) -> ThemePreference {
        self.settings.read().theme
    }

    pub fn set_theme_preference(&self, theme: ThemePreference) {
        self.settings.write().theme = theme;
        self.persist();
    }

    /// Snapshot of all hosts with connection state.
    pub fn hosts(&self) -> Vec<HostView> {
        let settings = self.settings.read();
        settings
            .hosts
            .iter()
            .map(|host| {
                let session = self.sessions.get(&host.id).map(|entry| entry.clone());
                let state = session
                    .as_ref()
                    .map(|session| session.state())
                    .unwrap_or_default();
                let rtt_ms = session.as_ref().and_then(|session| session.rtt_ms());
                HostView {
                    config: host.clone(),
                    state,
                    rtt_ms,
                }
            })
            .collect()
    }

    pub fn host_config(&self, host_id: &str) -> Option<HostConfig> {
        self.settings.read().host(host_id).cloned()
    }

    pub fn session_state(&self, host_id: &str) -> SessionState {
        self.sessions
            .get(host_id)
            .map(|session| session.state())
            .unwrap_or_default()
    }

    /// Snapshot of all rules, sorted by id for stable rendering.
    pub fn rules(&self) -> Vec<RuleView> {
        let mut rules: Vec<RuleView> = self.rules.iter().map(|rule| rule.view()).collect();
        rules.sort_by_key(|rule| rule.id);
        rules
    }

    pub fn rule(&self, rule_id: RuleId) -> Option<RuleView> {
        self.rules.get(&rule_id).map(|rule| rule.view())
    }

    pub fn rule_ids_for_host(&self, host_id: &str) -> Vec<RuleId> {
        let mut ids: Vec<RuleId> = self
            .rules
            .iter()
            .filter(|rule| rule.host_id == host_id)
            .map(|rule| rule.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    // ---------------------------------------------------------------- hosts

    /// (Re)import `~/.ssh/config`. Returns the number of hosts known afterwards.
    pub fn import_ssh_config(&self) -> Result<usize> {
        let Some(path) = config::ssh_config_path() else {
            bail!("could not locate the home directory");
        };
        let mut settings = self.settings.write();
        config::merge_ssh_hosts(&mut settings, &path)?;
        let count = settings.hosts.len();
        drop(settings);
        self.persist();
        let _ = self.events.send(CoreEvent::HostsChanged);
        Ok(count)
    }

    pub fn add_manual_host(
        &self,
        alias: &str,
        host_name: &str,
        port: u16,
        user: Option<String>,
    ) -> Result<HostId> {
        let alias = alias.trim();
        let host_name = host_name.trim();
        if alias.is_empty() {
            bail!("host name is required");
        }
        if host_name.is_empty() {
            bail!("address is required");
        }
        if port == 0 {
            bail!("port must be between 1 and 65535");
        }
        {
            let mut settings = self.settings.write();
            if settings.host(alias).is_some() {
                bail!("a host named `{alias}` already exists");
            }
            let mut host = HostConfig::manual(alias, host_name, port, user);
            host.auto_discover = true;
            settings.hosts.push(host);
            settings.hosts.sort_by_key(|host| host.alias.to_lowercase());
        }
        self.persist();
        let _ = self.events.send(CoreEvent::HostsChanged);
        Ok(alias.to_string())
    }

    pub fn remove_host(&self, host_id: &str) -> Result<()> {
        {
            let mut settings = self.settings.write();
            let before = settings.hosts.len();
            settings.hosts.retain(|host| host.id != host_id);
            if settings.hosts.len() == before {
                bail!("unknown host `{host_id}`");
            }
            let removed_rules: Vec<RuleId> = settings
                .rules
                .iter()
                .filter(|rule| rule.host_id == host_id)
                .map(|rule| rule.id)
                .collect();
            settings.rules.retain(|rule| rule.host_id != host_id);
            for rule_id in removed_rules {
                if let Some((_, rule)) = self.rules.remove(&rule_id) {
                    rule.cancel_current();
                }
                let _ = self.events.send(CoreEvent::RuleRemoved { id: rule_id });
            }
        }
        self.disconnect(host_id);
        self.discovered.remove(host_id);
        self.persist();
        let _ = self.events.send(CoreEvent::HostsChanged);
        Ok(())
    }

    pub fn set_host_preferences(
        self: &Arc<Self>,
        host_id: &str,
        auto_discover: Option<bool>,
        auto_forward: Option<bool>,
        bind_all: Option<bool>,
    ) -> Result<()> {
        let auto_discover_now = {
            let mut settings = self.settings.write();
            let host = settings
                .host_mut(host_id)
                .ok_or_else(|| anyhow!("unknown host `{host_id}`"))?;
            if let Some(value) = auto_discover {
                host.auto_discover = value;
            }
            if let Some(value) = auto_forward {
                host.auto_forward = value;
            }
            if let Some(value) = bind_all {
                host.bind_all = value;
            }
            host.auto_discover
        };
        self.persist();
        let _ = self.events.send(CoreEvent::HostsChanged);

        if auto_discover == Some(true) && auto_discover_now {
            if let Some(session) = self.sessions.get(host_id).map(|entry| entry.clone()) {
                self.start_discovery(host_id.to_string(), session);
            }
        } else if auto_discover == Some(false) {
            if let Some((_, token)) = self.discovery.remove(host_id) {
                token.cancel();
            }
        }

        // Turning auto-forward on should cover ports that were discovered
        // before the toggle flipped, not only the next diff.
        if auto_forward == Some(true) {
            let ports = self
                .discovered
                .get(host_id)
                .map(|entry| entry.clone())
                .unwrap_or_default();
            for port in ports {
                self.auto_forward_discovered(host_id, port);
            }
        }
        Ok(())
    }

    pub fn set_password(&self, host_id: &str, password: Option<String>) -> Result<()> {
        let mut settings = self.settings.write();
        let host = settings
            .host_mut(host_id)
            .ok_or_else(|| anyhow!("unknown host `{host_id}`"))?;
        host.password = password.filter(|password| !password.is_empty());
        Ok(())
    }

    pub fn set_auth(
        &self,
        host_id: &str,
        auth: AuthKind,
        identity_file: Option<PathBuf>,
    ) -> Result<()> {
        {
            let mut settings = self.settings.write();
            let host = settings
                .host_mut(host_id)
                .ok_or_else(|| anyhow!("unknown host `{host_id}`"))?;
            host.auth = auth;
            if identity_file.is_some() {
                host.identity_file = identity_file;
            }
        }
        self.persist();
        let _ = self.events.send(CoreEvent::HostsChanged);
        Ok(())
    }

    // ----------------------------------------------------------- connection

    /// Connect to a host. Idempotent while a connection is already in progress
    /// or established.
    pub fn connect(self: &Arc<Self>, host_id: &str) -> Result<()> {
        let host = self
            .host_config(host_id)
            .ok_or_else(|| anyhow!("unknown host `{host_id}`"))?;

        if let Some(existing) = self.sessions.get(host_id).map(|entry| entry.clone()) {
            if !matches!(
                existing.state(),
                SessionState::Disconnected | SessionState::Error { .. }
            ) {
                return Ok(());
            }
            existing.stop();
            self.sessions.remove(host_id);
        }

        let config = SessionConfig::from_host(&host);
        let session = Arc::new(SshSession::new(config, self.events.clone()));
        self.sessions.insert(host_id.to_string(), session.clone());
        session.start(&self.rt);
        self.spawn_session_watcher(host_id.to_string(), session.clone());
        if host.auto_discover {
            self.start_discovery(host_id.to_string(), session);
        }
        let _ = self.events.send(CoreEvent::HostsChanged);
        info!(host = host_id, "connection requested");
        Ok(())
    }

    pub fn disconnect(&self, host_id: &str) {
        if let Some((_, session)) = self.sessions.remove(host_id) {
            session.stop();
        }
        if let Some((_, token)) = self.discovery.remove(host_id) {
            token.cancel();
        }
        for rule_id in self.rule_ids_for_host(host_id) {
            self.stop(rule_id);
        }
        let _ = self.events.send(CoreEvent::HostsChanged);
    }

    /// Restart a host's transport (useful after editing credentials).
    pub fn reconnect(self: &Arc<Self>, host_id: &str) -> Result<()> {
        self.disconnect(host_id);
        self.connect(host_id)
    }

    fn spawn_session_watcher(self: &Arc<Self>, host_id: HostId, session: Arc<SshSession>) {
        let this = self.clone();
        let mut states = session.subscribe();
        self.rt.spawn(async move {
            loop {
                let state = states.borrow_and_update().clone();
                if state.is_connected() {
                    let ids: Vec<RuleId> = this
                        .rules
                        .iter()
                        .filter(|rule| {
                            rule.host_id == host_id
                                && rule.auto_start()
                                && matches!(rule.state(), RuleState::Stopped)
                        })
                        .map(|rule| rule.id)
                        .collect();
                    for rule_id in ids {
                        if let Err(err) = this.start(rule_id) {
                            warn!(rule = rule_id, "auto-start failed: {err:#}");
                        }
                    }
                }
                if matches!(state, SessionState::Disconnected) {
                    break;
                }
                if states.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    // -------------------------------------------------------------- rules

    /// Parse a quick-add expression and create a rule. The rule is started
    /// immediately when the host is connected.
    pub fn add_rule_from_spec(
        self: &Arc<Self>,
        host_id: &str,
        spec: &str,
        label: Option<String>,
    ) -> Result<RuleId> {
        let spec = config::parse_port_spec(spec)?;
        self.add_rule(host_id, spec, label)
    }

    pub fn add_rule(
        self: &Arc<Self>,
        host_id: &str,
        spec: PortSpec,
        label: Option<String>,
    ) -> Result<RuleId> {
        self.add_rule_internal(host_id, spec, label, None, false, true)
    }

    fn add_rule_internal(
        self: &Arc<Self>,
        host_id: &str,
        spec: PortSpec,
        label: Option<String>,
        process_name: Option<String>,
        auto_discovered: bool,
        auto_start: bool,
    ) -> Result<RuleId> {
        let bind_all = {
            let settings = self.settings.read();
            let host = settings
                .host(host_id)
                .ok_or_else(|| anyhow!("unknown host `{host_id}`"))?;
            host.bind_all
        };

        let id = self.next_rule_id.fetch_add(1, Ordering::Relaxed);
        let config = RuleConfig {
            id,
            host_id: host_id.to_string(),
            remote_host: spec.remote_host,
            remote_port: spec.remote_port,
            requested_local_port: spec.local_port,
            bind_all,
            label: label.filter(|value| !value.trim().is_empty()),
            process_name,
            auto_discovered,
            auto_start,
        };
        let rule = Arc::new(ForwardRule::from_config(&config));
        self.rules.insert(id, rule.clone());
        {
            let mut settings = self.settings.write();
            settings.rules.push(config);
        }
        self.persist();
        self.emit_rule(&rule);

        if auto_start && self.session_state(host_id).is_connected() {
            if let Err(err) = self.start(id) {
                warn!(rule = id, "could not start new rule: {err:#}");
            }
        }
        Ok(id)
    }

    /// Start a rule's local listener. Requires a connected host.
    pub fn start(self: &Arc<Self>, rule_id: RuleId) -> Result<()> {
        let rule = self
            .rules
            .get(&rule_id)
            .map(|entry| entry.clone())
            .ok_or_else(|| anyhow!("unknown forwarding rule {rule_id}"))?;
        let session = self
            .sessions
            .get(&rule.host_id)
            .map(|entry| entry.clone())
            .ok_or_else(|| anyhow!("connect to `{}` before forwarding", rule.host_id))?;
        if !session.state().is_connected() {
            bail!("`{}` is not connected yet", rule.host_id);
        }
        if !rule.try_begin_start() {
            return Ok(());
        }

        let cancel = rule.new_cancel_token();
        self.emit_rule(&rule);
        let events = self.events.clone();
        self.rt
            .spawn(forwarder::run_listener(rule, session, cancel, events));
        Ok(())
    }

    pub fn stop(&self, rule_id: RuleId) {
        if let Some(rule) = self.rules.get(&rule_id) {
            rule.cancel_current();
            rule.set_state(RuleState::Stopped);
            self.emit_rule(&rule);
        }
    }

    pub fn remove_rule(&self, rule_id: RuleId) {
        if let Some((_, rule)) = self.rules.remove(&rule_id) {
            rule.cancel_current();
            {
                let mut settings = self.settings.write();
                settings.rules.retain(|rule| rule.id != rule_id);
            }
            self.persist();
            let _ = self.events.send(CoreEvent::RuleRemoved { id: rule_id });
        }
    }

    pub fn set_rule_bind_all(self: &Arc<Self>, rule_id: RuleId, bind_all: bool) -> Result<()> {
        let rule = self
            .rules
            .get(&rule_id)
            .map(|entry| entry.clone())
            .ok_or_else(|| anyhow!("unknown forwarding rule {rule_id}"))?;
        if rule.bind_all() == bind_all {
            return Ok(());
        }
        let was_active = rule.state().is_active();
        rule.set_bind_all(bind_all);
        {
            let mut settings = self.settings.write();
            if let Some(config) = settings.rules.iter_mut().find(|rule| rule.id == rule_id) {
                config.bind_all = bind_all;
            }
        }
        self.persist();
        self.emit_rule(&rule);
        if was_active {
            self.restart_rule(rule_id);
        }
        Ok(())
    }

    pub fn set_rule_label(&self, rule_id: RuleId, label: Option<String>) -> Result<()> {
        let rule = self
            .rules
            .get(&rule_id)
            .ok_or_else(|| anyhow!("unknown forwarding rule {rule_id}"))?;
        let label = label.filter(|value| !value.trim().is_empty());
        rule.set_label(label.clone());
        {
            let mut settings = self.settings.write();
            if let Some(config) = settings.rules.iter_mut().find(|rule| rule.id == rule_id) {
                config.label = label;
            }
        }
        self.persist();
        self.emit_rule(&rule);
        Ok(())
    }

    pub fn set_rule_auto_start(&self, rule_id: RuleId, auto_start: bool) -> Result<()> {
        let rule = self
            .rules
            .get(&rule_id)
            .ok_or_else(|| anyhow!("unknown forwarding rule {rule_id}"))?;
        rule.set_auto_start(auto_start);
        {
            let mut settings = self.settings.write();
            if let Some(config) = settings.rules.iter_mut().find(|rule| rule.id == rule_id) {
                config.auto_start = auto_start;
            }
        }
        self.persist();
        self.emit_rule(&rule);
        Ok(())
    }

    fn restart_rule(self: &Arc<Self>, rule_id: RuleId) {
        if let Some(rule) = self.rules.get(&rule_id) {
            rule.cancel_current();
            rule.set_state(RuleState::Stopped);
            self.emit_rule(&rule);
        }
        let this = self.clone();
        self.rt.spawn(async move {
            // Wait for the old listener task to drop its socket so a restart
            // keeps the same local port instead of bumping.
            for _ in 0..40 {
                let still_bound = this
                    .rules
                    .get(&rule_id)
                    .map(|rule| rule.local_port().is_some())
                    .unwrap_or(false);
                if !still_bound {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if let Err(err) = this.start(rule_id) {
                warn!(rule = rule_id, "restart failed: {err:#}");
            }
        });
    }

    // ---------------------------------------------------------- discovery

    fn start_discovery(self: &Arc<Self>, host_id: HostId, session: Arc<SshSession>) {
        if self.discovery.contains_key(&host_id) {
            return;
        }
        let token = CancellationToken::new();
        self.discovery.insert(host_id.clone(), token.clone());

        let (interval, privileged_ports, ssh_port) = {
            let settings = self.settings.read();
            (
                Duration::from_secs(settings.discovery_interval_secs.max(1)),
                settings.privileged_ports.clone(),
                settings.host(&host_id).map(|host| host.port).unwrap_or(22),
            )
        };
        let filter = DiscoveryFilter {
            interval,
            privileged_ports,
            // Never offer to forward the SSH daemon itself.
            ignore_ports: vec![22, ssh_port],
            ..DiscoveryFilter::default()
        };

        let events = self.events.clone();
        let weak = Arc::downgrade(self);
        let host_for_callback = host_id.clone();
        let on_discovered = Arc::new(move |port: ListeningPort| {
            if let Some(service) = weak.upgrade() {
                service.auto_forward_discovered(&host_for_callback, port);
            }
        });

        let weak_closed = Arc::downgrade(self);
        let host_for_closed = host_id.clone();
        let on_closed = Arc::new(move |port: u16| {
            if let Some(service) = weak_closed.upgrade() {
                service.auto_close_discovered(&host_for_closed, port);
            }
        });

        let weak_snapshot = Arc::downgrade(self);
        let host_for_snapshot = host_id.clone();
        let on_snapshot = Arc::new(move |ports: Vec<ListeningPort>| {
            if let Some(service) = weak_snapshot.upgrade() {
                service.discovered.insert(host_for_snapshot.clone(), ports);
            }
        });

        let this = self.clone();
        let host_for_cleanup = host_id.clone();
        self.rt.spawn(async move {
            discovery::run(
                session,
                host_id,
                filter,
                events,
                token,
                discovery::DiscoveryCallbacks {
                    on_discovered,
                    on_closed,
                    on_snapshot,
                },
            )
            .await;
            // Allow a later connect to start a fresh discovery loop.
            this.discovery.remove(&host_for_cleanup);
        });
    }

    /// Auto-forward a newly discovered port when the host opts in.
    fn auto_forward_discovered(self: &Arc<Self>, host_id: &str, port: ListeningPort) {
        let _guard = self.auto_forward_lock.lock();

        let auto_forward = {
            let settings = self.settings.read();
            match settings.host(host_id) {
                Some(host) => host.auto_forward,
                None => return,
            }
        };
        if !auto_forward {
            return;
        }

        let remote_host = port.forward_host();

        // Feedback loop guard: if the SSH target is this machine (or runs
        // Porty itself), discovery sees our own listeners. Never forward
        // a port we are already listening on.
        if self
            .rules
            .iter()
            .any(|rule| rule.local_port() == Some(port.port))
        {
            debug!(
                host = host_id,
                port = port.port,
                "skipping discovered port that is one of our local listeners"
            );
            return;
        }

        // Known endpoint? Restart the existing auto rule rather than creating
        // a duplicate with a new id and a new local port. A manual rule for the
        // same endpoint also suppresses auto-forwarding.
        let mut any_existing = false;
        let mut auto_rule = None;
        for rule in self.rules.iter().filter(|rule| {
            rule.host_id == host_id
                && rule.remote_port == port.port
                && rule.remote_host == remote_host
        }) {
            any_existing = true;
            if rule.auto_discovered {
                auto_rule = Some((rule.id, rule.state().is_active()));
                break;
            }
        }
        if any_existing {
            if let Some((rule_id, active)) = auto_rule {
                if !active && self.session_state(host_id).is_connected() {
                    info!(
                        host = host_id,
                        port = port.port,
                        rule = rule_id,
                        "remote port returned, restarting forward"
                    );
                    if let Err(err) = self.start(rule_id) {
                        warn!(rule = rule_id, "could not restart forward: {err:#}");
                    }
                }
            }
            return;
        }

        let auto_count = self
            .rules
            .iter()
            .filter(|rule| rule.host_id == host_id && rule.auto_discovered)
            .count();
        if auto_count >= MAX_AUTO_FORWARDED_PORTS {
            warn!(
                host = host_id,
                "auto-forward limit ({MAX_AUTO_FORWARDED_PORTS}) reached; ignoring port {}",
                port.port
            );
            return;
        }

        let spec = PortSpec {
            remote_host,
            remote_port: port.port,
            local_port: port.port,
        };
        let label = port.summary();
        match self.add_rule_internal(
            host_id,
            spec,
            Some(label),
            port.process_name.clone(),
            true,
            // Discovery owns the lifecycle: auto-start is handled here, so a
            // stale persisted rule is not resurrected on the next launch.
            false,
        ) {
            Ok(rule_id) => {
                info!(
                    host = host_id,
                    port = port.port,
                    rule = rule_id,
                    "auto-forwarding discovered port"
                );
                if let Err(err) = self.start(rule_id) {
                    warn!(rule = rule_id, "could not start auto-forward: {err:#}");
                }
            }
            Err(err) => warn!(
                host = host_id,
                port = port.port,
                "auto-forward failed: {err:#}"
            ),
        }
    }

    /// Stop auto-created rules when their remote port disappears.
    fn auto_close_discovered(&self, host_id: &str, port: u16) {
        let rule_ids: Vec<RuleId> = self
            .rules
            .iter()
            .filter(|rule| {
                rule.host_id == host_id
                    && rule.auto_discovered
                    && rule.remote_port == port
                    && rule.state().is_active()
            })
            .map(|rule| rule.id)
            .collect();
        for rule_id in rule_ids {
            info!(
                host = host_id,
                port,
                rule = rule_id,
                "remote port closed, stopping forward"
            );
            self.stop(rule_id);
        }
    }

    // ----------------------------------------------------------- internals

    fn persist(&self) {
        let snapshot = self.settings.read().clone();
        if let Err(err) = snapshot.save(&self.settings_path) {
            warn!("could not save settings: {err:#}");
        }
    }

    fn emit_rule(&self, rule: &ForwardRule) {
        let _ = self.events.send(CoreEvent::RuleUpdated(rule.view()));
    }

    /// Cancel every task. The service can still be dropped safely afterwards.
    pub fn shutdown(&self) {
        for entry in self.discovery.iter() {
            entry.value().cancel();
        }
        self.discovery.clear();
        for entry in self.sessions.iter() {
            entry.value().stop();
        }
        self.sessions.clear();
        for entry in self.rules.iter() {
            entry.value().cancel_current();
        }
    }
}

impl Drop for Porty {
    fn drop(&mut self) {
        for entry in self.discovery.iter() {
            entry.value().cancel();
        }
        for entry in self.sessions.iter() {
            entry.value().stop();
        }
        for entry in self.rules.iter() {
            entry.value().cancel_current();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "porty-{name}-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn rules_persist_and_reload() {
        let path = test_path("rules");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut settings = Settings::default();
        settings
            .hosts
            .push(HostConfig::manual("box", "example.com", 22, None));

        let service = Porty::from_settings(runtime.handle().clone(), path.clone(), settings);
        let rule_id = service
            .add_rule(
                "box",
                PortSpec {
                    remote_host: "127.0.0.1".into(),
                    remote_port: 5432,
                    local_port: 15432,
                },
                Some("postgres".into()),
            )
            .unwrap();
        assert_eq!(rule_id, 1);
        assert_eq!(service.rules().len(), 1);
        assert_eq!(service.rules()[0].requested_local_port, 15432);
        assert_eq!(service.rules()[0].state, RuleState::Stopped);

        let reloaded = Settings::load(&path).unwrap();
        assert_eq!(reloaded.rules.len(), 1);
        assert_eq!(reloaded.rules[0].requested_local_port, 15432);
        assert_eq!(reloaded.rules[0].label.as_deref(), Some("postgres"));

        service.remove_rule(rule_id);
        assert!(service.rules().is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn enabling_auto_forward_uses_discovery_snapshot() {
        let path = test_path("snapshot");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut settings = Settings::default();
        let mut host = HostConfig::manual("box", "example.com", 22, None);
        host.auto_forward = false;
        settings.hosts.push(host);

        let service = Porty::from_settings(runtime.handle().clone(), path.clone(), settings);
        service.discovered.insert(
            "box".to_string(),
            vec![ListeningPort {
                port: 8080,
                bind_addr: "127.0.0.1".to_string(),
                process_name: Some("node".to_string()),
                pid: Some(42),
                command_line: None,
            }],
        );

        service
            .set_host_preferences("box", None, Some(true), None)
            .unwrap();
        let rules = service.rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].remote_port, 8080);
        assert!(rules[0].auto_discovered);
        // The host is not connected, so the rule exists but stays stopped.
        assert_eq!(rules[0].state, RuleState::Stopped);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn connect_from_a_non_runtime_thread_does_not_panic() {
        let path = test_path("non-runtime-connect");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let mut settings = Settings::default();
        settings
            .hosts
            .push(HostConfig::manual("unreachable", "127.0.0.1", 1, None));

        let service = Porty::from_settings(runtime.handle().clone(), path.clone(), settings);

        // This test thread has no Tokio runtime context. This is how the GPUI
        // event loop calls connect(); a bare `tokio::spawn` here would panic.
        service.connect("unreachable").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        service.disconnect("unreachable");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_unknown_hosts() {
        let path = test_path("hosts");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let service = Porty::from_settings(runtime.handle().clone(), path, Settings::default());
        assert!(service.add_rule_from_spec("nope", "3000", None).is_err());
        assert!(service.connect("nope").is_err());
    }
}
