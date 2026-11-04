#!/usr/bin/env bash
#
# Publish the terminal through a VPS with a reverse SSH tunnel.
#
# The VPS is never modified: the script only runs `ssh -R`, so the server that
# runs on THIS machine becomes reachable at
#
#     http://<VPS_HOST>:<REMOTE_PORT>/
#
# No address is baked in. Configure once in `.vps-tunnel.env` (gitignored) or
# with environment variables:
#
#     VPS_HOST=...        host or ~/.ssh/config alias (required)
#     VPS_USER=root       empty to keep the target as written
#     VPS_PORT=22         SSH port
#     LOCAL_PORT=19010    simccxt-server on this machine
#     REMOTE_PORT=19010   port opened on the VPS
#     BIND=0.0.0.0        127.0.0.1 if sshd has no `GatewayPorts yes`
#     PUBLIC_HOST=...     address to print when VPS_HOST is an ssh alias
#     VPS_TARGET=...      full ssh target, overrides user/host/port
#
# Usage:
#     scripts/vps-tunnel.sh                 # build if needed, serve, tunnel
#     scripts/vps-tunnel.sh my-vps          # host from an ssh alias
#     scripts/vps-tunnel.sh --status
#     scripts/vps-tunnel.sh --stop
#
# Auth is key-based by default (BatchMode): run
#     ssh-copy-id "$VPS_USER@$VPS_HOST"
# once. A configured SSH_ASKPASS helper is used when present.
#
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR="${RUN_DIR:-$ROOT/.run}"
ENV_FILE="${ENV_FILE:-$ROOT/.vps-tunnel.env}"
SERVER_BIN="${SERVER_BIN:-$ROOT/target/release/simccxt-server}"

LOCAL_PORT="${LOCAL_PORT:-19010}"
REMOTE_PORT="${REMOTE_PORT:-}"
VPS_USER="${VPS_USER:-root}"
VPS_PORT="${VPS_PORT:-22}"
BIND="${BIND:-0.0.0.0}"
VPS_HOST="${VPS_HOST:-}"
PUBLIC_HOST="${PUBLIC_HOST:-}"
VPS_TARGET="${VPS_TARGET:-}"

MODE=run
DO_BUILD=auto

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
Publish the terminal through a VPS with a reverse SSH tunnel.

The VPS is never modified: the script only runs `ssh -R`, so the server on
THIS machine becomes reachable at http://<VPS_HOST>:<REMOTE_PORT>/.

Configure once in .vps-tunnel.env (gitignored) or with environment variables:
  VPS_HOST      host or ~/.ssh/config alias (required)
  VPS_USER      ssh login, default root (empty keeps the target as written)
  VPS_PORT      ssh port, default 22
  LOCAL_PORT    simccxt-server here, default 19010
  REMOTE_PORT   port opened on the VPS, default LOCAL_PORT
  BIND          0.0.0.0 (GatewayPorts yes) or 127.0.0.1
  PUBLIC_HOST   address to print when VPS_HOST is an ~/.ssh alias
  VPS_TARGET    full ssh target, overrides user/host/port

Usage:
  scripts/vps-tunnel.sh [VPS_HOST]   build if needed, serve, open the tunnel
  scripts/vps-tunnel.sh --status     show server, tunnel and VPS reachability
  scripts/vps-tunnel.sh --stop       stop the tunnel and the server it started

Flags: --host --user --ssh-port --local-port --remote-port --bind
       --build --no-build --help

Auth is key-based by default (BatchMode); run ssh-copy-id once. A configured
SSH_ASKPASS helper is used instead when present.
EOF
}

if [ -f "$ENV_FILE" ]; then
  # shellcheck disable=SC1090
  set -a
  . "$ENV_FILE"
  set +a
fi

while [ $# -gt 0 ]; do
  case "$1" in
    --status) MODE=status; shift ;;
    --stop) MODE=stop; shift ;;
    --build) DO_BUILD=yes; shift ;;
    --no-build) DO_BUILD=no; shift ;;
    --host) VPS_HOST="${2:-}"; shift 2 ;;
    --user) VPS_USER="${2:-}"; shift 2 ;;
    --ssh-port) VPS_PORT="${2:-}"; shift 2 ;;
    --local-port) LOCAL_PORT="${2:-}"; shift 2 ;;
    --remote-port) REMOTE_PORT="${2:-}"; shift 2 ;;
    --bind) BIND="${2:-}"; shift 2 ;;
    --public-host) PUBLIC_HOST="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    -*) die "unknown flag: $1 (try --help)" ;;
    *) VPS_HOST="$1"; shift ;;
  esac
