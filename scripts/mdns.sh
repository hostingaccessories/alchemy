#!/usr/bin/env bash
# mDNS test (Linux only): run alchemyd against the real Docker engine and
# avahi-daemon, bring up an app, and check that <app>.local resolves to the
# LAN address and reaches the app by name; that a name another responder
# already answers for becomes an app problem; then stop avahi, restart
# alchemyd, and check the same on the built-in responder.
#
# It brings up and removes a Compose project named `mdnsapp`, publishes
# `mdnsapp.local` and `mdnsrival.local` on the LAN, starts and stops
# avahi-daemon (with sudo unless run as root), and leaves it as it found
# it, so it refuses to run unless CI=true or MDNS_CONFIRM=1.
#
# Usage: scripts/mdns.sh
#   MDNS_PORT          HTTP port for the test daemon (default 17243)
#   MDNS_SKIP_BUILD=1  reuse existing target/release binaries
#   MDNS_SETCAP=1      when port 80 is refused, grant alchemyd
#                      cap_net_bind_service with sudo setcap, so
#                      http://<app>.local/ is checked rather than
#                      http://<app>.local:<port>/
#
# Needs avahi-daemon, avahi-utils, libnss-mdns (`mdns4_minimal` in
# /etc/nsswitch.conf), and python3. Another device is played by a few lines
# of Python on this host: avahi doesn't treat two of its own clients
# publishing one name as a conflict, so the rival can't be `avahi-publish`.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
port="${MDNS_PORT:-17243}"
bin="${CARGO_TARGET_DIR:-${root}/target}/release"
app="mdnsapp"
rival_app="mdnsrival"
image="busybox:1.37.0"

