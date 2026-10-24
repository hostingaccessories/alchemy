#!/usr/bin/env bash
# Release build smoke test: build the web UI and release binaries, run
# alchemyd on a throwaway socket, port, and state directory, and check the
# daemon, embedded UI, CLI, and helper end to end.
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
state_dir="${tmp}/state"
daemon_pid=""
events_pid=""
watch_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]] && kill -0 "$daemon_pid" 2>/dev/null; then
    kill -KILL "$daemon_pid" 2>/dev/null || true
  fi
  if [[ -n "$events_pid" ]] && kill -0 "$events_pid" 2>/dev/null; then
    kill -KILL "$events_pid" 2>/dev/null || true
  fi
  if [[ -n "$watch_pid" ]] && kill -0 "$watch_pid" 2>/dev/null; then
    kill -KILL "$watch_pid" 2>/dev/null || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

log "Starting alchemyd on ${base}"
"${bin}/alchemyd" --listen "127.0.0.1:${port}" --socket "$socket" \
  --state-dir "$state_dir" >"${tmp}/alchemyd.log" 2>&1 &
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

log "Checking the state database"
db="${state_dir}/alchemy.db"
[[ -f "$db" ]] || fail "alchemyd did not create ${db}"
db_mode="$(stat -c '%a' "$db" 2>/dev/null || stat -f '%Lp' "$db")"
dir_mode="$(stat -c '%a' "$state_dir" 2>/dev/null || stat -f '%Lp' "$state_dir")"
[[ "$db_mode" == 600 ]] || fail "${db} has mode ${db_mode}, expected 600"
[[ "$dir_mode" == 700 ]] || fail "${state_dir} has mode ${dir_mode}, expected 700"

log "Checking /api/health"
health="$(curl -fsS "${base}/api/health")"
[[ "$health" == *'"ok"'* ]] || fail "unexpected health response: ${health}"

log "Checking /api/engine"
engine="$(curl -fsS "${base}/api/engine")"
[[ "$engine" == *'"state":'* ]] || fail "unexpected engine response: ${engine}"

log "Checking a rebinding Host and a cross-origin POST are refused"
rebind_code="$(curl -sS -o "${tmp}/rebind.json" -w '%{http_code}' \
  -H 'Host: evil.example' "${base}/api/health")" || fail "rebinding GET failed"
[[ "$rebind_code" == 403 ]] || fail "rebinding Host returned ${rebind_code}, expected 403"
grep -q '"code":"forbidden"' "${tmp}/rebind.json" \
  || fail "rebinding Host was not a JSON 403: $(cat "${tmp}/rebind.json")"
form_code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H 'Origin: https://evil.example' -d 'a=b' "${base}/api/health")" \
  || fail "cross-origin POST failed"
[[ "$form_code" == 403 ]] || fail "cross-origin POST returned ${form_code}, expected 403"

log "Checking /api/health on the socket"
sock_health="$(curl -fsS --unix-socket "$socket" http://localhost/api/health)" \
  || fail "GET /api/health on ${socket} failed"
[[ "$sock_health" == *'"ok"'* ]] || fail "unexpected socket health response: ${sock_health}"

log "Checking / on the socket is a JSON 404"
sock_root_code="$(curl -sS -o "${tmp}/sock-root.json" -w '%{http_code}' \
  --unix-socket "$socket" http://localhost/)" || fail "GET / on ${socket} failed"
[[ "$sock_root_code" == 404 ]] || fail "GET / on the socket returned ${sock_root_code}, expected 404"
grep -q '"code":"not_found"' "${tmp}/sock-root.json" \
  || fail "GET / on the socket was not a JSON 404: $(cat "${tmp}/sock-root.json")"

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

log "Checking metrics on the socket"
for _ in $(seq 1 50); do
  metrics="$(curl -fsS --unix-socket "$socket" http://localhost/api/metrics)" \
    || fail "GET /api/metrics on ${socket} failed"
  [[ "$metrics" == *'"memory_total":'* ]] && break
  sleep 0.2
done
[[ "$metrics" == *'"memory_total":'* ]] || fail "no machine metrics: ${metrics}"
history="$(curl -fsS --unix-socket "$socket" 'http://localhost/api/metrics/history?range=1h')" \
  || fail "GET /api/metrics/history on ${socket} failed"
[[ "$history" == *'"points":[{"t":'* ]] || fail "no stored metrics point: ${history}"
"${bin}/alchemy" --socket "$socket" machine stats | grep -q '^Memory ' \
  || fail "alchemy machine stats did not show memory"

log "Checking operations on the socket"
operations="$(curl -fsS --unix-socket "$socket" http://localhost/api/operations)" \
  || fail "GET /api/operations on ${socket} failed"
[[ "$operations" == '{"operations":[]}' ]] || fail "unexpected operations: ${operations}"
refused="$(curl -sS --unix-socket "$socket" -o /dev/null -w '%{http_code}' -X POST \
  -H 'Content-Type: application/json' -d '{"action":"up"}' \
  http://localhost/api/apps/smoke-missing/actions)"
