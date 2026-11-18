//! Manual end-to-end smoke test for the headless engine.
//!
//! It expects a throwaway SSH server (see `scripts/e2e.sh`) reachable on
//! `PW_E2E_SSH_PORT` (default 2222), an ed25519 key at `PW_E2E_KEY`, and an
//! HTTP server listening on remote port 18080. The test then:
//!
//! 1. authenticates with the key,
//! 2. discovers listening ports through `ss` / `/proc/net/tcp`,
//! 3. auto-forwards port 18080,
//! 4. performs a real HTTP request through the SSH tunnel.
//!
//! Run with: `cargo run -p pw-core --example e2e`

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use pw_core::config::{AuthKind, HostConfig, Settings};
use pw_core::{CoreEvent, Porty, RuleState, SessionState};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("pw_core=debug,info")
        .init();

    let ssh_port: u16 = std::env::var("PW_E2E_SSH_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2222);
    let key = PathBuf::from(
        std::env::var("PW_E2E_KEY").unwrap_or_else(|_| "/tmp/pw-sshd/client_key".to_string()),
    );
    let remote_port: u16 = std::env::var("PW_E2E_REMOTE_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(18080);

    if !key.exists() {
        bail!(
            "client key {} does not exist — run scripts/e2e.sh first",
            key.display()
        );
    }

    let user = std::env::var("PW_E2E_USER").unwrap_or_else(|_| pw_core::config::current_username());
    let mut host = HostConfig::manual("e2e", "127.0.0.1", ssh_port, Some(user));
    host.auth = AuthKind::Key;
    host.identity_file = Some(key);
    host.identity_files.clear();
    host.auto_discover = true;
    host.auto_forward = true;

    let settings = Settings {
        discovery_interval_secs: 1,
        hosts: vec![host],
        ..Settings::default()
    };

    let settings_path = std::env::temp_dir().join("porty-e2e-settings.json");
    let service = Porty::from_settings(tokio::runtime::Handle::current(), settings_path, settings);
    let mut events = service.subscribe();
    service.connect("e2e")?;

    // 1. Authentication.
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let event = events.recv().await?;
            if let CoreEvent::HostStateChanged {
                state: SessionState::Connected,
                ..
            } = &event
            {
                return Ok::<_, tokio::sync::broadcast::error::RecvError>(());
            }
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for SSH authentication"))??;
    println!("PASS authentication (key)");

    // 2 + 3. Discovery and auto-forwarding of the remote HTTP port.
    let local_port = tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let event = events.recv().await?;
            match event {
                CoreEvent::RemotePortDiscovered { port, .. } if port.port == remote_port => {
                    println!("PASS discovery: {} -> {}", port.port, port.summary());
                }
                CoreEvent::RuleUpdated(view)
                    if view.remote_port == remote_port && view.state == RuleState::Active =>
                {
                    if let Some(local) = view.local_port {
                        println!(
                            "PASS auto-forward: remote {remote_port} -> local 127.0.0.1:{local} \
                             (requested {})",
                            view.requested_local_port
                        );
                        return Ok::<_, tokio::sync::broadcast::error::RecvError>(local);
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for discovery + auto-forward"))??;

    // 4. Real traffic through the multiplexed SSH session.
    let response = tokio::task::spawn_blocking(move || http_get(local_port))
        .await
        .context("joining HTTP task")??;
    let status_ok = response.starts_with("HTTP/1.0 200")
        || response.starts_with("HTTP/1.1 200")
        || response.starts_with("HTTP/1.1 200 OK");
    if !status_ok {
        bail!(
            "unexpected HTTP response through the tunnel: {}",
            response.lines().next().unwrap_or("<empty>")
        );
    }
    println!("PASS tunneled HTTP 200 through 127.0.0.1:{local_port}");

    tokio::time::sleep(Duration::from_millis(250)).await;
    if let Some(rule) = service
        .rules()
        .into_iter()
        .find(|rule| rule.remote_port == remote_port)
    {
        println!(
            "METRICS {} active · {} bytes up · {} bytes down",
            rule.active_connections, rule.bytes_tx, rule.bytes_rx
        );
        if rule.bytes_tx == 0 {
            bail!("forwarded traffic was not counted (bytes_tx == 0)");
        }
    }

    service.disconnect("e2e");
    service.shutdown();
    println!("ALL CHECKS PASSED");
    Ok(())
}

fn http_get(port: u16) -> Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("connecting to forwarded port 127.0.0.1:{port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .context("writing HTTP request")?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .context("reading HTTP response")?;
    Ok(response)
}
