//! Local TCP listeners bridged into SSH `direct-tcpip` channels.
//!
//! Each [`ForwardRule`] owns its listener task and live traffic counters. A
//! connection is never tied to the transport instance: if the SSH session
//! reconnects, the listener stays up and new connections use the new handle.

use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::{HostId, RuleConfig, RuleId};
use crate::ssh_session::SshSession;
use crate::CoreEvent;

/// Lifecycle of a forwarding rule.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuleState {
    #[default]
    Stopped,
    Starting,
    Active,
    Error {
        message: String,
    },
}

impl RuleState {
    pub fn is_active(&self) -> bool {
        matches!(self, RuleState::Active | RuleState::Starting)
    }

    pub fn label(&self) -> &'static str {
        match self {
            RuleState::Stopped => "Stopped",
            RuleState::Starting => "Starting",
            RuleState::Active => "Active",
            RuleState::Error { .. } => "Error",
        }
    }
}

/// Live counters for one rule. Cheap to read from the UI thread.
#[derive(Debug, Default)]
pub struct RuleMetrics {
    pub active_connections: AtomicUsize,
    pub total_connections: AtomicU64,
    /// Bytes sent from the local client to the remote server.
    pub bytes_tx: AtomicU64,
    /// Bytes received from the remote server.
    pub bytes_rx: AtomicU64,
}

impl RuleMetrics {
    pub fn snapshot(&self) -> (usize, u64, u64, u64) {
        (
            self.active_connections.load(Ordering::Relaxed),
            self.total_connections.load(Ordering::Relaxed),
            self.bytes_tx.load(Ordering::Relaxed),
            self.bytes_rx.load(Ordering::Relaxed),
        )
    }

    fn connection_started(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
        self.total_connections.fetch_add(1, Ordering::Relaxed);
    }

    fn connection_finished(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }
}

struct ConnectionGuard<'a>(&'a RuleMetrics);

impl Drop for ConnectionGuard<'_> {
    fn drop(&mut self) {
        self.0.connection_finished();
    }
}

/// A forwarding rule and its runtime state.
pub struct ForwardRule {
    pub id: RuleId,
    pub host_id: HostId,
    pub remote_host: String,
    pub remote_port: u16,
    pub requested_local_port: u16,
    pub auto_discovered: bool,
    bind_all: AtomicBool,
    auto_start: AtomicBool,
    label: Mutex<Option<String>>,
    process_name: Mutex<Option<String>>,
    state: Mutex<RuleState>,
    local_port: AtomicU16,
    bumped: AtomicBool,
    pub metrics: RuleMetrics,
    cancel: Mutex<CancellationToken>,
}

impl ForwardRule {
    pub fn from_config(config: &RuleConfig) -> Self {
        Self {
            id: config.id,
            host_id: config.host_id.clone(),
            remote_host: config.remote_host.clone(),
            remote_port: config.remote_port,
            requested_local_port: config.requested_local_port,
            auto_discovered: config.auto_discovered,
            bind_all: AtomicBool::new(config.bind_all),
            auto_start: AtomicBool::new(config.auto_start),
            label: Mutex::new(config.label.clone()),
            process_name: Mutex::new(config.process_name.clone()),
            state: Mutex::new(RuleState::Stopped),
            local_port: AtomicU16::new(0),
            bumped: AtomicBool::new(false),
            metrics: RuleMetrics::default(),
            cancel: Mutex::new(CancellationToken::new()),
        }
    }

    pub fn state(&self) -> RuleState {
        self.state.lock().clone()
    }

    /// Atomically move from `Stopped`/`Error` to `Starting`.
    ///
    /// Returns `false` when a listener is already starting or active, which
    /// makes concurrent `start` calls (discovery, reconnect watcher, UI)
    /// idempotent without double-binding the local port.
    pub fn try_begin_start(&self) -> bool {
        let mut state = self.state.lock();
        if matches!(*state, RuleState::Starting | RuleState::Active) {
            return false;
        }
        *state = RuleState::Starting;
        true
    }

    pub fn set_state(&self, state: RuleState) {
        *self.state.lock() = state;
    }

    pub fn bind_all(&self) -> bool {
        self.bind_all.load(Ordering::Relaxed)
    }

    pub fn set_bind_all(&self, bind_all: bool) {
        self.bind_all.store(bind_all, Ordering::Relaxed);
    }