[[ "$refused" == 404 ]] || fail "an action on a missing app answered ${refused}, not 404"

log "Checking app discovery in a throwaway app home"
apps_dir="${tmp}/apps"
mkdir -p "${apps_dir}/Not_An_App"
printf 'services: {}\n' >"${apps_dir}/Not_An_App/compose.yaml"
"${bin}/alchemy" --socket "$socket" settings set app-home "$apps_dir" >/dev/null \
  || fail "alchemy settings set app-home failed"
for _ in $(seq 1 50); do
  apps="$("${bin}/alchemy" --socket "$socket" --json app list)" \
    || fail "alchemy app list failed"
  [[ "$apps" == *'"reason":"invalid_name"'* ]] && break
  sleep 0.2
done
[[ "$apps" == *"\"path\":\"${apps_dir}\""* ]] || fail "app list does not search ${apps_dir}: ${apps}"
[[ "$apps" == *'"apps":[]'* ]] || fail "an empty app home listed apps: ${apps}"
[[ "$apps" == *'"reason":"invalid_name"'* ]] || fail "Not_An_App was not reported as skipped: ${apps}"

log "Checking embedded web UI"
index="$(curl -fsS "${base}/")" || fail "GET / failed (web UI not embedded?)"
[[ "$index" == *'id="root"'* ]] || fail "GET / did not return the web UI"
deep="$(curl -fsS "${base}/machines")" || fail "GET /machines failed (no index.html fallback?)"
[[ "$deep" == *'id="root"'* ]] || fail "GET /machines did not return the web UI"
theme_type="$(curl -fsS -o /dev/null -w '%{content_type}' "${base}/theme.js")" \
  || fail "GET /theme.js failed (pre-paint theme script not embedded?)"
[[ "$theme_type" == *javascript* ]] || fail "GET /theme.js served as ${theme_type}"

log "Checking alchemy status"
cli_version="$("${bin}/alchemy" --version | awk '{print $2}')"
status="$("${bin}/alchemy" --socket "$socket" --json status)"
[[ "$status" == *"\"daemon\":\"${cli_version}\""* ]] \
  || fail "status did not report daemon ${cli_version}: ${status}"

log "Following alchemy status --watch"
"${bin}/alchemy" --socket "$socket" status --watch \
  >"${tmp}/watch.txt" 2>"${tmp}/watch.err" &
watch_pid=$!
for _ in $(seq 1 50); do
  [[ "$(wc -l <"${tmp}/watch.txt")" -ge 2 ]] && break
  sleep 0.2
done
[[ "$(wc -l <"${tmp}/watch.txt")" -ge 2 ]] \
  || fail "status --watch printed no engine line: $(cat "${tmp}/watch.txt" "${tmp}/watch.err")"
[[ "$(head -n 1 "${tmp}/watch.txt")" == "alchemyd ${cli_version} (protocol "*") at ${socket}" ]] \
  || fail "status --watch did not report daemon ${cli_version}: $(head -n 1 "${tmp}/watch.txt")"
kill -0 "$watch_pid" 2>/dev/null || fail "status --watch exited early: $(cat "${tmp}/watch.err")"

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

log "Checking alchemy status --watch ends when alchemyd stops"
set +e
wait "$watch_pid"
code=$?
set -e
watch_pid=""
[[ "$code" -eq 69 ]] || fail "status --watch exited ${code}, expected 69: $(cat "${tmp}/watch.err")"
grep -q 'alchemyd closed the connection' "${tmp}/watch.err" \
  || fail "status --watch did not report the disconnect: $(cat "${tmp}/watch.err")"

log "Checking alchemyd refuses a corrupt state database"
bad_state="${tmp}/bad-state"
mkdir -p "$bad_state"
head -c 8192 /dev/zero | tr '\0' 'Z' >"${bad_state}/alchemy.db"
"${bin}/alchemyd" --listen "127.0.0.1:${port}" --socket "${tmp}/bad.sock" \
  --state-dir "$bad_state" >"${tmp}/bad.log" 2>&1 &
daemon_pid=$!
for _ in $(seq 1 50); do
  kill -0 "$daemon_pid" 2>/dev/null || break
  sleep 0.2
done
kill -0 "$daemon_pid" 2>/dev/null && fail "alchemyd started on a corrupt state database"
set +e
wait "$daemon_pid"
code=$?
set -e
daemon_pid=""
[[ "$code" -ne 0 ]] || fail "alchemyd exited 0 on a corrupt state database"
grep -q 'corrupt or not a SQLite database' "${tmp}/bad.log" \
  || fail "alchemyd did not explain the corrupt database: $(cat "${tmp}/bad.log")"
[[ ! -e "${tmp}/bad.sock" ]] || fail "alchemyd bound its socket before refusing the database"

log "Release smoke test passed"
