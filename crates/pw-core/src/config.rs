//! Configuration: `~/.ssh/config` import, Porty settings persistence and
//! the quick-add port-forwarding syntax.
//!
//! Nothing in this module touches the network. It is shared verbatim by the
//! desktop UI, a future CLI and the iOS shell.

use std::collections::HashSet;
use std::fmt;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use ssh2_config::{ParseRule, SshConfig};

/// Stable identifier for a host. Aliases from `~/.ssh/config` are unique
/// already; manually added hosts are required to pick a non-conflicting name.
pub type HostId = String;

/// Monotonic identifier for a forwarding rule.
pub type RuleId = u64;

/// Which authentication method the user prefers for a host.
///
/// Regardless of the preferred method, [`crate::ssh_session`] falls back to the
/// other methods when the preferred one is unavailable (an agent that is not
/// running, an encrypted key, ...).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// Use the system SSH agent (`SSH_AUTH_SOCK`, OpenSSH named pipe or Pageant).
    #[default]
    Agent,
    /// Use an `IdentityFile` / `~/.ssh/id_*` private key.
    Key,
    /// Use a password. Passwords are never written to disk by this crate.
    Password,
}

impl AuthKind {
    pub fn label(self) -> &'static str {
        match self {
            AuthKind::Agent => "SSH agent",
            AuthKind::Key => "Private key",
            AuthKind::Password => "Password",
        }
    }
}

/// Where a host entry originated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostSource {
    /// Imported from `~/.ssh/config`.
    SshConfig,
    /// Added in the Porty UI.
    Manual,
}

/// A host as read from `~/.ssh/config` (before user preferences are applied).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshConfigHost {
    pub alias: String,
    pub host_name: String,
    pub port: u16,
    pub user: Option<String>,
    pub identity_files: Vec<PathBuf>,
    pub proxy_jump: Option<String>,
}

/// A host known to Porty, including per-host preferences.
#[derive(Clone, Serialize, Deserialize)]
pub struct HostConfig {
    pub id: HostId,
    /// Name shown in the sidebar (the `Host` alias).
    pub alias: String,
    /// Address actually dialed.
    pub host_name: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: Option<String>,
    #[serde(default)]
    pub auth: AuthKind,
    /// Preferred private key, if the user picked one.
    pub identity_file: Option<PathBuf>,
    /// Keys discovered from `~/.ssh/config` (or the defaults).
    #[serde(default)]
    pub identity_files: Vec<PathBuf>,
    #[serde(default)]
    pub proxy_jump: Option<String>,
    pub source: HostSource,
    /// Poll the remote for listening ports while connected.
    #[serde(default)]
    pub auto_discover: bool,
    /// Automatically forward ports discovered on this host.
    #[serde(default)]
    pub auto_forward: bool,
    /// Bind forwarded ports on `0.0.0.0` (LAN / Tailscale) instead of loopback.
    #[serde(default)]
    pub bind_all: bool,
    /// In-memory password. Deliberately skipped by serde so credentials never
    /// end up in `settings.json`.
    #[serde(skip)]
    pub password: Option<String>,
}

