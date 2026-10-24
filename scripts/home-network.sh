#!/usr/bin/env bash
# Home-network test (Linux only): for each simulated home network, put an
# address on a dummy interface, run alchemyd against the real Docker engine,
# and check that the `alchemy` network lands on a subnet clear of it and that
# the network warnings and their fix are right.
#
# It adds and removes a network interface (with sudo unless run as root) and
# removes the engine's `alchemy` network before and after each run, so it
# expects a fresh engine with only Docker's default networks, such as a CI
# runner. It refuses to run unless CI=true or HOME_NET_CONFIRM=1.
#
# Usage: scripts/home-network.sh
#   HOME_NET_PORT        HTTP port for the test daemon (default 17241)
#   HOME_NET_SKIP_BUILD=1  reuse existing target/release binaries

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
port="${HOME_NET_PORT:-17241}"
bin="${CARGO_TARGET_DIR:-${root}/target}/release"
link="alchemy-lan0"

log() { printf '==> %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

as_root() {
  if [[ "$(id -u)" -eq 0 ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

[[ "$(uname -s)" == Linux ]] || fail "the home-network test needs Linux"
[[ "${CI:-}" == true || "${HOME_NET_CONFIRM:-}" == 1 ]] \
  || fail "this changes network interfaces and removes the Docker network 'alchemy'; set HOME_NET_CONFIRM=1 to run it"
for tool in ip jq curl docker; do
  command -v "$tool" >/dev/null || fail "missing ${tool}"
done
docker info >/dev/null 2>&1 || fail "Docker isn't running or this user can't reach it"

cd "$root"

if [[ "${HOME_NET_SKIP_BUILD:-}" != 1 ]]; then
  log "Building release binaries"
  cargo build --release --locked -p alchemyd -p alchemy
fi
for exe in alchemyd alchemy; do
  [[ -x "${bin}/${exe}" ]] || fail "missing ${bin}/${exe}"
done

tmp="$(mktemp -d)"
daemon_pid=""

stop_daemon() {
  if [[ -n "$daemon_pid" ]] && kill -0 "$daemon_pid" 2>/dev/null; then
    kill -TERM "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  daemon_pid=""
}

reset() {
  stop_daemon
  if ip link show "$link" >/dev/null 2>&1; then
    as_root ip link del "$link"
  fi
  docker network rm alchemy >/dev/null 2>&1 || true
}

cleanup() {
  reset
  rm -rf "$tmp"
}
trap cleanup EXIT

ip_int() {
  local a b c d
  IFS=. read -r a b c d <<<"$1"
  echo $(((a << 24) | (b << 16) | (c << 8) | d))
}

# Whether two CIDR subnets share any address.
overlaps() {
  local a_ip="${1%/*}" a_len="${1#*/}" b_ip="${2%/*}" b_len="${2#*/}"
  local a_start b_start a_end b_end
  a_start=$(($(ip_int "$a_ip") & (0xFFFFFFFF << (32 - a_len)) & 0xFFFFFFFF))
  b_start=$(($(ip_int "$b_ip") & (0xFFFFFFFF << (32 - b_len)) & 0xFFFFFFFF))
  a_end=$((a_start + (1 << (32 - a_len)) - 1))
  b_end=$((b_start + (1 << (32 - b_len)) - 1))
  ((a_start <= b_end && b_start <= a_end))
}

# Where the fix says to put the settings, by engine flavor.
fix_lead() {
  case "$1" in
    docker_engine)
      printf '%s' "Add these settings to /etc/docker/daemon.json (create it if it doesn't exist), then restart Docker with \`sudo systemctl restart docker\`:"
      ;;
    docker_desktop)
      printf '%s' "In Docker Desktop, open Settings, then Docker Engine, add these settings to the JSON there, and select Apply & restart:"
      ;;
    *) printf '%s' "Add these settings to your Docker engine's daemon.json, then restart it:" ;;
  esac
}

# The fix every overlap here should suggest: move the pools to
# 10.200.0.0/16 and, when BIP is set, the default bridge to it.
expected_fix() {
  local lead="$1" bip="$2"
  printf '%s\n{\n' "$lead"
  if [[ -n "$bip" ]]; then
    printf '  "bip": "%s",\n' "$bip"
  fi
  printf '  "default-address-pools": [\n'
  printf '    { "base": "10.200.0.0/16", "size": 24 }\n'
  printf '  ]\n}\n'
  printf '%s' "Networks that already exist keep their subnets until they are removed and created again."
}

# check NAME ADDRESS LAN MOVES_BRIDGE WARNING...
#
# Each WARNING is `kind subnet`, in the order alchemyd reports them.
# MOVES_BRIDGE is 1 when the fix should also move the default bridge.
check() {
  local name="$1" address="$2" lan="$3" moves_bridge="$4"
  shift 4
  local expected=("$@")
  local dir="${tmp}/${name}"
  local socket="${dir}/a.sock"
  mkdir -p "$dir"

  log "${name}: home network ${lan} on ${link}"
  reset
  as_root ip link add "$link" type dummy
  as_root ip addr add "$address" dev "$link"
  as_root ip link set "$link" up

  "${bin}/alchemyd" --listen "127.0.0.1:${port}" --socket "$socket" \
    --state-dir "${dir}/state" >"${dir}/alchemyd.log" 2>&1 &
  daemon_pid=$!

  local report=""
  for _ in $(seq 1 120); do
    report="$(curl -fsS --unix-socket "$socket" http://alchemyd/api/network 2>/dev/null)" || report=""
    if [[ -n "$report" ]] \
      && jq -e '.engine_ready and .alchemy.state != "pending"' <<<"$report" >/dev/null; then
      break
    fi
    kill -0 "$daemon_pid" 2>/dev/null || break
    sleep 0.5
  done
  printf '%s\n' "$report" >"${dir}/network.json"
  if ! jq -e '.engine_ready and .alchemy.state != "pending"' <<<"$report" >/dev/null 2>&1; then
    cat "${dir}/alchemyd.log" >&2
    fail "${name}: alchemyd never finished checking Docker's networks: ${report}"
  fi

  jq -e --arg lan "$lan" '.lan | index($lan)' <<<"$report" >/dev/null \
    || fail "${name}: ${lan} is not among the LAN subnets: $(jq -c .lan <<<"$report")"
  [[ "$(jq -r .pools_source <<<"$report")" == defaults ]] \
    || fail "${name}: expected Docker's default pools, got $(jq -c '{pools_source, pools}' <<<"$report")"

  local state subnet docker_subnet
  state="$(jq -r .alchemy.state <<<"$report")"
  subnet="$(jq -r .alchemy.subnet <<<"$report")"
  [[ "$state" == ready ]] || fail "${name}: the alchemy network is ${state}: $(jq -c .alchemy <<<"$report")"
  docker_subnet="$(docker network inspect alchemy --format '{{range .IPAM.Config}}{{.Subnet}}{{end}}')" \
    || fail "${name}: alchemyd reported the alchemy network ready, but Docker has none"
  [[ "$docker_subnet" == "$subnet" ]] \
    || fail "${name}: alchemyd reports ${subnet}, Docker has ${docker_subnet}"
  [[ "$(docker network inspect alchemy --format '{{index .Labels "com.alchemy.managed"}}')" == true ]] \
    || fail "${name}: the alchemy network is not labelled com.alchemy.managed=true"
  local taken
  for taken in $(jq -r '(.lan + .routes + [.bridge // empty])[]' <<<"$report"); do
    if overlaps "$subnet" "$taken"; then
      fail "${name}: the alchemy network ${subnet} overlaps ${taken}"
    fi
  done
  if [[ "$(jq -r .bridge <<<"$report")" == 172.17.0.0/16 && "$(docker network ls -q | wc -l)" -eq 4 ]]; then
    [[ "$subnet" == 172.18.0.0/24 ]] \
      || fail "${name}: on a fresh engine the alchemy network should be 172.18.0.0/24, not ${subnet}"
  fi

  local got want
  got="$(jq -r '.warnings[] | "\(.kind) \(.subnet)"' <<<"$report")"
  want="$(printf '%s\n' "${expected[@]+"${expected[@]}"}" | sed '/^$/d')"
  [[ "$got" == "$want" ]] \
    || fail "${name}: expected warnings [${want//$'\n'/, }], got [${got//$'\n'/, }]: $(jq -c .warnings <<<"$report")"

  local flavor bip="" fix
  flavor="$(curl -fsS --unix-socket "$socket" http://alchemyd/api/engine | jq -r '.flavor // "other"')"
  if [[ "$moves_bridge" == 1 ]]; then
    bip="$(jq -r '[.warnings[].fix // empty | capture("\"bip\": \"(?<bip>[0-9./]+)\"") | .bip][0] // empty' <<<"$report")"
    [[ -n "$bip" ]] || fail "${name}: the fix does not move the default bridge: $(jq -c .warnings <<<"$report")"
    for taken in $(jq -r '(.lan + .routes + [.networks[].subnets[]])[]' <<<"$report"); do
      if overlaps "$bip" "$taken"; then
        fail "${name}: the fix moves the default bridge to ${bip}, which overlaps ${taken}"
      fi
    done
  fi
  fix="$(expected_fix "$(fix_lead "$flavor")" "$bip")"
  jq -e --arg fix "$fix" '[.warnings[] | select(.kind == "bridge_overlap" or .kind == "pool_overlap") | .fix == $fix] | all' \
    <<<"$report" >/dev/null \
    || fail "${name}: expected the fix ${fix}, got $(jq -r '.warnings[0].fix' <<<"$report")"

  local warning kind subnet_w message
  for warning in "${expected[@]+"${expected[@]}"}"; do
    kind="${warning% *}"
    subnet_w="${warning#* }"
    case "$kind" in
      bridge_overlap)
        message="Docker's default bridge network (${subnet_w}) overlaps your local network ${lan}, so containers on it can't reach devices on it."
        ;;
      pool_overlap)
        message="Docker's address pool ${subnet_w} overlaps your local network ${lan}, so a new network may be given addresses that hide devices on it."
        ;;
      *) fail "no expected message for ${kind}" ;;
    esac
    jq -e --arg kind "$kind" --arg message "$message" --arg lan "$lan" \
      '.warnings[] | select(.kind == $kind) | select(.message == $message and .host == "lan" and .host_subnet == $lan)' \
      <<<"$report" >/dev/null \
      || fail "${name}: ${kind} is not \"${message}\": $(jq -c .warnings <<<"$report")"
  done

  # alchemy status also warns when .local names can't be announced, which
  # depends on the runner's mDNS responder, not on the network under test.
  local mdns_warnings
  mdns_warnings="$("${bin}/alchemy" --socket "$socket" --json status \
    | jq -r 'if .network.mdns.state == "unavailable" then 1 else 0 end')" \
    || fail "${name}: alchemy status --json failed"
  local status
  status="$("${bin}/alchemy" --socket "$socket" status)" || fail "${name}: alchemy status failed"
  printf '%s\n' "$status" >"${dir}/status.txt"
  local count
  count="$(grep -c '^Warning: ' <<<"$status" || true)"
  [[ "$count" -eq "$((${#expected[@]} + mdns_warnings))" ]] \
    || fail "${name}: alchemy status printed ${count} warnings, expected ${#expected[@]} and ${mdns_warnings} for local names: ${status}"
  if [[ "${#expected[@]}" -gt 0 ]]; then
    grep -qxF "  Fix: $(head -n 1 <<<"$fix")" <<<"$status" \
      || fail "${name}: alchemy status did not print the fix: ${status}"
  fi
  if [[ "${#expected[@]}" -gt 1 ]]; then
    grep -qxF "  Fix: the same as above." <<<"$status" \
      || fail "${name}: alchemy status repeated the fix: ${status}"
  fi
  "${bin}/alchemy" --socket "$socket" --json status \
    | jq -e --argjson n "${#expected[@]}" '.network.warnings | length == $n' >/dev/null \
    || fail "${name}: alchemy status --json does not carry the network report"

  stop_daemon
}

check office 10.0.1.5/24 10.0.1.0/24 0
check docker-lan 172.17.0.5/16 172.17.0.0/16 1 \
  "bridge_overlap 172.17.0.0/16" "pool_overlap 172.17.0.0/16"
check home 192.168.1.5/16 192.168.0.0/16 0 \
  "pool_overlap 192.168.0.0/16"

log "Home-network test passed"
