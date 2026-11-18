<img src="assets/icon.svg" width="64" height="64" alt="">

# Porty

[![build](https://github.com/ztxtxwd/Porty/actions/workflows/build.yml/badge.svg)](https://github.com/ztxtxwd/Porty/actions/workflows/build.yml)

A native, GPU-accelerated SSH port-forwarding and remote port discovery app
built in pure Rust with [GPUI](https://gpui.rs) and
[gpui-component](https://github.com/longbridge/gpui-component) — the same UI
stack as Zed. No Electron, no embedded browser, no JavaScript runtime.

It recreates the workflow of VS Code Remote-SSH's **Ports** panel:

- import hosts from `~/.ssh/config` automatically
- discover listening ports on the remote server in real time
- forward them to `127.0.0.1` (or `0.0.0.0` for LAN / Tailscale access)
- quick-add forwards with `3000`, `8080:3000`, `host:5432->15432`
- live connection counts and traffic counters, one-click browser / copy

The networking engine is a standalone crate with zero UI dependencies, so the
same code can power a CLI, a daemon, or an iOS app later.

## Download

GitHub Actions builds two portable bundles on every push:

| Platform | Artifact |
| --- | --- |
| Windows 10/11 x86_64 | `Porty-Windows-x64.zip` |
| macOS 11+ Apple Silicon | `Porty-macOS-arm64.zip` |

Open the [latest build](https://github.com/ztxtxwd/Porty/actions/workflows/build.yml)
and download the artifact for your platform, or grab the packaged zips from
[Releases](https://github.com/ztxtxwd/Porty/releases).

The binaries are unsigned. On Windows, SmartScreen → **More info → Run anyway**.
On macOS, right-click the app → **Open**, or run
`xattr -dr com.apple.quarantine /Applications/Porty.app` if Gatekeeper complains.

## Workspace

```text
porty/
├── Cargo.toml                 # workspace root
├── crates/
│   ├── pw-core/               # headless SSH engine (no UI dependencies)
│   │   ├── src/config.rs      # ~/.ssh/config import + settings persistence
│   │   ├── src/ssh_session.rs # russh client, auth, keepalive, reconnect
│   │   ├── src/forwarder.rs   # listener -> direct-tcpip bridge + metrics
│   │   ├── src/discovery.rs   # /proc/net/tcp + ss -tlpn polling and diffs
│   │   ├── src/lib.rs         # Porty service + CoreEvent bus
│   │   └── examples/e2e.rs    # real SSH end-to-end smoke test
│   └── pw-gui/
│       ├── src/main.rs        # GPUI bootstrap, Tokio runtime bridge, assets
│       ├── src/app.rs         # sidebar, header, quick-add, host credentials
│       ├── src/ports_view.rs  # virtualized ports table (TableDelegate)
│       └── src/theme.rs       # theme helpers and traffic formatting
├── assets/                    # app icon + embedded Lucide SVGs
├── packaging/                 # macOS Info.plist and Chinese quickstart
├── scripts/e2e.sh             # starts sshd + HTTP server, runs the e2e test
├── .github/workflows/build.yml # Windows x64 + macOS arm64 CI
└── vendor/xattr/              # Linux build fix for a transitive dependency
```

## Requirements

- Rust 1.85+ (edition 2021 crates)
- macOS: Xcode command line tools
- Windows: MSVC Build Tools + Windows SDK
- Linux (development only): `libxkbcommon-dev`, `libxkbcommon-x11-dev`,
  `libxcb1-dev`, `libx11-dev`, `cmake`, `clang` and a Vulkan driver

## Run

```bash
cargo run -p pw-gui          # debug build
cargo run -p pw-gui --release
```

### Windows

A normal Windows build with `cargo build --release -p pw-gui` produces
`target\release\porty.exe`. GPUI compiles its HLSL shaders with the
Windows SDK's `fxc.exe`, and Porty embeds the app icon, version info and a
PerMonitorV2 DPI manifest through `assets/icon.ico`.

Cross-compiling the exe from Linux (`x86_64-pc-windows-gnu`) is also supported.
It needs mingw-w64, `nasm`, Wine and Microsoft's `fxc.exe` (from the Windows
SDK). GPUI skips HLSL compilation when the host is not Windows, so generate
`build/<gpui>/out/shaders_bytes.rs` with
[`scripts/gen_windows_shaders.py`](scripts/gen_windows_shaders.py) between two
build passes; see the script header for the exact commands. The bundled
`build.rs` embeds the icon and DPI manifest for cross builds as well.

### macOS

`cargo build --release -p pw-gui` builds for the host architecture. The CI job
packages an arm64 `Porty.app` (ad-hoc signed, not notarized) with
`packaging/macos/Info.plist` and `assets/icon.icns`.

## Configuration

The app stores settings in the platform config directory:

| OS      | Path                                                    |
| ------- | ------------------------------------------------------- |
| macOS   | `~/Library/Application Support/Porty/settings.json` |
| Windows | `%APPDATA%\Porty\settings.json`                     |
| Linux   | `$XDG_CONFIG_HOME/porty/settings.json`              |

Override with `PORTY_SETTINGS=/path/to/settings.json` or
`PORTY_CONFIG_DIR=/path/to/dir`.

Passwords are **never** written to disk: `HostConfig::password` is
`#[serde(skip)]`, and the credentials panel keeps them in memory only.

## Quick-add syntax

| Input                     | Remote target         | Local port |
| ------------------------- | --------------------- | ---------- |
| `5173`                    | `127.0.0.1:5173`      | `5173`     |
| `8080:3000`               | `127.0.0.1:3000`      | `8080`     |
| `10.0.0.2:5432:15432`     | `10.0.0.2:5432`       | `15432`    |
| `10.0.0.12:5432->15432`   | `10.0.0.12:5432`      | `15432`    |
| `[::1]:5432->15432`       | `[::1]:5432`          | `15432`    |

When the requested local port is busy, the forwarder bumps to the next free
port (as VS Code does) and the mapping column highlights the change in amber.

## How it works

### Authentication

Each host tries its preferred method first, then falls back:

1. **SSH agent** — `AgentClient::connect_env()` (`SSH_AUTH_SOCK`). On Windows
   the OpenSSH named pipe and Pageant are tried.
2. **Private keys** — `IdentityFile` entries from `~/.ssh/config` followed by
   `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`, `id_dsa`.
3. **Password** — entered in the credentials panel, kept in memory.

Host keys are verified with OpenSSH's `known_hosts` and unknown keys are
recorded on first use (`accept-new` semantics).

### Forwarding

`forwarder.rs` binds a Tokio `TcpListener`, accepts connections, opens a
`direct-tcpip` channel on the multiplexed `russh` handle, and pipes bytes with
`tokio::io::copy_bidirectional` while counting them. The SSH connection can drop
and reconnect without restarting listeners; new connections simply use the new
transport. A `CancellationToken` stops the listener and all active streams.

### Remote discovery

A single `exec` per poll gathers:

```sh
cat /proc/net/tcp /proc/net/tcp6; ss -tlpn; <cmdlines for the pids ss reported>
```

Listening sockets (`st == 0A`) are decoded from hex, merged with `ss` process
ownership, and filtered:

- port 22 and the host's own SSH port are always ignored
- ports below 1024 only when whitelisted (default `80`, `443`)
- ephemeral ports (`>= 32768`) are ignored
- ports that Porty itself is listening on are ignored, which prevents a
  feedback loop when the SSH target is the local machine

New ports are emitted as `CoreEvent::RemotePortDiscovered`, removed ports as
`RemotePortClosed`, and `auto_forward` hosts get a working rule automatically.
If the remote service restarts, its auto-created rule is reused (keeping the
same local port when possible) instead of duplicating.

## Keyboard shortcuts

| Shortcut                    | Action                          |
| --------------------------- | ------------------------------- |
| `Cmd/Ctrl + N`              | Focus the quick-add bar         |
| `Cmd/Ctrl + F` or `/`       | Focus the port filter           |
| `Cmd/Ctrl + Enter`          | Connect / disconnect the host   |
| `Cmd/Ctrl + R`              | Re-import `~/.ssh/config`       |
| `Cmd/Ctrl + Shift + L`      | Toggle light / dark theme       |

## Troubleshooting

Porty writes logs to the platform config directory:

| OS | Log |
| --- | --- |
| Windows | `%APPDATA%\Porty\logs\porty.log` |
| macOS | `~/Library/Application Support/Porty/logs/porty.log` |
| Linux | `$XDG_CONFIG_HOME/porty/logs/porty.log` |

Panics are recorded there with their source location and a backtrace. Set
`RUST_LOG=debug` before launching for verbose logs (on Windows:
`$env:RUST_LOG="debug"; .\porty.exe`).

## Tests

```bash
cargo test --workspace       # unit tests (parsers, bind bump, settings)
cargo clippy --workspace --all-targets
```

A real end-to-end test needs `sshd` (run as root on Linux):

```bash
sudo scripts/e2e.sh
```

It starts a throwaway sshd and HTTP server, then verifies key authentication,
port discovery, auto-forwarding, an actual HTTP request through the tunnel, and
the live byte counters.

## Notes and current limitations

- **Remote discovery targets Linux.** It relies on `/proc/net/tcp` and `ss`;
  other server OSes need an additional collector.
- **ProxyJump is parsed but not used.** Direct connections only.
- **macOS Keychain / Windows Credential Manager** are not integrated yet;
  password auth is session-scoped.
- **Linux build fix**: `gpui` pulls in `zed-async-tar` → `xattr 0.2.3`, which no
  longer compiles against recent `libc` on Linux. `vendor/xattr` is upstream
  0.2.3 with `ENOATTR` mapped to `ENODATA` on Linux; macOS/Windows are
  unaffected. Remove the `[patch.crates-io]` entry once upstream fixes it.
- UI icons come from [Lucide](https://lucide.dev) (ISC); each SVG keeps its
  upstream license header.
- gpui-component 0.5.1 is the last release built directly on the published
  `gpui = "0.2.2"` crate; 0.6+ uses a separate snapshot and is intentionally
  not used here.

Created in [T3 Code](https://t3.codes).