impl fmt::Debug for HostConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostConfig")
            .field("id", &self.id)
            .field("host_name", &self.host_name)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("auth", &self.auth)
            .field("identity_file", &self.identity_file)
            .field("source", &self.source)
            .field("auto_discover", &self.auto_discover)
            .field("auto_forward", &self.auto_forward)
            .field("bind_all", &self.bind_all)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl HostConfig {
    pub fn from_ssh(host: SshConfigHost) -> Self {
        Self {
            id: host.alias.clone(),
            alias: host.alias,
            host_name: host.host_name,
            port: host.port,
            user: host.user,
            auth: AuthKind::Agent,
            identity_file: host.identity_files.first().cloned(),
            identity_files: host.identity_files,
            proxy_jump: host.proxy_jump,
            source: HostSource::SshConfig,
            auto_discover: false,
            auto_forward: false,
            bind_all: false,
            password: None,
        }
    }

    /// Build a manually-added host.
    pub fn manual(alias: &str, host_name: &str, port: u16, user: Option<String>) -> Self {
        Self {
            id: alias.to_string(),
            alias: alias.to_string(),
            host_name: host_name.to_string(),
            port,
            user,
            auth: AuthKind::Agent,
            identity_file: None,
            identity_files: default_identity_files(),
            proxy_jump: None,
            source: HostSource::Manual,
            auto_discover: true,
            auto_forward: false,
            bind_all: false,
            password: None,
        }
    }

    /// All key paths to try, most specific first.
    pub fn candidate_keys(&self) -> Vec<PathBuf> {
        let mut keys = Vec::new();
        if let Some(key) = &self.identity_file {
            keys.push(key.clone());
        }
        for key in &self.identity_files {
            if !keys.contains(key) {
                keys.push(key.clone());
            }
        }
        for key in default_identity_files() {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    }

    pub fn display_target(&self) -> String {
        match &self.user {
            Some(user) if !user.is_empty() => {
                format!("{user}@{}:{}", self.host_name, self.port)
            }
            _ => format!("{}:{}", self.host_name, self.port),
        }
    }
}

/// A forwarding rule as persisted by the UI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuleConfig {
    pub id: RuleId,
    pub host_id: HostId,
    /// Address the remote SSH server should connect to (usually `127.0.0.1`).
    pub remote_host: String,
    pub remote_port: u16,
    /// The local port the user asked for. The actual port may differ when the
    /// requested one is busy (VS Code-style auto-bump).
    pub requested_local_port: u16,
    #[serde(default)]
    pub bind_all: bool,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub process_name: Option<String>,
    #[serde(default)]
    pub auto_discovered: bool,
    /// Start this rule automatically whenever the host connects and on launch.
    #[serde(default)]
    pub auto_start: bool,
}

/// Theme preference persisted across launches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    #[default]
    System,
    Dark,
    Light,
}

/// Everything Porty persists.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "settings_version")]
    pub version: u32,
    #[serde(default)]
    pub hosts: Vec<HostConfig>,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
    /// Discovery poll interval in seconds.
    #[serde(default = "default_poll_interval")]
    pub discovery_interval_secs: u64,
    /// Ports below 1024 that discovery should still surface.
    #[serde(default = "default_privileged_ports")]
    pub privileged_ports: Vec<u16>,
    #[serde(default)]
    pub theme: ThemePreference,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: settings_version(),
            hosts: Vec::new(),
            rules: Vec::new(),
            discovery_interval_secs: default_poll_interval(),
            privileged_ports: default_privileged_ports(),
            theme: ThemePreference::System,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let settings: Settings = serde_json::from_str(&raw)
                    .with_context(|| format!("parsing {}", path.display()))?;
                Ok(settings)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        // Write to a sibling first so a crash cannot truncate the settings file.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        // Windows cannot rename over an existing file.
        #[cfg(windows)]
        if path.exists() {
            std::fs::remove_file(path).ok();
        }
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    pub fn host(&self, id: &str) -> Option<&HostConfig> {
        self.hosts.iter().find(|host| host.id == id)
    }

    pub fn host_mut(&mut self, id: &str) -> Option<&mut HostConfig> {
        self.hosts.iter_mut().find(|host| host.id == id)
    }
}

fn settings_version() -> u32 {
    1
}

fn default_ssh_port() -> u16 {
    22
}

fn default_poll_interval() -> u64 {
    3
}

fn default_privileged_ports() -> Vec<u16> {
    vec![80, 443, 3000]
}

/// `%APPDATA%\Porty` on Windows, `~/Library/Application Support/Porty`
/// on macOS and `$XDG_CONFIG_HOME/porty` elsewhere.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PORTY_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("Porty");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = home_dir() {
            return home
                .join("Library")
                .join("Application Support")
                .join("Porty");
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(dir).join("porty");
        }
        if let Some(home) = home_dir() {
            return home.join(".config").join("porty");
        }
    }
    PathBuf::from(".porty")
}

pub fn default_settings_path() -> PathBuf {
    if let Some(path) = std::env::var_os("PORTY_SETTINGS") {
        return PathBuf::from(path);
    }
    config_dir().join("settings.json")
}

pub fn home_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

pub fn ssh_config_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".ssh").join("config"))
}

pub fn known_identity_files() -> [&'static str; 5] {
    ["id_ed25519", "id_ecdsa", "id_rsa", "id_dsa", "id_ecdsa_sk"]
}