done

# REMOTE_PORT follows LOCAL_PORT unless it was set explicitly anywhere.
REMOTE_PORT="${REMOTE_PORT:-$LOCAL_PORT}"
PUBLIC_HOST="${PUBLIC_HOST:-$VPS_HOST}"

TARGET=""
SSH_PORT_ARGS=()
# Resolved lazily: --stop and --status work without a host.
resolve_target() {
  if [ -n "$VPS_TARGET" ]; then
    TARGET="$VPS_TARGET"
    SSH_PORT_ARGS=()
  else
    [ -n "$VPS_HOST" ] || die "VPS_HOST is empty: export VPS_HOST=... or fill $ENV_FILE"
    if [ -n "$VPS_USER" ]; then TARGET="$VPS_USER@$VPS_HOST"; else TARGET="$VPS_HOST"; fi
    SSH_PORT_ARGS=(-p "$VPS_PORT")
  fi
}

SSH_OPTS=(
  -o ConnectTimeout=10
  -o ServerAliveInterval=20
  -o ServerAliveCountMax=3
  -o StrictHostKeyChecking=accept-new
)
# Key auth by default (never hangs waiting for a password). If an askpass
# helper is configured, let ssh use it instead.
if [ -n "${SSH_ASKPASS:-}" ]; then
  SSH_OPTS+=(-o BatchMode=no -o NumberOfPasswordPrompts=1)
else
  SSH_OPTS+=(-o BatchMode=yes)
fi

SERVER_PID_FILE="$RUN_DIR/server.pid"
TUNNEL_PID_FILE="$RUN_DIR/tunnel.pid"
SERVER_LOG="$RUN_DIR/server.log"
TUNNEL_LOG="$RUN_DIR/tunnel.log"

mkdir -p "$RUN_DIR"

port_open() {
  local port="$1"
  # bash's /dev/tcp needs no extra tool; nc covers builds without it.
  (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && return 0
  command -v nc >/dev/null 2>&1 && nc -z 127.0.0.1 "$port" 2>/dev/null
}

wait_for_port() {
  local port="$1" tries="${2:-60}"
  for ((i = 0; i < tries; i++)); do
    port_open "$port" && return 0
    sleep 0.5
  done
  return 1
}

pid_alive() {
  [ -f "$1" ] && kill -0 "$(cat "$1")" 2>/dev/null
}

stop_pidfile() {
  local file="$1" label="$2" pid
  [ -f "$file" ] || return 0
  pid="$(cat "$file")"
  if kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    local n=0
    while kill -0 "$pid" 2>/dev/null && [ "$n" -lt 20 ]; do
      sleep 0.1
      n=$((n + 1))
    done
    kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null || true
    say "stopped $label (pid $pid)"
  fi
  rm -f "$file"
}

remote_check() {
  ssh "${SSH_OPTS[@]}" "${SSH_PORT_ARGS[@]}" "$TARGET" \
    "if command -v curl >/dev/null 2>&1; then
       curl -fsS -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1:$REMOTE_PORT/;
     else
       ss -ltn 2>/dev/null | grep -q ':$REMOTE_PORT ' && echo listening;
     fi" 2>/dev/null || true
}

# A loopback-only tunnel is still useful, but the address the user can open
# differs: an SSH local forward instead of the VPS public IP.
print_url() {
  case "$BIND" in
    127.0.0.1|localhost|::1)
      say "  ssh -L $REMOTE_PORT:127.0.0.1:$REMOTE_PORT $TARGET   # then http://127.0.0.1:$REMOTE_PORT/"
      ;;
    *)
      say "  http://${PUBLIC_HOST:-$TARGET}:$REMOTE_PORT/"
      ;;
  esac
}

build_if_needed() {
  [ "$DO_BUILD" = no ] && return 0
  local need=0
  [ -x "$SERVER_BIN" ] || need=1
  [ -f "$ROOT/frontend/dist/index.html" ] || need=1
  [ "$DO_BUILD" = yes ] && need=1
  [ "$need" = 0 ] && return 0

  say "building the server and the terminal (first run only)…"
  command -v cargo >/dev/null || die "cargo not found — install Rust (https://rustup.rs)"
  (cd "$ROOT" && cargo build --release -p simccxt-server)

  if [ ! -f "$ROOT/frontend/dist/index.html" ]; then
    command -v npm >/dev/null || die "npm not found — install Node 18+"
    (cd "$ROOT/frontend" && { [ -d node_modules ] || npm install; } && npm run build)
  fi
}