log() { printf '==> %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

as_root() {
  if [[ "$(id -u)" -eq 0 ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

[[ "$(uname -s)" == Linux ]] || fail "the mDNS test needs Linux"
[[ "${CI:-}" == true || "${MDNS_CONFIRM:-}" == 1 ]] \
  || fail "this runs a Compose project named ${app}, publishes names on your network, and stops avahi-daemon; set MDNS_CONFIRM=1 to run it"
for tool in jq curl docker getent avahi-daemon avahi-resolve python3; do
  command -v "$tool" >/dev/null || fail "missing ${tool}; install avahi-daemon avahi-utils libnss-mdns"
done
grep -q '^hosts:.*mdns4' /etc/nsswitch.conf || fail "/etc/nsswitch.conf doesn't resolve .local names; install libnss-mdns"
docker info >/dev/null 2>&1 || fail "Docker isn't running or this user can't reach it"

cd "$root"

if [[ "${MDNS_SKIP_BUILD:-}" != 1 ]]; then
  log "Building release binaries"
  cargo build --release --locked -p alchemyd -p alchemy
fi
for exe in alchemyd alchemy; do
  [[ -x "${bin}/${exe}" ]] || fail "missing ${bin}/${exe}"
done

tmp="$(mktemp -d)"
chmod 755 "$tmp"
socket="${tmp}/a.sock"
state_dir="${tmp}/state"
apps_dir="${tmp}/apps"
daemon_pid=""
rival_pid=""
granted=""
avahi_was_running=""

alchemy() { "${bin}/alchemy" --socket "$socket" "$@"; }
api() { curl -fsS --unix-socket "$socket" "http://alchemyd$1"; }

# Without systemd, as in a container, run avahi-daemon directly; it needs
# the system D-Bus.
avahi_start() {
  if [[ -d /run/systemd/system ]]; then
    as_root systemctl start avahi-daemon.socket avahi-daemon.service
  else
    as_root avahi-daemon --daemonize --no-chroot
  fi
  for _ in $(seq 1 50); do
    avahi-daemon --check 2>/dev/null && return 0
    sleep 0.2
  done
  fail "avahi-daemon did not start"
}

# Stop the socket too, or a lookup would start avahi again.
avahi_stop() {
  if [[ -d /run/systemd/system ]]; then
    as_root systemctl stop avahi-daemon.socket avahi-daemon.service
  else
    as_root avahi-daemon --kill 2>/dev/null || true
  fi
  for _ in $(seq 1 50); do
    avahi-daemon --check 2>/dev/null || return 0
    sleep 0.2
  done
  fail "avahi-daemon did not stop"
}

log_from=1
start_daemon() {
  touch "${tmp}/alchemyd.log"
  log_from="$(($(wc -l <"${tmp}/alchemyd.log") + 1))"
  ALCHEMY_LOG=info "${bin}/alchemyd" --listen "127.0.0.1:${port}" --socket "$socket" \
    --state-dir "$state_dir" >>"${tmp}/alchemyd.log" 2>&1 &
  daemon_pid=$!
}

# logged PATTERN: the running daemon has logged a line matching PATTERN.
logged() {
  local lines
  lines="$(tail -n "+${log_from}" "${tmp}/alchemyd.log")"
  grep -q "$1" <<<"$lines"
}

stop_daemon() {
  if [[ -n "$daemon_pid" ]] && kill -0 "$daemon_pid" 2>/dev/null; then
    kill -TERM "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  daemon_pid=""
}

cleanup() {
  stop_daemon
  if [[ -n "$rival_pid" ]] && kill -0 "$rival_pid" 2>/dev/null; then
    kill -TERM "$rival_pid" 2>/dev/null || true
    wait "$rival_pid" 2>/dev/null || true
  fi
  if [[ -f "${apps_dir}/${app}/compose.yaml" ]]; then
    docker compose --project-directory "${apps_dir}/${app}" down --timeout 1 >/dev/null 2>&1 || true
  fi
  if [[ -n "$granted" ]]; then
    as_root setcap -r "${bin}/alchemyd" 2>/dev/null || true
  fi
  if [[ -n "$avahi_was_running" ]] && ! avahi-daemon --check 2>/dev/null; then
    avahi_start || true
  elif [[ -z "$avahi_was_running" ]] && avahi-daemon --check 2>/dev/null; then
    avahi_stop || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

die() {
  cat "${tmp}/alchemyd.log" >&2 || true
  [[ -f "${tmp}/rival.log" ]] && cat "${tmp}/rival.log" >&2
  fail "$@"
}

# wait_for WHAT JQ PATH: poll PATH on the socket until JQ holds, and
# leave the last answer in $answer.
answer=""
wait_for() {
  local what="$1" filter="$2" path="$3"
  for _ in $(seq 1 120); do
    answer="$(api "$path" 2>/dev/null)" || answer=""
    if [[ -n "$answer" ]] && jq -e "$filter" <<<"$answer" >/dev/null 2>&1; then
      return 0
    fi
    kill -0 "$daemon_pid" 2>/dev/null || die "alchemyd exited while waiting for ${what}"
    sleep 0.5
  done
  die "timed out waiting for ${what}: ${answer}"
}

# Plain mDNS with no responder library: `rival` answers for a name as
# another device that already owns it would, and `resolve` looks a name up
# without avahi-daemon. Both share this host's port 5353 with the
# responder under test, so the rival multicasts every answer: a unicast
# one could land on its own socket instead. (python3-zeroconf can't play
# the rival: it doesn't answer the ANY questions responders probe with.)
cat >"${tmp}/peer.py" <<'EOF'
import signal
import socket
import struct
import sys
import time

GROUP = ("224.0.0.251", 5353)
TYPE_A, TYPE_ANY, CLASS_IN, CACHE_FLUSH = 1, 255, 1, 0x8000


def open_socket(lan):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
    sock.bind(("", GROUP[1]))
    group = socket.inet_aton(GROUP[0]) + socket.inet_aton(lan)
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, group)
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(lan))
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 255)
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_LOOP, 1)
    return sock


def encode(name):
    return b"".join(bytes([len(label)]) + label.encode() for label in name.split(".")) + b"\0"


def read_name(data, at):
    labels, end = [], None
    while True:
        size = data[at]
        if size & 0xC0 == 0xC0:
            end = end or at + 2
            at = ((size & 0x3F) << 8) | data[at + 1]
        elif size == 0:
            return ".".join(labels).lower(), end or at + 1
        else:
            labels.append(data[at + 1 : at + 1 + size].decode(errors="replace"))
            at += 1 + size


def parse(data):
    _, flags, questions, answers, authorities, additionals = struct.unpack("!6H", data[:12])
    at, asked, records = 12, [], []
    for _ in range(questions):
        name, at = read_name(data, at)
        asked.append((name, struct.unpack("!H", data[at : at + 2])[0]))
        at += 4
    for _ in range(answers + authorities + additionals):
        name, at = read_name(data, at)
        kind, _, _, size = struct.unpack("!2HIH", data[at : at + 10])
        records.append((name, kind, data[at + 10 : at + 10 + size]))
        at += 10 + size
    return bool(flags & 0x8000), asked, records


def rival(name, addr, lan):
    host = f"{name}.local".lower()
    answer = (
        struct.pack("!6H", 0, 0x8400, 0, 1, 0, 0)
        + encode(host)
        + struct.pack("!2HIH", TYPE_A, CLASS_IN | CACHE_FLUSH, 120, 4)
        + socket.inet_aton(addr)
    )
    sock = open_socket(lan)
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    for _ in range(2):
        sock.sendto(answer, GROUP)
        time.sleep(1)
    print(f"ready {host} {addr}", flush=True)
    while True:
        data, _ = sock.recvfrom(9000)
        try:
            is_response, asked, _ = parse(data)
        except (IndexError, struct.error):
            continue
        if not is_response and any(q == host and t in (TYPE_A, TYPE_ANY) for q, t in asked):
            sock.sendto(answer, GROUP)


def resolve(name, lan):
    host = f"{name}.local".lower()
    sock = open_socket(lan)
    query = struct.pack("!6H", 0, 0, 1, 0, 0, 0) + encode(host) + struct.pack("!2H", TYPE_A, CLASS_IN)
    sock.sendto(query, GROUP)
    found, deadline = [], time.monotonic() + 3
    while not found and (left := deadline - time.monotonic()) > 0:
        sock.settimeout(left)
        try:
            data, _ = sock.recvfrom(9000)
            is_response, _, records = parse(data)
        except (TimeoutError, IndexError, struct.error):
            continue
        if is_response:
            found = [socket.inet_ntoa(r) for n, t, r in records if n == host and t == TYPE_A and len(r) == 4]
    print("\n".join(found))


if __name__ == "__main__":
    {"rival": rival, "resolve": resolve}[sys.argv[1]](*sys.argv[2:])
EOF

nonce="$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"

log "Writing app ${app}: one busybox httpd service on a loopback port"
mkdir -p "${apps_dir}/${app}/www"
cat >"${apps_dir}/${app}/compose.yaml" <<EOF
x-alchemy:
  service: web
  port: 80
services:
  web:
    image: ${image}
    init: true
    command: ["httpd", "-f", "-p", "80", "-h", "/www"]
    ports:
      - "127.0.0.1::80"
    volumes:
      - ./www:/www:ro
EOF
printf 'nonce=%s\n' "$nonce" >"${apps_dir}/${app}/www/index.html"
chmod -R a+rX "${apps_dir}"
docker pull --quiet "$image" >/dev/null

if avahi-daemon --check 2>/dev/null; then
  avahi_was_running=1
else
  log "Starting avahi-daemon"
  avahi_start
fi

log "Starting alchemyd"
start_daemon
wait_for "the engine" '.engine_ready' /api/network
alchemy settings set app-home "$apps_dir" >/dev/null || die "alchemy settings set app-home failed"
wait_for "Compose to read app ${app}" ".apps | any(.name == \"${app}\" and (.services | length) > 0)" /api/apps

log "Bringing up ${app}"
alchemy app up "$app" >"${tmp}/up.txt" 2>&1 || { cat "${tmp}/up.txt" >&2; die "alchemy app up ${app} failed"; }

host_json="$(api /api/network | jq -c .proxy.host)"
if [[ "$(jq -r .state <<<"$host_json")" != listening && "${MDNS_SETCAP:-}" == 1 ]]; then
  log "Granting alchemyd cap_net_bind_service and restarting it"
  stop_daemon
  granted=1
  as_root setcap cap_net_bind_service=+ep "${bin}/alchemyd"
  start_daemon
  wait_for "port 80" '.proxy.host.state == "listening"' /api/network
fi

# check_backend BACKEND LINE: the report names BACKEND, mdnsapp's best
# address is its ready .local one, alchemy status prints LINE, and the
# log says the name is on the network. Sets $lan_ip and $local_url.
lan_ip=""
local_url=""
check_backend() {
  local backend="$1" line="$2" app_json
  wait_for "mDNS to announce through ${backend}" \
    ".mdns.state == \"announcing\" and .mdns.backend == \"${backend}\"" /api/network
  wait_for "a ready .local address for ${app}" \
    ".apps[] | select(.name == \"${app}\") | .addresses // [] | first | .state == \"ready\" and (.url | test(\"^http://${app}\\\\.local[:/]\"))" \
    /api/apps
  app_json="$(jq -c ".apps[] | select(.name == \"${app}\")" <<<"$answer")"
  jq -e '.problems | length == 0' <<<"$app_json" >/dev/null \
    || die "${app} has problems: $(jq -c .problems <<<"$app_json")"
  local_url="$(jq -r '.addresses[0].url' <<<"$app_json")"
  lan_ip="$(jq -r '.addresses[] | select(.kind == "port" and (.url | test("\\.local[:/]") | not)) | .url' <<<"$app_json")"
  lan_ip="${lan_ip#http://}"
  lan_ip="${lan_ip%%:*}"
  [[ "$lan_ip" =~ ^[0-9]+(\.[0-9]+){3}$ ]] || die "${app} has no LAN IP:port address: ${app_json}"
  if [[ "$(jq -r .proxy.host.state <<<"$(api /api/network)")" == listening ]]; then
    [[ "$local_url" == "http://${app}.local/" ]] || die "expected http://${app}.local/ first with port 80 verified, got ${local_url}"
  fi
  log "${app}'s best address is ${local_url} on ${lan_ip}"
  logged "\.local name is on the network name=\"\?${app}\.local" \
    || die "the log does not say ${app}.local is on the network"
  alchemy status | grep -qxF "$line" || die "alchemy status does not print '${line}': $(alchemy status)"
}

# check_page HOW: the app's page came back with this run's random value.
check_page() {
  grep -qxF "nonce=${nonce}" "${tmp}/page.txt" || die "$1 did not reach ${app}: $(cat "${tmp}/page.txt")"
}

# start_rival: another responder answers for mdnsrival.local with another
# address on the LAN's subnet before alchemyd announces it.
rival_ip=""
start_rival() {
  local a b c d
  IFS=. read -r a b c d <<<"$lan_ip"
  rival_ip="${a}.${b}.${c}.$([[ "$d" == 250 ]] && echo 251 || echo 250)"
  log "Answering for ${rival_app}.local with ${rival_ip} from another responder"
  python3 -I "${tmp}/peer.py" rival "$rival_app" "$rival_ip" "$lan_ip" >"${tmp}/rival.log" 2>&1 &
  rival_pid=$!
  for _ in $(seq 1 50); do
    grep -q '^ready' "${tmp}/rival.log" && return 0
    kill -0 "$rival_pid" 2>/dev/null || die "the rival responder exited"
    sleep 0.2
  done
  die "the rival responder did not register"
}

# check_conflict: alchemyd lost mdnsrival.local to the rival, and says so.
check_conflict() {
  local message="Another device on your network uses ${rival_app}.local. Set a hostname in x-alchemy."
  wait_for "${rival_app}.local to be in use" \
    ".apps[] | select(.name == \"${rival_app}\") | .problems | any(.kind == \"route\" and .message == \"${message}\")" \
    /api/apps
  jq -e ".apps[] | select(.name == \"${rival_app}\") | .addresses | map(select(.url | test(\"^http://${rival_app}\\\\.local[:/]\"))) | length > 0 and all(.state == \"unavailable\" and .message == \"Another device on your network uses ${rival_app}.local.\")" \
    <<<"$answer" >/dev/null || die "${rival_app}'s .local addresses are not unavailable: ${answer}"
  jq -e ".apps[] | select(.name == \"${rival_app}\") | .addresses[0].url | test(\"\\\\.local[:/]\") | not" <<<"$answer" >/dev/null \
    || die "${rival_app}'s best address is still a .local one: ${answer}"
  logged "another device on the network uses this .local name name=\"\?${rival_app}\.local" \
    || die "the log does not warn that ${rival_app}.local is in use"
}

write_rival_app() {
  mkdir -p "${apps_dir}/${rival_app}"
  printf 'services:\n  web:\n    image: %s\n    ports:\n      - "127.0.0.1::80"\n' "$image" \
    >"${apps_dir}/${rival_app}/compose.yaml"
  chmod -R a+rX "${apps_dir}/${rival_app}"
}

# avahi_ip NAME: the address avahi resolves NAME to, if any. avahi-resolve
# exits 0 even when it finds nothing.
avahi_ip() { avahi-resolve -4 -n "$1" 2>/dev/null | awk '{print $2}'; }

# check_gone WHEN: avahi stops resolving mdnsapp.local within a few seconds.
check_gone() {
  local found
  for _ in $(seq 1 10); do
    found="$(avahi_ip "${app}.local")"
    [[ -z "$found" ]] && return 0
    sleep 0.5
  done
  die "${app}.local still resolves to ${found} $1"
}

log "Checking ${app}.local through avahi"
check_backend avahi "Local names (mDNS): announced through avahi"
resolved="$(avahi_ip "${app}.local")"
[[ "$resolved" == "$lan_ip" ]] || die "avahi-resolve gave '${resolved}' for ${app}.local, expected ${lan_ip}"
resolved="$(getent ahostsv4 "${app}.local" | awk 'NR == 1 {print $1}')"
[[ "$resolved" == "$lan_ip" ]] || die "getent (nss-mdns) gave '${resolved}' for ${app}.local, expected ${lan_ip}"
# nss-mdns first asks unicast DNS whether it serves `local`, which can
# take a while behind some resolvers.
curl -4 -fsS --max-time 30 "$local_url" >"${tmp}/page.txt" || die "GET ${local_url} by name failed"
check_page "GET ${local_url} by name"

start_rival
resolved="$(avahi_ip "${rival_app}.local")"
[[ "$resolved" == "$rival_ip" ]] || die "avahi doesn't see the rival: ${rival_app}.local is '${resolved}', expected ${rival_ip}"
write_rival_app
check_conflict

log "Turning local names off and on"
alchemy settings set mdns off >/dev/null || die "alchemy settings set mdns off failed"
wait_for "local names to be off" '.mdns.state == "off"' /api/network
wait_for "${app}'s .local addresses to go" \
  ".apps[] | select(.name == \"${app}\") | .addresses | all(.url | test(\"\\\\.local[:/]\") | not)" /api/apps
check_gone "with local names off"
logged "withdrew .local name name=\"\?${app}\.local" \
  || die "the log does not say ${app}.local was withdrawn"
alchemy settings set mdns on >/dev/null || die "alchemy settings set mdns on failed"
wait_for "${app}.local to be ready again" \
  ".apps[] | select(.name == \"${app}\") | .addresses // [] | first | .state == \"ready\" and (.url | test(\"^http://${app}\\\\.local[:/]\"))" \
  /api/apps

log "Stopping alchemyd"
stop_daemon
logged 'withdrew .local names count=' || die "alchemyd did not withdraw its names on shutdown"
check_gone "after alchemyd stopped"

log "Stopping avahi-daemon and restarting alchemyd"
avahi_stop
start_daemon

log "Checking ${app}.local through the built-in responder"
check_backend built_in "Local names (mDNS): announced by the built-in responder"
logged 'announcing .local names backend=built-in' \
  || die "the log does not say alchemyd fell back to the built-in responder"
resolved=""
for _ in $(seq 1 5); do
  resolved="$(python3 -I "${tmp}/peer.py" resolve "$app" "$lan_ip" | head -n 1)"
  [[ -n "$resolved" ]] && break
done
[[ "$resolved" == "$lan_ip" ]] || die "another responder resolved ${app}.local to '${resolved}', expected ${lan_ip}"
local_host="${local_url#http://}"
local_host="${local_host%/}"
[[ "$local_host" == *:* ]] || local_host="${local_host}:80"
curl -fsS --max-time 10 --resolve "${local_host}:${resolved}" "$local_url" >"${tmp}/page.txt" \
  || die "GET ${local_url} at the address mDNS gave failed"
check_page "GET ${local_url} at the address mDNS gave"
check_conflict

stop_daemon
log "mDNS test passed"