/// Expand a leading `~/` using the user's home directory.
pub fn expand_tilde(path: PathBuf) -> PathBuf {
    if let Some(rest) = path.to_str().and_then(|value| value.strip_prefix("~/")) {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    path
}

pub fn default_identity_files() -> Vec<PathBuf> {
    let Some(ssh_dir) = home_dir().map(|home| home.join(".ssh")) else {
        return Vec::new();
    };
    known_identity_files()
        .iter()
        .map(|name| ssh_dir.join(name))
        .filter(|path| path.exists())
        .collect()
}

pub fn current_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "root".to_string())
}

/// Parse `~/.ssh/config`, skipping wildcard-only blocks.
///
/// The file is parsed leniently: unknown directives are ignored, because real
/// world configs regularly contain options Porty does not consume.
pub fn read_ssh_hosts(path: &Path) -> Result<Vec<SshConfigHost>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut config = SshConfig::default();
    if let Some(home) = home_dir() {
        config = config.home_dir(home);
    }
    let config = config
        .parse(
            &mut reader,
            ParseRule::ALLOW_UNKNOWN_FIELDS | ParseRule::ALLOW_UNSUPPORTED_FIELDS,
        )
        .map_err(|err| anyhow!("failed to parse {}: {err}", path.display()))?;

    let mut hosts = Vec::new();
    let mut seen = HashSet::new();
    for host in config.get_hosts() {
        for clause in &host.pattern {
            if clause.negated {
                continue;
            }
            let alias = clause.pattern.trim();
            // Wildcard blocks describe defaults, not connectable hosts.
            if alias.is_empty()
                || alias.contains('*')
                || alias.contains('?')
                || alias.contains('!')
                || !seen.insert(alias.to_string())
            {
                continue;
            }
            let params = config.query(alias);
            let identity_files = params
                .identity_file
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(expand_tilde)
                .collect::<Vec<_>>();
            hosts.push(SshConfigHost {
                alias: alias.to_string(),
                host_name: params
                    .host_name
                    .clone()
                    .unwrap_or_else(|| alias.to_string()),
                port: params.port.unwrap_or(22),
                user: params.user.clone(),
                identity_files: if identity_files.is_empty() {
                    default_identity_files()
                } else {
                    identity_files
                },
                proxy_jump: params
                    .proxy_jump
                    .as_ref()
                    .filter(|jumps| !jumps.is_empty())
                    .map(|jumps| jumps.join(",")),
            });
        }
    }
    hosts.sort_by_key(|host| host.alias.to_lowercase());
    Ok(hosts)
}

/// Merge imported SSH hosts into the settings, preserving user preferences.
///
/// Returns `true` when the settings changed and should be persisted.
pub fn merge_ssh_hosts(settings: &mut Settings, path: &Path) -> Result<bool> {
    let imported = read_ssh_hosts(path)?;
    let imported_aliases: HashSet<String> = imported.iter().map(|h| h.alias.clone()).collect();
    let mut changed = false;

    for host in imported {
        match settings
            .hosts
            .iter_mut()
            .find(|existing| existing.id == host.alias)
        {
            Some(existing) if existing.source == HostSource::SshConfig => {
                if existing.host_name != host.host_name
                    || existing.port != host.port
                    || existing.user != host.user
                    || existing.identity_files != host.identity_files
                    || existing.proxy_jump != host.proxy_jump
                {
                    existing.host_name = host.host_name;
                    existing.port = host.port;
                    existing.user = host.user;
                    existing.identity_files = host.identity_files;
                    existing.proxy_jump = host.proxy_jump;
                    changed = true;
                }
            }
            // A manual host with the same alias shadows the config entry.
            Some(_) => {}
            None => {
                settings.hosts.push(HostConfig::from_ssh(host));
                changed = true;
            }
        }
    }

    // Remove hosts that vanished from `~/.ssh/config`; keep manual hosts.
    let before = settings.hosts.len();
    settings
        .hosts
        .retain(|host| host.source == HostSource::Manual || imported_aliases.contains(&host.alias));
    if settings.hosts.len() != before {
        changed = true;
    }

    settings.hosts.sort_by_key(|host| host.alias.to_lowercase());
    Ok(changed)
}

/// A parsed quick-add expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortSpec {
    pub remote_host: String,
    pub remote_port: u16,
    pub local_port: u16,
}

