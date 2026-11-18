#!/usr/bin/env bash
#
# End-to-end smoke test for the headless engine.
#
# Starts a throwaway sshd and HTTP server on 127.0.0.1, then runs
# `pw-core --example e2e`, which authenticates with an ed25519 key,
# discovers listening ports, auto-forwards the HTTP server and performs a
# real request through the SSH tunnel.
#
# Usage:
#   sudo scripts/e2e.sh
#
# Environment overrides:
#   PW_E2E_DIR        working directory (default /tmp/porty-e2e)
#   PW_E2E_SSH_PORT   sshd port       (default 2222)
#   PW_E2E_REMOTE_PORT forwarded port (default 18080)
#   PW_E2E_USER       SSH user        (default current user)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="${PW_E2E_DIR:-/tmp/porty-e2e}"
SSH_PORT="${PW_E2E_SSH_PORT:-2222}"
REMOTE_PORT="${PW_E2E_REMOTE_PORT:-18080}"
SSH_USER="${PW_E2E_USER:-$(id -un)}"

for bin in sshd ssh-keygen python3; do
    if ! command -v "$bin" >/dev/null 2>&1 && [ ! -x "/usr/sbin/$bin" ]; then
        echo "error: $bin is required for the e2e test" >&2
        exit 1
    fi
done

mkdir -p "$WORK/http-root" /run/sshd 2>/dev/null || true
chmod 700 "$WORK"

if [ ! -f "$WORK/host_key" ]; then
    ssh-keygen -q -t ed25519 -N '' -f "$WORK/host_key"
fi
if [ ! -f "$WORK/client_key" ]; then
    ssh-keygen -q -t ed25519 -N '' -f "$WORK/client_key"
fi
cp "$WORK/client_key.pub" "$WORK/authorized_keys"
chmod 600 "$WORK/authorized_keys" "$WORK/client_key" "$WORK/host_key"

cat > "$WORK/sshd_config" <<EOF
Port $SSH_PORT
ListenAddress 127.0.0.1
HostKey $WORK/host_key
PidFile $WORK/sshd.pid
AuthorizedKeysFile $WORK/authorized_keys
PermitRootLogin yes
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
AllowTcpForwarding yes
LogLevel ERROR
EOF

SSHD_BIN="/usr/sbin/sshd"
[ -x "$SSHD_BIN" ] || SSHD_BIN="$(command -v sshd)"
"$SSHD_BIN" -f "$WORK/sshd_config" -E "$WORK/sshd.log"
SSHD_PID="$(cat "$WORK/sshd.pid")"

printf '<!doctype html><title>ok</title><h1>Porty e2e</h1>\n' > "$WORK/http-root/index.html"
python3 -m http.server "$REMOTE_PORT" --bind 127.0.0.1 --directory "$WORK/http-root" \
    >"$WORK/http.log" 2>&1 &
HTTP_PID=$!

cleanup() {
    kill "$HTTP_PID" 2>/dev/null || true
    kill "$SSHD_PID" 2>/dev/null || true
}
trap cleanup EXIT

sleep 1
cd "$ROOT"
PW_E2E_KEY="$WORK/client_key" \
PW_E2E_SSH_PORT="$SSH_PORT" \
PW_E2E_REMOTE_PORT="$REMOTE_PORT" \
PW_E2E_USER="$SSH_USER" \
    cargo run -p pw-core --example e2e
