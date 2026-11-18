//! Remote listening-port discovery.
//!
//! One `exec` channel per poll gathers:
//!
//! 1. `/proc/net/tcp` and `/proc/net/tcp6` — authoritative kernel view, works
//!    even when `ss` is not installed.
//! 2. `ss -tlpn` — process ownership, which `/proc/net/tcp` does not provide.
//! 3. `/proc/<pid>/cmdline` — full command lines for nicer labels.
//!
//! Ports are diffed between polls so the UI receives precise discovered /
//! closed events instead of re-rendering an unordered list every few seconds.

use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::config::HostId;
use crate::ssh_session::SshSession;
use crate::CoreEvent;

/// Command executed on the remote host for every poll.
pub const DISCOVERY_COMMAND: &str = r#"sh -c 'cat /proc/net/tcp /proc/net/tcp6 2>/dev/null; echo "---SS---"; ss -tlpn 2>/dev/null; echo "---CMD---"; for p in $(ss -tlpn 2>/dev/null | grep -o "pid=[0-9]*" | cut -d= -f2 | sort -u); do printf "%s " "$p"; tr "\0" " " < "/proc/$p/cmdline" 2>/dev/null; echo; done'"#;

/// A listening TCP socket found on the remote host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListeningPort {
    pub port: u16,
    /// Address the remote process is bound to (`127.0.0.1`, `0.0.0.0`, `::`, ...).
    pub bind_addr: String,
    pub process_name: Option<String>,
    pub pid: Option<u32>,
    pub command_line: Option<String>,
}

impl ListeningPort {
    /// Address a forwarder should target. Wildcard and loopback bindings are
    /// reachable through `127.0.0.1`; a socket bound to a specific interface is
    /// reached on that address.
    pub fn forward_host(&self) -> String {
        let addr = self.bind_addr.as_str();
        if addr == "0.0.0.0"
            || addr == "::"
            || addr == "::1"
            || addr == "127.0.0.1"
            || addr.starts_with("127.")
        {
            "127.0.0.1".to_string()
        } else {
            addr.to_string()
        }
    }

    /// Best-effort one-line description, e.g. `node — /srv/app (pid 4242)`.
    pub fn summary(&self) -> String {
        let mut out = self
            .process_name
            .clone()
            .unwrap_or_else(|| "unknown process".to_string());
        if let Some(pid) = self.pid {
            out.push_str(&format!(" (pid {pid})"));
        }
        if let Some(command) = self.command_line.as_ref().filter(|line| !line.is_empty()) {
            out.push_str(" — ");
            out.push_str(command);
        }
        out
    }
}

/// Rules controlling which discovered ports surface in the UI.
#[derive(Clone, Debug)]
pub struct DiscoveryFilter {
    pub interval: Duration,
    /// Ports below 1024 that are still shown.
    pub privileged_ports: Vec<u16>,
    /// Ports that must never be surfaced, notably the SSH daemon's own port.
    pub ignore_ports: Vec<u16>,
    pub min_port: u16,
    /// Exclusive upper bound; ephemeral ports are ignored.
    pub max_port: u16,
}

impl Default for DiscoveryFilter {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(3),
            privileged_ports: vec![80, 443],
            ignore_ports: vec![22],
            min_port: 1,
            max_port: 32768,
        }
    }
}

impl DiscoveryFilter {
    pub fn allows(&self, port: u16) -> bool {
        port != 22
            && !self.ignore_ports.contains(&port)
            && port >= self.min_port
            && port < self.max_port
            && (port >= 1024 || self.privileged_ports.contains(&port))
    }
}

#[derive(Debug)]
struct ProcEntry {
    port: u16,
    bind_addr: String,
    /// Socket inode. Kept for diagnostics and future use; `ss` is matched by
    /// port because it does not print inodes by default.
    inode: Option<u64>,
}

#[derive(Debug, Default)]
struct SsEntry {
    bind_addr: String,
    process_name: Option<String>,
    pid: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Proc,
    Ss,
    Cmd,
}