impl PortSpec {
    pub fn summary(&self) -> String {
        format!(
            "127.0.0.1:{} -> {}:{}",
            self.local_port, self.remote_host, self.remote_port
        )
    }
}

/// Parse the flexible quick-add grammar:
///
/// * `5173`                          -> remote `127.0.0.1:5173`, local 5173
/// * `8080:3000`                     -> local 8080, remote `127.0.0.1:3000`
/// * `3000:8080:3000`                -> local 3000, remote `8080:3000` (ssh -L shape)
/// * `10.0.0.12:5432->15432`         -> local 15432, remote `10.0.0.12:5432`
/// * `[::1]:5432->15432`             -> IPv6 remote address
pub fn parse_port_spec(input: &str) -> Result<PortSpec> {
    let raw = input.trim();
    if raw.is_empty() {
        bail!("enter a port, e.g. 3000, 8080:3000 or 10.0.0.5:5432->15432");
    }

    if let Some((remote, local)) = raw.split_once("->") {
        let (remote_host, remote_port) = parse_remote(remote.trim())?;
        let local_port = parse_port(local.trim())?;
        return Ok(PortSpec {
            remote_host,
            remote_port,
            local_port,
        });
    }

    // Bracketed IPv6, optionally with local/remote portions.
    if raw.starts_with('[') {
        let (remote_host, remote_port, rest) = parse_bracketed(raw)?;
        if let Some(local) = rest {
            let local_port = parse_port(&local)?;
            return Ok(PortSpec {
                remote_host,
                remote_port,
                local_port,
            });
        }
        return Ok(PortSpec {
            local_port: remote_port,
            remote_host,
            remote_port,
        });
    }

    let colon_count = raw.matches(':').count();
    match colon_count {
        0 => {
            let port = parse_port(raw)?;
            Ok(PortSpec {
                remote_host: "127.0.0.1".to_string(),
                remote_port: port,
                local_port: port,
            })
        }
        1 => {
            // local:remote
            let (local, remote) = raw.split_once(':').expect("one colon");
            Ok(PortSpec {
                remote_host: "127.0.0.1".to_string(),
                remote_port: parse_port(remote)?,
                local_port: parse_port(local)?,
            })
        }
        2 => {
            // host:remote:local
            let mut parts = raw.splitn(3, ':');
            let host = parts.next().unwrap_or_default().trim();
            let remote = parts.next().unwrap_or_default().trim();
            let local = parts.next().unwrap_or_default().trim();
            if host.is_empty() {
                bail!("missing remote host in `{raw}`");
            }
            Ok(PortSpec {
                remote_host: host.to_string(),
                remote_port: parse_port(remote)?,
                local_port: parse_port(local)?,
            })
        }
        _ => bail!(
            "could not parse `{raw}`; use 3000, 8080:3000, host:5432:15432 or host:5432->15432"
        ),
    }
}

fn parse_remote(input: &str) -> Result<(String, u16)> {
    let input = input.trim();
    if input.is_empty() {
        bail!("missing remote port");
    }
    if input.starts_with('[') {
        let (host, port, rest) = parse_bracketed(input)?;
        if rest.is_some() {
            bail!("unexpected trailing data after `{input}`");
        }
        return Ok((host, port));
    }
    match input.rsplit_once(':') {
        Some((host, port)) => {
            if host.is_empty() {
                bail!("missing remote host before `:{}`", port);
            }
            Ok((host.to_string(), parse_port(port)?))
        }
        None => Ok(("127.0.0.1".to_string(), parse_port(input)?)),
    }
}

/// Parse `[ipv6]:port` and return the trailing text (used for `[::1]:5432:15432`).
fn parse_bracketed(input: &str) -> Result<(String, u16, Option<String>)> {
    let close = input
        .find(']')
        .ok_or_else(|| anyhow!("missing `]` in `{input}`"))?;
    let host = input[1..close].trim();
    if host.is_empty() {
        bail!("empty IPv6 address in `{input}`");
    }
    let rest = &input[close + 1..];
    let rest = rest.trim_start();
    let rest = rest
        .strip_prefix(':')
        .ok_or_else(|| anyhow!("missing `:port` after `]` in `{input}`"))?;
    match rest.split_once(':') {
        Some((port, tail)) => Ok((host.to_string(), parse_port(port)?, Some(tail.to_string()))),
        None => Ok((host.to_string(), parse_port(rest)?, None)),
    }
}

