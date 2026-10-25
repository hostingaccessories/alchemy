#!/usr/bin/env bash
# Recovery test (Linux only): run alchemyd against the real Docker engine
# with four apps up, stop one in Alchemy and one by hand, kill alchemyd,
# stop the rest the way a reboot would, break one, and start alchemyd
# again. Then check that the app that was running came back with its
# reason, that the stop intent and the stop by hand stayed down, and that
# the broken app was tried once and reported as failed.
#
# It brings up and removes Compose projects named recovery-*, and alchemyd
# creates the `alchemy` network if it is missing, so it refuses to run
# unless CI=true or RECOVERY_CONFIRM=1.
#
# Usage: scripts/recovery.sh
#   RECOVERY_PORT          HTTP port for the test daemon (default 17243)
#   RECOVERY_SKIP_BUILD=1  reuse existing target/release binaries

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
port="${RECOVERY_PORT:-17243}"
bin="${CARGO_TARGET_DIR:-${root}/target}/release"
image="busybox:1.37.0"
missing_image="alchemy-recovery-missing:0"
back="recovery-back"
intent="recovery-intent"
hand="recovery-hand"
broken="recovery-broken"
apps=("$back" "$intent" "$hand" "$broken")

log() { printf '==> %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

[[ "$(uname -s)" == Linux ]] || fail "the recovery test needs Linux"
[[ "${CI:-}" == true || "${RECOVERY_CONFIRM:-}" == 1 ]] \
  || fail "this runs Compose projects named recovery-*; set RECOVERY_CONFIRM=1 to run it"
for tool in jq curl docker; do
  command -v "$tool" >/dev/null || fail "missing ${tool}"
done
docker info >/dev/null 2>&1 || fail "Docker isn't running or this user can't reach it"

cd "$root"

if [[ "${RECOVERY_SKIP_BUILD:-}" != 1 ]]; then
  log "Building release binaries"
  cargo build --release --locked -p alchemyd -p alchemy
fi
for exe in alchemyd alchemy; do
  [[ -x "${bin}/${exe}" ]] || fail "missing ${bin}/${exe}"
done

tmp="$(mktemp -d)"
socket="${tmp}/a.sock"
state_dir="${tmp}/state"
apps_dir="${tmp}/apps"
daemon_pid=""

alchemy() { "${bin}/alchemy" --socket "$socket" "$@"; }
api() { curl -fsS --unix-socket "$socket" "http://alchemyd$1"; }

# Debug logging for the reconcile task shows each change to the apps
# last seen running.
start_daemon() {
  ALCHEMY_LOG="info,alchemyd::reconcile=debug" "${bin}/alchemyd" \
    --listen "127.0.0.1:${port}" --socket "$socket" \
    --state-dir "$state_dir" >>"${tmp}/alchemyd.log" 2>&1 &
  daemon_pid=$!
}

stop_daemon() {
  if [[ -n "$daemon_pid" ]] && kill -0 "$daemon_pid" 2>/dev/null; then
    kill -TERM "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  daemon_pid=""
}

compose() {
  local app="$1"
  shift
  docker compose --project-directory "${apps_dir}/${app}" "$@"
}

cleanup() {
  stop_daemon
  local app
  for app in "${apps[@]}"; do
    if [[ -f "${apps_dir}/${app}/compose.yaml" ]]; then
      compose "$app" down --timeout 1 >/dev/null 2>&1 || true
    fi
  done
  rm -rf "$tmp"
}
trap cleanup EXIT

die() {
  cat "${tmp}/alchemyd.log" >&2 || true
  fail "$@"
}

# wait_for WHAT JQ PATH: poll PATH on the socket until JQ holds, and
# leave the last answer in $answer.
answer=""
wait_for() {
  local what="$1" filter="$2" path="$3"
  for _ in $(seq 1 240); do
    answer="$(api "$path" 2>/dev/null)" || answer=""
    if [[ -n "$answer" ]] && jq -e "$filter" <<<"$answer" >/dev/null 2>&1; then
      return 0
    fi
    kill -0 "$daemon_pid" 2>/dev/null || die "alchemyd exited while waiting for ${what}"
    sleep 0.5
  done
  die "timed out waiting for ${what}: ${answer}"
}

# wait_for_log WHAT PATTERN: wait until alchemyd's log has a line
# matching PATTERN. A stop by hand counts only after STOP_GRACE (20s).
wait_for_log() {
  local what="$1" pattern="$2"
  for _ in $(seq 1 120); do
    grep -qE "$pattern" "${tmp}/alchemyd.log" && return 0
    kill -0 "$daemon_pid" 2>/dev/null || die "alchemyd exited while waiting for ${what}"
    sleep 0.5
  done
  die "timed out waiting for ${what}"
}

running() {
  [[ -n "$(compose "$1" ps -q --status running 2>/dev/null)" ]]
}

write_app() {
  local app="$1" app_image="${2:-$image}"
  mkdir -p "${apps_dir}/${app}"
  cat >"${apps_dir}/${app}/compose.yaml" <<EOF
services:
  main:
    image: ${app_image}
    pull_policy: never
    init: true
    command: ["sleep", "infinity"]
EOF
}

log "Writing apps ${apps[*]}"
for app in "${apps[@]}"; do
  write_app "$app"
done
docker pull --quiet "$image" >/dev/null
if docker image inspect "$missing_image" >/dev/null 2>&1; then
  fail "the image ${missing_image} exists; this test needs it missing"
fi

log "Starting alchemyd"
start_daemon
wait_for "the engine" '.state == "ready"' /api/engine
alchemy settings set app-home "$apps_dir" >/dev/null || die "alchemy settings set app-home failed"
wait_for "Compose to read the apps" \
  "[.apps[] | select(.name | startswith(\"recovery-\")) | select(.project != null)] | length == ${#apps[@]}" \
  /api/apps

for app in "${apps[@]}"; do
  log "Bringing up ${app}"
  alchemy app up "$app" >"${tmp}/up.txt" 2>&1 || { cat "${tmp}/up.txt" >&2; die "alchemy app up ${app} failed"; }
  wait_for_log "${app} to be seen running" "app=${app} running=true"
done

log "Stopping ${intent} in Alchemy, and ${hand} by hand"
alchemy app stop "$intent" >/dev/null 2>&1 || die "alchemy app stop ${intent} failed"
compose "$hand" stop --timeout 1 >/dev/null 2>&1 || die "docker compose stop ${hand} failed"
wait_for_log "the stop by hand to count" "app=${hand} running=false"

log "Killing alchemyd, then stopping ${back} and ${broken} as a reboot would, and breaking ${broken}"
kill -KILL "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
compose "$back" stop --timeout 1 >/dev/null 2>&1 || fail "docker compose stop ${back} failed"
compose "$broken" stop --timeout 1 >/dev/null 2>&1 || fail "docker compose stop ${broken} failed"
write_app "$broken" "$missing_image"
for app in "${apps[@]}"; do
  running "$app" && fail "${app} is still running before alchemyd starts again"
done

log "Starting alchemyd again"
start_daemon
wait_for "the reconcile after the restart to finish" \
  '.last.trigger == "daemon_start" and (.last.apps | all(.outcome != "starting"))' /api/reconcile
reconcile="$answer"
printf '%s\n' "$reconcile" >"${tmp}/reconcile.json"
log "Reconcile: $(jq -c '[.last.apps[] | select(.app | startswith("recovery-")) | {app, outcome, reason}]' <<<"$reconcile")"

entry() { jq -c --arg app "$1" '.last.apps[] | select(.app == $app)' <<<"$reconcile"; }

reason="was running before Alchemy restarted"
jq -e --arg reason "$reason" '.last.reason == $reason' <<<"$reconcile" >/dev/null \
  || die "the reconcile's reason is not \"${reason}\": $(jq -c .last.reason <<<"$reconcile")"

got="$(entry "$back")"
jq -e '.outcome == "started" and .operation != null' <<<"$got" >/dev/null \
  || die "${back} did not come back: ${got}"
op="$(api "/api/operations/$(jq -r .operation <<<"$got")")" || die "cannot read ${back}'s operation"
jq -e --arg reason "$reason" '.action == "up" and .state == "succeeded" and .reason == $reason' <<<"$op" >/dev/null \
  || die "${back}'s operation is not a successful up with the reason \"${reason}\": ${op}"
running "$back" || die "${back} is not running after it came back"

got="$(entry "$intent")"
jq -e '.outcome == "skipped" and .reason == "stop_intent" and .message == "you stopped it in Alchemy"' <<<"$got" >/dev/null \
  || die "${intent} was not skipped for its stop intent: ${got}"

got="$(entry "$hand")"
jq -e '.outcome == "skipped" and .reason == "not_running_before"' <<<"$got" >/dev/null \
  || die "${hand}, stopped by hand, was not skipped as not running before: ${got}"

got="$(entry "$broken")"
jq -e '.outcome == "failed" and (.message // "" | length > 0)' <<<"$got" >/dev/null \
  || die "${broken} was not reported as failed with a message: ${got}"
log "${broken}: $(jq -r .message <<<"$got")"
if [[ "$(jq -r '.operation // empty' <<<"$got")" != "" ]]; then
  op="$(api "/api/operations/$(jq -r .operation <<<"$got")")" || die "cannot read ${broken}'s operation"
  jq -e '.state == "failed" and (.error // "" | length > 0)' <<<"$op" >/dev/null \
    || die "${broken}'s operation did not fail with an error: ${op}"
fi

log "Checking nothing else starts and ${broken} is not retried"
sleep 5
for app in "$intent" "$hand" "$broken"; do
  running "$app" && die "${app} is running, but should have stayed down"
done
ups="$(api /api/operations | jq --arg app "$broken" '[.operations[] | select(.app == $app and .action == "up")] | length')" \
  || die "cannot list operations"
[[ "$ups" -eq 2 ]] || die "expected two up operations for ${broken} (the first, and the one try after the restart), got ${ups}"
api /api/reconcile | jq -e '.last.trigger == "daemon_start"' >/dev/null \
  || die "another reconcile ran after the restart: $(api /api/reconcile | jq -c .last)"

stop_daemon
log "Recovery test passed"