/// Parse the combined output of [`DISCOVERY_COMMAND`].
pub fn parse_discovery_output(output: &str) -> Vec<ListeningPort> {
    let mut section = Section::Proc;
    let mut proc_entries = Vec::new();
    let mut ss_entries: HashMap<u16, SsEntry> = HashMap::new();
    let mut cmdlines: HashMap<u32, String> = HashMap::new();

    for line in output.lines() {
        match line.trim() {
            "---SS---" => {
                section = Section::Ss;
                continue;
            }
            "---CMD---" => {
                section = Section::Cmd;
                continue;
            }
            _ => {}
        }

        match section {
            Section::Proc => {
                if let Some(entry) = parse_proc_line(line) {
                    trace!(port = entry.port, inode = ?entry.inode, "discovered proc entry");
                    proc_entries.push(entry);
                }
            }
            Section::Ss => {
                if let Some((port, entry)) = parse_ss_line(line) {
                    ss_entries.entry(port).or_insert(entry);
                }
            }
            Section::Cmd => {
                if let Some((pid, command)) = parse_cmd_line(line) {
                    cmdlines.insert(pid, command);
                }
            }
        }
    }

    let mut by_port: BTreeMap<u16, ListeningPort> = BTreeMap::new();

    for entry in proc_entries {
        let ss = ss_entries.get(&entry.port);
        let pid = ss.and_then(|entry| entry.pid);
        let command_line = pid
            .and_then(|pid| cmdlines.get(&pid).cloned())
            .filter(|line| !line.trim().is_empty());
        let process_name = ss
            .and_then(|entry| entry.process_name.clone())
            .or_else(|| command_line.as_deref().and_then(process_from_cmdline));

        let candidate = ListeningPort {
            port: entry.port,
            bind_addr: entry.bind_addr,
            process_name,
            pid,
            command_line,
        };

        match by_port.get(&candidate.port) {
            // Both /proc/net/tcp and /proc/net/tcp6 can list the same port.
            // Prefer the loopback/most specific binding for display.
            Some(existing)
                if bind_score(&existing.bind_addr) >= bind_score(&candidate.bind_addr) => {}
            _ => {
                by_port.insert(candidate.port, candidate);
            }
        }
    }

    // Ports seen by `ss` but not in /proc (containers, exotic kernels) still
    // deserve to appear.
    for (port, entry) in ss_entries {
        by_port.entry(port).or_insert_with(|| ListeningPort {
            port,
            bind_addr: entry.bind_addr,
            process_name: entry.process_name,
            pid: entry.pid,
            command_line: entry
                .pid
                .and_then(|pid| cmdlines.get(&pid).cloned())
                .filter(|line| !line.trim().is_empty()),
        });
    }

    by_port.into_values().collect()
}

fn bind_score(addr: &str) -> u8 {
    if addr.starts_with("127.") || addr == "::1" {
        3
    } else if addr == "0.0.0.0" || addr == "::" || addr.is_empty() {
        1
    } else {
        2
    }
}

fn parse_proc_line(line: &str) -> Option<ProcEntry> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    // sl local_address rem_address st tx_queue:rx_queue tr:tm->when retrnsmt
    // uid timeout inode ...
    if fields.len() < 10 || fields[0].strip_suffix(':').is_none() {
        return None;
    }
    if fields[3] != "0A" {
        return None; // TCP_LISTEN
    }
    let (bind_addr, port) = parse_proc_address(fields[1])?;
    let inode = fields[9].parse::<u64>().ok();
    Some(ProcEntry {
        port,
        bind_addr,
        inode,
    })
}

fn parse_proc_address(value: &str) -> Option<(String, u16)> {
    let (ip_hex, port_hex) = value.split_once(':')?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    let addr = match ip_hex.len() {
        8 => {
            let raw = u32::from_str_radix(ip_hex, 16).ok()?;
            Ipv4Addr::from(raw.to_le_bytes()).to_string()
        }
        32 => {
            let mut bytes = [0u8; 16];
            for (index, chunk) in ip_hex.as_bytes().chunks(8).enumerate() {
                let group = std::str::from_utf8(chunk).ok()?;
                let raw = u32::from_str_radix(group, 16).ok()?;
                bytes[index * 4..index * 4 + 4].copy_from_slice(&raw.to_le_bytes());
            }
            Ipv6Addr::from(bytes).to_string()
        }
        _ => return None,
    };
    Some((addr, port))
}