start_server() {
  if port_open "$LOCAL_PORT"; then
    say "server already listening on 127.0.0.1:$LOCAL_PORT"
    return 0
  fi
  [ -x "$SERVER_BIN" ] || die "missing $SERVER_BIN (run with --build)"
  say "starting simccxt-server on 127.0.0.1:$LOCAL_PORT"
  (
    cd "$ROOT"
    nohup "$SERVER_BIN" --port "$LOCAL_PORT" --host 127.0.0.1 \
      >"$SERVER_LOG" 2>&1 &
    echo $! >"$SERVER_PID_FILE"
  )
  if ! wait_for_port "$LOCAL_PORT"; then
    say "server did not start; last log lines:"
    tail -n 20 "$SERVER_LOG" >&2 || true
    exit 1
  fi
}

start_tunnel() {
  : >"$TUNNEL_LOG"
  ssh -N "${SSH_OPTS[@]}" "${SSH_PORT_ARGS[@]}" \
    -o ExitOnForwardFailure=yes \
    -R "$BIND:$REMOTE_PORT:127.0.0.1:$LOCAL_PORT" \
    "$TARGET" >>"$TUNNEL_LOG" 2>&1 &
  local pid=$!
  echo "$pid" >"$TUNNEL_PID_FILE"
  sleep 1
  if ! kill -0 "$pid" 2>/dev/null; then
    say "the tunnel could not be opened:" >&2
    tail -n 20 "$TUNNEL_LOG" >&2 || true
    rm -f "$TUNNEL_PID_FILE"
    die "check key auth (ssh-copy-id), the remote port, and GatewayPorts on the VPS"
  fi
  say "tunnel pid $pid → $TARGET"
}

cleanup() {
  stop_pidfile "$TUNNEL_PID_FILE" "tunnel"
}

# Installed before anything is started, so Ctrl-C during the build or the
# handshake cannot leave a tunnel behind.
trap 'cleanup; exit 130' INT TERM

case "$MODE" in
  stop)
    stop_pidfile "$TUNNEL_PID_FILE" "tunnel"
    if pid_alive "$SERVER_PID_FILE" &&
      ps -p "$(cat "$SERVER_PID_FILE")" -o cmd= 2>/dev/null | grep -q simccxt-server; then
      stop_pidfile "$SERVER_PID_FILE" "server"
    else
      rm -f "$SERVER_PID_FILE"
      say "server was not started by this script; leave it running"
    fi
    say "tunnel is down"
    exit 0
    ;;

  status)
    if port_open "$LOCAL_PORT"; then
      say "server: up on 127.0.0.1:$LOCAL_PORT"
    else
      say "server: down on 127.0.0.1:$LOCAL_PORT"
    fi
    if pid_alive "$TUNNEL_PID_FILE"; then
      say "tunnel: up (pid $(cat "$TUNNEL_PID_FILE")), remote port $REMOTE_PORT"
      if [ -n "$VPS_HOST" ] || [ -n "$VPS_TARGET" ]; then
        resolve_target
        code="$(remote_check)"
        if [ -n "$code" ]; then
          say "on the VPS: http://127.0.0.1:$REMOTE_PORT/ → $code"
        else
          say "on the VPS: not reachable yet (GatewayPorts / firewall?)"
        fi
      fi
    else
      say "tunnel: down"
    fi
    if [ -n "$VPS_HOST" ] || [ -n "$VPS_TARGET" ]; then
      say "open:"
      print_url
    fi
    exit 0
    ;;
esac

resolve_target
build_if_needed
start_server
start_tunnel

code="$(remote_check)"
if [ -n "$code" ]; then
  say "checked from the VPS: $code"
else
  say "warning: the VPS could not reach 127.0.0.1:$REMOTE_PORT yet"
  say "         (sshd needs 'GatewayPorts yes' for BIND=$BIND; check the firewall)"
fi

say ""
say "open:"
print_url
say ""
say "Ctrl-C closes the tunnel; the server keeps running."
say "Stop both: $0 --stop"
say "Logs: $SERVER_LOG, $TUNNEL_LOG"

# Keep the script (and therefore the tunnel) alive until ssh exits or Ctrl-C.
wait "$(cat "$TUNNEL_PID_FILE")" || say "tunnel closed"
rm -f "$TUNNEL_PID_FILE"