    pub fn auto_start(&self) -> bool {
        self.auto_start.load(Ordering::Relaxed)
    }

    pub fn set_auto_start(&self, auto_start: bool) {
        self.auto_start.store(auto_start, Ordering::Relaxed);
    }

    pub fn label(&self) -> Option<String> {
        self.label.lock().clone()
    }

    pub fn set_label(&self, value: Option<String>) {
        *self.label.lock() = value;
    }

    pub fn process_name(&self) -> Option<String> {
        self.process_name.lock().clone()
    }

    pub fn local_port(&self) -> Option<u16> {
        match self.local_port.load(Ordering::Relaxed) {
            0 => None,
            port => Some(port),
        }
    }

    fn set_local_port(&self, port: u16, bumped: bool) {
        self.local_port.store(port, Ordering::Relaxed);
        self.bumped.store(bumped, Ordering::Relaxed);
    }

    pub fn bumped(&self) -> bool {
        self.bumped.load(Ordering::Relaxed)
    }

    /// Replace the cancellation token, invalidating any previous listener task.
    pub fn new_cancel_token(&self) -> CancellationToken {
        let token = CancellationToken::new();
        *self.cancel.lock() = token.clone();
        token
    }

    pub fn cancel_current(&self) {
        self.cancel.lock().cancel();
    }

    /// The effective display name: user label, process name or the port.
    pub fn display_name(&self) -> String {
        if let Some(label) = self.label() {
            if !label.trim().is_empty() {
                return label;
            }
        }
        if let Some(process) = self.process_name() {
            if !process.trim().is_empty() {
                return process;
            }
        }
        format!("port {}", self.remote_port)
    }

    pub fn view(&self) -> RuleView {
        let (active_connections, total_connections, bytes_tx, bytes_rx) = self.metrics.snapshot();
        RuleView {
            id: self.id,
            host_id: self.host_id.clone(),
            remote_host: self.remote_host.clone(),
            remote_port: self.remote_port,
            requested_local_port: self.requested_local_port,
            local_port: self.local_port(),
            bumped: self.bumped(),
            bind_all: self.bind_all(),
            label: self.label(),
            process_name: self.process_name(),
            auto_discovered: self.auto_discovered,
            auto_start: self.auto_start(),
            state: self.state(),
            active_connections,
            total_connections,
            bytes_tx,
            bytes_rx,
        }
    }

    /// Snapshot suitable for persistence.
    pub fn config(&self) -> RuleConfig {
        RuleConfig {
            id: self.id,
            host_id: self.host_id.clone(),
            remote_host: self.remote_host.clone(),
            remote_port: self.remote_port,
            requested_local_port: self.requested_local_port,
            bind_all: self.bind_all(),
            label: self.label(),
            process_name: self.process_name(),
            auto_discovered: self.auto_discovered,
            auto_start: self.auto_start(),
        }
    }
}

/// UI-facing snapshot of a forwarding rule.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuleView {
    pub id: RuleId,
    pub host_id: HostId,
    pub remote_host: String,
    pub remote_port: u16,
    pub requested_local_port: u16,
    pub local_port: Option<u16>,
    pub bumped: bool,
    pub bind_all: bool,
    pub label: Option<String>,
    pub process_name: Option<String>,
    pub auto_discovered: bool,
    pub auto_start: bool,
    pub state: RuleState,
    pub active_connections: usize,
    pub total_connections: u64,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
}

impl RuleView {
    pub fn display_name(&self) -> String {
        if let Some(label) = self.label.as_ref().filter(|label| !label.trim().is_empty()) {
            return label.clone();
        }
        if let Some(process) = self
            .process_name
            .as_ref()
            .filter(|process| !process.trim().is_empty())
        {
            return process.clone();
        }
        format!("port {}", self.remote_port)
    }

    /// e.g. `3000 -> 127.0.0.1:3000` or the bumped `3000 -> 127.0.0.1:3001`.
    pub fn mapping(&self) -> String {
        match self.local_port {
            Some(local) if self.remote_host == "127.0.0.1" => {
                format!("{} -> 127.0.0.1:{local}", self.remote_port)
            }
            Some(local) => format!(
                "{}:{} -> 127.0.0.1:{local}",
                self.remote_host, self.remote_port
            ),
            None => format!("{} -> not bound", self.remote_port),
        }
    }