fn parse_ss_line(line: &str) -> Option<(u16, SsEntry)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 5 || fields[0] != "LISTEN" {
        return None;
    }
    let (bind_addr, port) = split_host_port(fields[3])?;
    let process = fields
        .get(5..)
        .map(|rest| rest.join(" "))
        .unwrap_or_default();
    let (process_name, pid) = parse_ss_process(&process);
    Some((
        port,
        SsEntry {
            bind_addr,
            process_name,
            pid,
        },
    ))
}

fn split_host_port(value: &str) -> Option<(String, u16)> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = normalize_ss_host(&rest[..close]);
        let port = rest[close + 1..].strip_prefix(':')?;
        return Some((host, port.parse().ok()?));
    }
    let (host, port) = value.rsplit_once(':')?;
    Some((normalize_ss_host(host), port.parse().ok()?))
}

fn normalize_ss_host(host: &str) -> String {
    let host = host.split('%').next().unwrap_or(host);
    match host {
        "" | "*" => "0.0.0.0".to_string(),
        other => other.to_string(),
    }
}

fn parse_ss_process(text: &str) -> (Option<String>, Option<u32>) {
    let name = text.find("((\"").and_then(|start| {
        text[start + 3..]
            .split('"')
            .next()
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    });
    let pid = text.find("pid=").and_then(|start| {
        let digits: String = text[start + 4..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    });
    (name, pid)
}

fn parse_cmd_line(line: &str) -> Option<(u32, String)> {
    let (pid, command) = line.split_once(' ')?;
    let pid: u32 = pid.trim().parse().ok()?;
    Some((pid, command.trim().to_string()))
}

fn process_from_cmdline(command: &str) -> Option<String> {
    let first = command.split_whitespace().next()?;
    let name = first.rsplit('/').next().unwrap_or(first);
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Hooks invoked as the discovered port set changes.
#[derive(Clone)]
pub struct DiscoveryCallbacks {
    /// Called once for each newly seen port, so the caller can auto-forward it.
    pub on_discovered: Arc<dyn Fn(ListeningPort) + Send + Sync>,
    /// Called when a previously seen port disappears.
    pub on_closed: Arc<dyn Fn(u16) + Send + Sync>,
    /// Called with the full set after every successful poll.
    pub on_snapshot: Arc<dyn Fn(Vec<ListeningPort>) + Send + Sync>,
}

/// Poll the remote host forever, emitting diff events.
///
/// Discovery survives SSH reconnects: while the transport is down it simply
/// skips polls and keeps the last known port set.
pub async fn run(
    session: Arc<SshSession>,
    host_id: HostId,
    filter: DiscoveryFilter,
    events: broadcast::Sender<CoreEvent>,
    cancel: CancellationToken,
    callbacks: DiscoveryCallbacks,
) {
    let DiscoveryCallbacks {
        on_discovered,
        on_closed,
        on_snapshot,
    } = callbacks;
    let mut known: BTreeMap<u16, ListeningPort> = BTreeMap::new();
    let mut first_poll = true;

    loop {
        if !first_poll {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(filter.interval) => {}
            }
        }
        first_poll = false;
        if cancel.is_cancelled() {
            break;
        }

        if session.current_handle().await.is_none() {
            continue;
        }

        let output = match session.exec(DISCOVERY_COMMAND).await {
            Ok(output) => output,
            Err(err) => {
                debug!(host = %host_id, "port discovery poll failed: {err:#}");
                continue;
            }
        };

        let current: BTreeMap<u16, ListeningPort> = parse_discovery_output(&output)
            .into_iter()
            .filter(|entry| filter.allows(entry.port))
            .map(|entry| (entry.port, entry))
            .collect();

        on_snapshot(current.values().cloned().collect());

        for (port, entry) in &current {
            if !known.contains_key(port) {
                let _ = events.send(CoreEvent::RemotePortDiscovered {
                    host_id: host_id.clone(),
                    port: entry.clone(),
                });
                on_discovered(entry.clone());
            }
        }
        for port in known.keys() {
            if !current.contains_key(port) {
                let _ = events.send(CoreEvent::RemotePortClosed {
                    host_id: host_id.clone(),
                    port: *port,
                });
                on_closed(*port);
            }
        }

        known = current;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111111 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 222222 1 0000000000000000 100 0 0 10 0
   2: 0100007F:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 333333 1 0000000000000000 100 0 0 10 0
   3: 0100007F:9C40 0100007F:0BB8 01 00000000:00000000 00:00000000 00000000  1000        0 444444 1 0000000000000000 100 0 0 10 0
---SS---
State  Recv-Q Send-Q Local Address:Port Peer Address:Port Process
LISTEN 0      511          127.0.0.1:3000      0.0.0.0:*    users:(("node",pid=4242,fd=20))
LISTEN 0      128          0.0.0.0:22        0.0.0.0:*    users:(("sshd",pid=1,fd=3))
LISTEN 0      4096             [::]:8000          [::]:*    users:(("python3",pid=777,fd=5))
---CMD---
4242 node /srv/app/node_modules/.bin/vite --port 3000
1 /usr/sbin/sshd -D
777 python3 -m http.server 8000
"#;

    #[test]
    fn parses_listening_sockets_only() {
        let ports = parse_discovery_output(SAMPLE);
        let numbers: Vec<u16> = ports.iter().map(|p| p.port).collect();
        // 40000 is ESTABLISHED (st 01) and must be ignored; 8080 only exists
        // in /proc/net/tcp6, 8000 only exists in the `ss` section.
        assert_eq!(numbers, vec![22, 3000, 8000, 8080]);
    }

    #[test]
    fn merges_process_metadata() {
        let ports = parse_discovery_output(SAMPLE);
        let vite = ports.iter().find(|p| p.port == 3000).unwrap();
        assert_eq!(vite.bind_addr, "127.0.0.1");
        assert_eq!(vite.process_name.as_deref(), Some("node"));
        assert_eq!(vite.pid, Some(4242));
        assert_eq!(
            vite.command_line.as_deref(),
            Some("node /srv/app/node_modules/.bin/vite --port 3000")
        );

        let python = ports.iter().find(|p| p.port == 8000).unwrap();
        assert_eq!(python.bind_addr, "::");
        assert_eq!(python.process_name.as_deref(), Some("python3"));
    }

    #[test]
    fn decodes_hex_addresses() {
        assert_eq!(
            parse_proc_address("0100007F:0BB8"),
            Some(("127.0.0.1".to_string(), 3000))
        );
        assert_eq!(
            parse_proc_address("00000000:0016"),
            Some(("0.0.0.0".to_string(), 22))
        );
        assert_eq!(
            parse_proc_address("00000000000000000000000001000000:1F90"),
            Some(("::1".to_string(), 8080))
        );
        assert!(parse_proc_address("zzzz:1234").is_none());
    }

    #[test]
    fn filters_default_ports() {
        let filter = DiscoveryFilter::default();
        assert!(!filter.allows(22));
        assert!(!filter.allows(1023));
        assert!(filter.allows(1024));
        assert!(filter.allows(3000));
        assert!(!filter.allows(32768));
        assert!(filter.allows(5432));

        let with_privileged = DiscoveryFilter {
            privileged_ports: vec![80, 443],
            ..DiscoveryFilter::default()
        };
        assert!(with_privileged.allows(80));
        assert!(with_privileged.allows(443));
        assert!(!with_privileged.allows(25));

        let ignore_ssh = DiscoveryFilter {
            ignore_ports: vec![22, 2222],
            ..DiscoveryFilter::default()
        };
        assert!(!ignore_ssh.allows(2222));
        assert!(ignore_ssh.allows(2223));
    }

    #[test]
    fn forward_host_prefers_loopback() {
        let mut entry = ListeningPort {
            port: 3000,
            bind_addr: "0.0.0.0".into(),
            process_name: None,
            pid: None,
            command_line: None,
        };
        assert_eq!(entry.forward_host(), "127.0.0.1");
        entry.bind_addr = "::".into();
        assert_eq!(entry.forward_host(), "127.0.0.1");
        entry.bind_addr = "10.0.0.7".into();
        assert_eq!(entry.forward_host(), "10.0.0.7");
    }
}