fn parse_port(input: &str) -> Result<u16> {
    let input = input.trim();
    if input.is_empty() {
        bail!("missing port number");
    }
    let port: u16 = input
        .parse()
        .map_err(|_| anyhow!("`{input}` is not a valid port (1-65535)"))?;
    if port == 0 {
        bail!("port 0 is not a valid forwarding port");
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_port() {
        let spec = parse_port_spec("5173").unwrap();
        assert_eq!(spec.remote_host, "127.0.0.1");
        assert_eq!(spec.remote_port, 5173);
        assert_eq!(spec.local_port, 5173);
    }

    #[test]
    fn parses_local_remote_pair() {
        let spec = parse_port_spec("8080:3000").unwrap();
        assert_eq!(spec.remote_host, "127.0.0.1");
        assert_eq!(spec.remote_port, 3000);
        assert_eq!(spec.local_port, 8080);
    }

    #[test]
    fn parses_ssh_l_triple() {
        // host:remote_port:local_port, the same shape as `ssh -L`.
        let spec = parse_port_spec("10.0.0.2:5432:3000").unwrap();
        assert_eq!(spec.remote_host, "10.0.0.2");
        assert_eq!(spec.remote_port, 5432);
        assert_eq!(spec.local_port, 3000);
    }

    #[test]
    fn parses_arrow_syntax() {
        let spec = parse_port_spec("10.0.0.12:5432->15432").unwrap();
        assert_eq!(spec.remote_host, "10.0.0.12");
        assert_eq!(spec.remote_port, 5432);
        assert_eq!(spec.local_port, 15432);
    }

    #[test]
    fn parses_arrow_with_bare_port() {
        let spec = parse_port_spec(" 3000 -> 4000 ").unwrap();
        assert_eq!(spec.remote_host, "127.0.0.1");
        assert_eq!(spec.remote_port, 3000);
        assert_eq!(spec.local_port, 4000);
    }

    #[test]
    fn parses_ipv6() {
        let spec = parse_port_spec("[::1]:5432->15432").unwrap();
        assert_eq!(spec.remote_host, "::1");
        assert_eq!(spec.remote_port, 5432);
        assert_eq!(spec.local_port, 15432);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_port_spec("").is_err());
        assert!(parse_port_spec("abc").is_err());
        assert!(parse_port_spec("0").is_err());
        assert!(parse_port_spec("70000").is_err());
        assert!(parse_port_spec("1:2:3:4").is_err());
    }

    #[test]
    fn parses_ssh_config_entries() {
        let dir = std::env::temp_dir().join(format!(
            "porty-sshconfig-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config");
        std::fs::write(
            &path,
            "\
Host prod
    HostName prod.example.com
    User deploy
    Port 2222
    IdentityFile ~/.ssh/id_prod_test

Host *.internal
    User ops

Host * !prod
    ServerAliveInterval 30
",
        )
        .unwrap();

        let hosts = read_ssh_hosts(&path).unwrap();
        assert_eq!(hosts.len(), 1, "wildcard blocks must be skipped");
        let prod = &hosts[0];
        assert_eq!(prod.alias, "prod");
        assert_eq!(prod.host_name, "prod.example.com");
        assert_eq!(prod.user.as_deref(), Some("deploy"));
        assert_eq!(prod.port, 2222);

        let key = prod.identity_files.first().unwrap();
        assert!(
            key.ends_with(".ssh/id_prod_test"),
            "IdentityFile should be expanded: {}",
            key.display()
        );
        assert!(
            !key.to_string_lossy().starts_with('~'),
            "tilde must not survive: {}",
            key.display()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn settings_roundtrip_skips_passwords() {
        let mut settings = Settings::default();
        let mut host = HostConfig::manual("box", "example.com", 22, Some("dev".into()));
        host.password = Some("hunter2".into());
        settings.hosts.push(host);

        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("hunter2"));
        let parsed: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.hosts.len(), 1);
        assert!(parsed.hosts[0].password.is_none());
        assert_eq!(parsed.hosts[0].display_target(), "dev@example.com:22");
    }
}