    pub fn local_address(&self) -> Option<String> {
        self.local_port.map(|port| {
            if self.bind_all {
                format!("0.0.0.0:{port}")
            } else {
                format!("127.0.0.1:{port}")
            }
        })
    }

    pub fn browser_url(&self) -> Option<String> {
        self.local_port
            .map(|port| format!("http://127.0.0.1:{port}"))
    }

    pub fn total_bytes(&self) -> u64 {
        self.bytes_tx.saturating_add(self.bytes_rx)
    }
}

/// Bind the local listener, bumping to the next free port when `requested` is
/// taken (VS Code's behavior). Returns `(listener, actual_port, bumped)`.
pub async fn bind_listener(
    bind_all: bool,
    requested: u16,
) -> anyhow::Result<(TcpListener, u16, bool)> {
    let ip = if bind_all {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    };

    if requested == 0 {
        let listener = TcpListener::bind((ip, 0)).await?;
        let port = listener.local_addr()?.port();
        return Ok((listener, port, false));
    }

    let mut last_error = None;
    for offset in 0..=64u16 {
        let Some(port) = requested.checked_add(offset) else {
            break;
        };
        match TcpListener::bind((ip, port)).await {
            Ok(listener) => return Ok((listener, port, offset != 0)),
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                last_error = Some(err);
                continue;
            }
            Err(err) => return Err(err.into()),
        }
    }

    // Everything nearby is taken: fall back to an ephemeral port so the rule
    // still works instead of failing outright.
    let listener = TcpListener::bind((ip, 0)).await?;
    let port = listener.local_addr()?.port();
    warn!(
        requested,
        port, "all nearby local ports busy, using ephemeral port"
    );
    let _ = last_error;
    Ok((listener, port, true))
}

/// Run a rule until its cancellation token fires.
pub async fn run_listener(
    rule: Arc<ForwardRule>,
    session: Arc<SshSession>,
    cancel: CancellationToken,
    events: tokio::sync::broadcast::Sender<CoreEvent>,
) {
    rule.set_state(RuleState::Starting);
    emit_rule(&events, &rule);

    let (listener, port, bumped) =
        match bind_listener(rule.bind_all(), rule.requested_local_port).await {
            Ok(bound) => bound,
            Err(err) => {
                let message = format!("could not bind local port: {err:#}");
                warn!(rule = rule.id, "{message}");
                rule.set_state(RuleState::Error { message });
                emit_rule(&events, &rule);
                return;
            }
        };

    rule.set_local_port(port, bumped);
    rule.set_state(RuleState::Active);
    emit_rule(&events, &rule);
    info!(
        rule = rule.id,
        local = port,
        remote = %format!("{}:{}", rule.remote_host, rule.remote_port),
        bind_all = rule.bind_all(),
        "forwarding rule active"
    );

    let error = accept_loop(listener, &rule, &session, &cancel).await;

    rule.set_local_port(0, false);
    match error {
        Some(message) if !cancel.is_cancelled() => rule.set_state(RuleState::Error { message }),
        _ => rule.set_state(RuleState::Stopped),
    }
    emit_rule(&events, &rule);
}

async fn accept_loop(
    listener: TcpListener,
    rule: &Arc<ForwardRule>,
    session: &Arc<SshSession>,
    cancel: &CancellationToken,
) -> Option<String> {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return None,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        debug!(rule = rule.id, peer = %peer, "accepted local connection");
                        rule.metrics.connection_started();
                        tokio::spawn(handle_connection(
                            stream,
                            peer,
                            rule.clone(),
                            session.clone(),
                            cancel.clone(),
                        ));
                    }
                    Err(err) => {
                        // EMFILE and friends are transient; a short pause avoids
                        // spinning while the OS recovers.
                        debug!(rule = rule.id, "accept failed: {err}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        if !is_transient_accept_error(&err) {
                            return Some(format!("listener stopped: {err}"));
                        }
                    }
                }
            }
        }
    }
}

fn is_transient_accept_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::OutOfMemory
    )
}

