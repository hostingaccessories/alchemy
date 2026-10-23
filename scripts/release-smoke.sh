#!/usr/bin/env bash
# Release build smoke test: build the web UI and release binaries, run
# alchemyd on a throwaway socket and port, and check the daemon, embedded
# UI, CLI, and helper end to end.
#
# Usage: scripts/release-smoke.sh
#   SMOKE_PORT  HTTP port for the test daemon (default 17240)
#   SMOKE_SKIP_BUILD=1  reuse existing dist/ and target/release binaries

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
port="${SMOKE_PORT:-17240}"
base="http://127.0.0.1:${port}"
bin="${root}/target/release"

log() { printf '==> %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

cd "$root"

if [[ "${SMOKE_SKIP_BUILD:-}" != 1 ]]; then
  log "Building web UI"
  vp run alchemy-web#build
  log "Building release binaries"
  cargo build --release --locked --workspace
fi

for exe in alchemyd alchemy alchemy-helper; do
  [[ -x "${bin}/${exe}" ]] || fail "missing ${bin}/${exe}"
done

tmp="$(mktemp -d)"
socket="${tmp}/alchemyd.sock"
daemon_pid=""
events_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]] && kill -0 "$daemon_pid" 2>/dev/null; then
    kill -KILL "$daemon_pid" 2>/dev/null || true
  fi
  if [[ -n "$events_pid" ]] && kill -0 "$events_pid" 2>/dev/null; then
    kill -KILL "$events_pid" 2>/dev/null || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

log "Starting alchemyd on ${base}"
"${bin}/alchemyd" --listen "127.0.0.1:${port}" --socket "$socket" \
  >"${tmp}/alchemyd.log" 2>&1 &
daemon_pid=$!

ready=""
for _ in $(seq 1 50); do
  if curl -fsS "${base}/api/health" >/dev/null 2>&1; then
    ready=1
    break
  fi
  kill -0 "$daemon_pid" 2>/dev/null || break
  sleep 0.2
done
if [[ -z "$ready" ]]; then
  cat "${tmp}/alchemyd.log" >&2
  fail "alchemyd did not become healthy"
fi

log "Checking /api/health"
health="$(curl -fsS "${base}/api/health")"
[[ "$health" == *'"ok"'* ]] || fail "unexpected health response: ${health}"

log "Checking /api/engine"
engine="$(curl -fsS "${base}/api/engine")"
[[ "$engine" == *'"state":'* ]] || fail "unexpected engine response: ${engine}"

log "Opening /api/events"
curl -fsSN -H 'Origin: tauri://localhost' "${base}/api/events" \
  >"${tmp}/events.txt" 2>/dev/null &
events_pid=$!
for _ in $(seq 1 50); do
  grep -q '^event: engine' "${tmp}/events.txt" && break
  sleep 0.2
done
grep -q '^event: engine' "${tmp}/events.txt" \
  || fail "no engine event on /api/events: $(cat "${tmp}/events.txt")"

log "Checking embedded web UI"
index="$(curl -fsS "${base}/")" || fail "GET / failed (web UI not embedded?)"
[[ "$index" == *'id="root"'* ]] || fail "GET / did not return the web UI"

log "Checking alchemy status"
cli_version="$("${bin}/alchemy" --version | awk '{print $2}')"
status="$("${bin}/alchemy" --socket "$socket" --json status)"
[[ "$status" == *"\"daemon\":\"${cli_version}\""* ]] \
  || fail "status did not report daemon ${cli_version}: ${status}"

log "Checking alchemy-helper refuses non-root"
if [[ "$(id -u)" -ne 0 ]]; then
  set +e
  "${bin}/alchemy-helper" check 2>/dev/null
  code=$?
  set -e
  [[ "$code" -eq 77 ]] || fail "alchemy-helper check exited ${code}, expected 77"
else
  log "Running as root; skipping"
fi

log "Stopping alchemyd"
kill -TERM "$daemon_pid"
set +e
wait "$daemon_pid"
code=$?
set -e
daemon_pid=""
[[ "$code" -eq 0 ]] || { cat "${tmp}/alchemyd.log" >&2; fail "alchemyd exited ${code}"; }
[[ ! -e "$socket" ]] || fail "socket ${socket} left behind after shutdown"
set +e
wait "$events_pid"
set -e
events_pid=""

log "Release smoke test passed"