async fn handle_connection(
    stream: TcpStream,
    peer: std::net::SocketAddr,
    rule: Arc<ForwardRule>,
    session: Arc<SshSession>,
    cancel: CancellationToken,
) {
    let _guard = ConnectionGuard(&rule.metrics);

    debug!(rule = rule.id, peer = %peer, remote = %format!("{}:{}", rule.remote_host, rule.remote_port), "opening direct-tcpip channel");
    let channel = match session
        .open_direct_tcpip(
            &rule.remote_host,
            rule.remote_port,
            &peer.ip().to_string(),
            peer.port(),
        )
        .await
    {
        Ok(channel) => channel,
        Err(err) => {
            debug!(
                rule = rule.id,
                "could not open direct-tcpip channel to {}:{}: {err:#}",
                rule.remote_host,
                rule.remote_port
            );
            return;
        }
    };

    debug!(rule = rule.id, peer = %peer, "direct-tcpip channel open");
    let remote = channel.into_stream();
    if let Err(err) = copy_bidirectional_counted(stream, remote, &rule.metrics, &cancel).await {
        debug!(
            rule = rule.id,
            "forwarded connection ended with error: {err}"
        );
    }
}

/// `tokio::io::copy_bidirectional` with live byte counters.
///
/// Counters are updated as the copy progresses, so the UI shows throughput
/// during long-lived transfers instead of waiting for the connection to close.
/// The local stream is wrapped: reading from it is upload traffic (`bytes_tx`),
/// writing to it is download traffic (`bytes_rx`).
async fn copy_bidirectional_counted<A, B>(
    local: A,
    mut remote: B,
    metrics: &RuleMetrics,
    cancel: &CancellationToken,
) -> io::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let mut local = CountingStream {
        inner: local,
        read_counter: &metrics.bytes_tx,
        write_counter: &metrics.bytes_rx,
    };

    tokio::select! {
        _ = cancel.cancelled() => Ok(()),
        result = tokio::io::copy_bidirectional(&mut local, &mut remote) => result.map(|_| ()),
    }
}

struct CountingStream<'a, S> {
    inner: S,
    /// Bytes read from the inner stream (local -> remote).
    read_counter: &'a AtomicU64,
    /// Bytes written to the inner stream (remote -> local).
    write_counter: &'a AtomicU64,
}

impl<S: AsyncRead + Unpin> AsyncRead for CountingStream<'_, S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            let read = buf.filled().len().saturating_sub(before);
            if read > 0 {
                this.read_counter.fetch_add(read as u64, Ordering::Relaxed);
            }
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CountingStream<'_, S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = &result {
            if *written > 0 {
                this.write_counter
                    .fetch_add(*written as u64, Ordering::Relaxed);
            }
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

fn emit_rule(events: &tokio::sync::broadcast::Sender<CoreEvent>, rule: &ForwardRule) {
    let _ = events.send(CoreEvent::RuleUpdated(rule.view()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bumps_when_requested_port_is_busy() {
        let occupied = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let busy_port = occupied.local_addr().unwrap().port();

        let (_listener, port, bumped) = bind_listener(false, busy_port).await.unwrap();
        assert_ne!(port, busy_port);
        assert!(bumped);
    }

    #[tokio::test]
    async fn binds_lan_interface() {
        let (listener, port, bumped) = bind_listener(true, 0).await.unwrap();
        assert_eq!(
            listener.local_addr().unwrap().ip(),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
        assert_ne!(port, 0);
        assert!(!bumped);
    }

    #[tokio::test]
    async fn counts_copied_bytes() {
        let metrics = Arc::new(RuleMetrics::default());
        let cancel = CancellationToken::new();
        let (client, server) = tokio::io::duplex(1024);
        let (other, echo) = tokio::io::duplex(1024);
        // Echo whatever arrives on `other` back to it is unnecessary; just
        // verify the pipe moves bytes and updates the counters.
        let task = tokio::spawn({
            let cancel = cancel.clone();
            let metrics = metrics.clone();
            async move { copy_bidirectional_counted(client, other, &metrics, &cancel).await }
        });
        use tokio::io::AsyncWriteExt;
        let mut server = server;
        let mut echo = echo;
        server.write_all(b"hello").await.unwrap();
        server.shutdown().await.unwrap();
        let mut buf = [0u8; 5];
        use tokio::io::AsyncReadExt;
        echo.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
        // Close the peer so the download half sees EOF and the copy joins.
        drop(echo);
        let _ = task.await.unwrap();
        assert!(metrics.bytes_tx.load(Ordering::Relaxed) >= 5);
        cancel.cancel();
    }
}
