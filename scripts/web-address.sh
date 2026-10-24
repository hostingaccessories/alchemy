#!/usr/bin/env bash
# Web-address test (Linux only): run alchemyd against the real Docker
# engine, bring up an app whose compose file publishes no ports, and check
# that the app answers through its reserved proxy port and, when alchemyd
# can bind port 80, through http://<app>.localhost, and that each answer
# came from the app's container over the `alchemy` network.
#
# It brings up and removes a Compose project named `webaddr`, and alchemyd
# tries to listen on port 80 and creates the `alchemy` network if it is
# missing, so it refuses to run unless CI=true or WEB_ADDR_CONFIRM=1.
#
# Usage: scripts/web-address.sh
#   WEB_ADDR_PORT          HTTP port for the test daemon (default 17242)
#   WEB_ADDR_SKIP_BUILD=1  reuse existing target/release binaries
#   WEB_ADDR_SETCAP=1      when port 80 is refused, grant alchemyd
#                          cap_net_bind_service with sudo setcap, restart
#                          it, and check Host routing too

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
port="${WEB_ADDR_PORT:-17242}"
bin="${CARGO_TARGET_DIR:-${root}/target}/release"
app="webaddr"
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

[[ "$(uname -s)" == Linux ]] || fail "the web-address test needs Linux"
[[ "${CI:-}" == true || "${WEB_ADDR_CONFIRM:-}" == 1 ]] \
  || fail "this runs a Compose project named ${app} and listens on port 80; set WEB_ADDR_CONFIRM=1 to run it"
for tool in jq curl docker; do
  command -v "$tool" >/dev/null || fail "missing ${tool}"
done
docker info >/dev/null 2>&1 || fail "Docker isn't running or this user can't reach it"

cd "$root"

if [[ "${WEB_ADDR_SKIP_BUILD:-}" != 1 ]]; then
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
granted=""

alchemy() { "${bin}/alchemy" --socket "$socket" "$@"; }
api() { curl -fsS --unix-socket "$socket" "http://alchemyd$1"; }

start_daemon() {
  "${bin}/alchemyd" --listen "127.0.0.1:${port}" --socket "$socket" \
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

cleanup() {
  stop_daemon
  if [[ -f "${apps_dir}/${app}/compose.yaml" ]]; then
    docker compose --project-directory "${apps_dir}/${app}" down --timeout 1 >/dev/null 2>&1 || true
  fi
  if [[ -n "$granted" ]]; then
    as_root setcap -r "${bin}/alchemyd" 2>/dev/null || true
  fi
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

ip_int() {
  local a b c d
  IFS=. read -r a b c d <<<"$1"
  echo $(((a << 24) | (b << 16) | (c << 8) | d))
}

int_ip() {
  local n="$1"
  printf '%d.%d.%d.%d' $(((n >> 24) & 255)) $(((n >> 16) & 255)) $(((n >> 8) & 255)) $((n & 255))
}

nonce="$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"

log "Writing app ${app}: one busybox httpd service, no ports, routed by x-alchemy"
mkdir -p "${apps_dir}/${app}/www/cgi-bin"
cat >"${apps_dir}/${app}/compose.yaml" <<EOF
x-alchemy:
  service: web
  port: 80
services:
  web:
    image: ${image}
    init: true
    command: ["httpd", "-f", "-p", "80", "-h", "/www"]
    volumes:
      - ./www:/www:ro
EOF
cat >"${apps_dir}/${app}/www/cgi-bin/echo" <<EOF
#!/bin/sh
printf 'Content-Type: text/plain\r\n\r\n'
printf 'nonce=%s\n' '${nonce}'
printf 'hostname=%s\n' "\$(hostname)"
printf 'host=%s\n' "\$HTTP_HOST"
printf 'remote=%s\n' "\$REMOTE_ADDR"
printf 'forwarded_for=%s\n' "\$HTTP_X_FORWARDED_FOR"
printf 'forwarded_host=%s\n' "\$HTTP_X_FORWARDED_HOST"
printf 'forwarded_proto=%s\n' "\$HTTP_X_FORWARDED_PROTO"
printf 'cookie=%s\n' "\$HTTP_COOKIE"
EOF
chmod -R a+rX "${apps_dir}"
chmod 755 "${apps_dir}/${app}/www/cgi-bin/echo"
docker pull --quiet "$image" >/dev/null

log "Starting alchemyd"
start_daemon
wait_for "the engine and the alchemy network" \
  '.engine_ready and .alchemy.state == "ready" and .proxy.reachability != null' /api/network
reach="$(jq -r .proxy.reachability <<<"$answer")"
[[ "$reach" == container_ip ]] \
  || die "expected the proxy to reach container IPs on a native engine, got ${reach}"

alchemy settings set app-home "$apps_dir" >/dev/null || die "alchemy settings set app-home failed"
wait_for "Compose to read app ${app}" ".apps | any(.name == \"${app}\" and (.services | length) > 0)" /api/apps

log "Bringing up ${app}"
alchemy app up "$app" >"${tmp}/up.txt" 2>&1 || { cat "${tmp}/up.txt" >&2; die "alchemy app up ${app} failed"; }

wait_for "a ready port address for ${app}" \
  ".apps[] | select(.name == \"${app}\") | .addresses // [] | any(.kind == \"port\" and .state == \"ready\")" \
  /api/apps
app_json="$(jq -c ".apps[] | select(.name == \"${app}\")" <<<"$answer")"
jq -e '.problems | length == 0' <<<"$app_json" >/dev/null \
  || die "${app} has problems: $(jq -c .problems <<<"$app_json")"
port_url="$(jq -r '.addresses[] | select(.kind == "port") | .url' <<<"$app_json")"
proxy_port="${port_url##*:}"
proxy_port="${proxy_port%/}"
if ! [[ "$proxy_port" =~ ^[0-9]+$ ]] || ((proxy_port < 8100 || proxy_port > 8999)); then
  die "${app}'s port address ${port_url} is not on a port from 8100-8999"
fi
jq -e --arg url "$port_url" '.addresses[] | select(.url == $url) | .service == "web" and .port == 80' \
  <<<"$app_json" >/dev/null || die "${port_url} does not go to port 80 of service web: ${app_json}"

api /api/ports | jq -e --arg owner "proxy:${app}" --arg port "$proxy_port" \
  '.ports | any(.holder.kind == "reservation" and .holder.owner == $owner and .port.published == $port)' \
  >/dev/null || die "port ${proxy_port} is not reserved as proxy:${app}"
api /api/network | jq -e --arg app "$app" --argjson port "$proxy_port" \
  '.proxy.ports | any(.port == $port and .app == $app and .state == "listening")' >/dev/null \
  || die "the proxy is not listening on ${proxy_port} for ${app}"

log "Checking the container publishes nothing and is on the alchemy network"
cid="$(docker ps -q --filter "label=com.docker.compose.project=${app}" --filter label=com.docker.compose.service=web)"
[[ -n "$cid" && "$(wc -l <<<"$cid")" -eq 1 ]] || die "expected one running web container, got: ${cid}"
docker inspect "$cid" --format '{{json .NetworkSettings.Ports}}' \
  | jq -e '[.[]? | select(. != null)] | length == 0' >/dev/null \
  || die "the web container publishes ports: $(docker inspect "$cid" --format '{{json .NetworkSettings.Ports}}')"
container_ip="$(docker inspect "$cid" --format '{{with index .NetworkSettings.Networks "alchemy"}}{{.IPAddress}}{{end}}')"
[[ -n "$container_ip" ]] \
  || die "the web container is not on the alchemy network: $(docker inspect "$cid" --format '{{json .NetworkSettings.Networks}}')"
hostname="$(docker inspect "$cid" --format '{{.Config.Hostname}}')"
subnet="$(docker network inspect alchemy --format '{{range .IPAM.Config}}{{.Subnet}}{{end}}')"
gateway="$(docker network inspect alchemy --format '{{range .IPAM.Config}}{{.Gateway}}{{end}}')"
if [[ -z "$gateway" ]]; then
  gateway="$(int_ip $(($(ip_int "${subnet%/*}") + 1)))"
fi
log "Container ${hostname} is ${container_ip} on alchemy (${subnet}, gateway ${gateway})"

# check_echo NAME URL HOST [CLIENT]: GET URL/cgi-bin/echo and check the
# answer came from the container through the proxy, with Host: HOST kept
# and X-Forwarded-For set to CLIENT (127.0.0.1 by default).
check_echo() {
  local name="$1" url="$2" host="$3" client="${4:-127.0.0.1}" body
  body="$(curl -fsS --max-time 10 -H "Host: ${host}" \
    -H 'X-Forwarded-For: 203.0.113.9' -H 'X-Forwarded-Host: evil.example' \
    -H 'Cookie: alchemy_session_0123=secret; keep=1' \
    "${url%/}/cgi-bin/echo")" || die "${name}: GET ${url%/}/cgi-bin/echo failed"
  printf '%s\n' "$body" >"${tmp}/${name}.txt"
  local field remote
  for field in "nonce=${nonce}" "hostname=${hostname}" "host=${host}" \
    "forwarded_for=${client}" "forwarded_host=${host}" "forwarded_proto=http" "cookie=keep=1"; do
    grep -qxF "$field" <<<"$body" || die "${name}: expected ${field} in the app's answer: ${body}"
  done
  remote="$(sed -n 's/^remote=//p' <<<"$body")"
  remote="${remote#[}"
  remote="${remote%]}"
  remote="${remote#::ffff:}"
  [[ "$remote" == "$gateway" ]] \
    || die "${name}: the request reached the container from ${remote}, not the alchemy network's gateway ${gateway}"
}

log "Checking ${app} answers on 127.0.0.1:${proxy_port}"
check_echo port "http://127.0.0.1:${proxy_port}/" "127.0.0.1:${proxy_port}"
if [[ "$port_url" != "http://127.0.0.1:${proxy_port}/" ]]; then
  log "Checking ${app} answers on its advertised address ${port_url}"
  lan_host="${port_url#http://}"
  lan_host="${lan_host%/}"
  check_echo lan "$port_url" "$lan_host" "${lan_host%:*}"
fi

log "Checking a forged listener probe never reaches ${app}"
code="$(curl -sS -o "${tmp}/probe.html" -w '%{http_code}' -H "X-Alchemy-Probe: $(printf '0%.0s' {1..64})" \
  "http://127.0.0.1:${proxy_port}/cgi-bin/echo")" || die "GET with X-Alchemy-Probe on ${proxy_port} failed"
[[ "$code" == 404 ]] || die "a forged probe on ${proxy_port} returned ${code}, expected 404"
if grep -qF "nonce=" "${tmp}/probe.html"; then
  die "a forged probe on ${proxy_port} reached ${app}"
fi

# check_host: Host routing on port 80 reaches the app, and an unknown Host
# gets the proxy's 404, never the API.
check_host() {
  local host_url
  host_url="$(api /api/apps | jq -r ".apps[] | select(.name == \"${app}\") | .addresses[] | select(.kind == \"host\") | .url")"
  [[ "$host_url" == "http://${app}.localhost/" ]] || die "expected the address http://${app}.localhost/, got ${host_url}"
  log "Checking Host: ${app}.localhost on port 80"
  check_echo host80 "http://127.0.0.1/" "${app}.localhost"
  local name code
  for name in nope.localhost localhost "127.0.0.1:${port}"; do
    code="$(curl -sS -o "${tmp}/unknown.html" -w '%{http_code}' -H "Host: ${name}" http://127.0.0.1/api/health)" \
      || die "GET /api/health with Host ${name} on port 80 failed"
    [[ "$code" == 404 ]] || die "Host ${name} on port 80 returned ${code}, expected 404"
    if grep -q '"ok"' "${tmp}/unknown.html"; then
      die "Host ${name} on port 80 reached the API"
    fi
    grep -qi 'alchemy' "${tmp}/unknown.html" || die "Host ${name} on port 80 did not get the proxy's page"
  done
}

host_json="$(api /api/network | jq -c .proxy.host)"
case "$(jq -r .state <<<"$host_json")" in
  listening)
    check_host
    ;;
  unavailable)
    log "alchemyd can't bind port 80: $(jq -r .message <<<"$host_json")"
    jq -e '.port == 80 and (.message | test("permission")) and (.fix | test("setcap cap_net_bind_service"))' \
      <<<"$host_json" >/dev/null || die "unexpected port 80 report: ${host_json}"
    api /api/apps | jq -e ".apps[] | select(.name == \"${app}\") | .addresses | any(.kind == \"host\" and .state == \"unavailable\")" \
      >/dev/null || die "${app}'s host address is not reported unavailable"
    alchemy status | grep -qF "Warning: Alchemy doesn't have permission to listen on port 80" \
      || die "alchemy status does not warn about port 80"
    if [[ "${WEB_ADDR_SETCAP:-}" == 1 ]]; then
      log "Granting alchemyd cap_net_bind_service and restarting it"
      stop_daemon
      granted=1
      as_root setcap cap_net_bind_service=+ep "${bin}/alchemyd"
      start_daemon
      wait_for "port 80 and ${app}'s addresses after the restart" \
        "(.apps[] | select(.name == \"${app}\") | .addresses // [] | map(select(.state == \"ready\")) | length) == 2" \
        /api/apps
      api /api/apps | jq -e --arg url "$port_url" ".apps[] | select(.name == \"${app}\") | .addresses | any(.url == \$url)" \
        >/dev/null || die "${app} lost its port ${proxy_port} across a restart"
      check_echo port-restart "http://127.0.0.1:${proxy_port}/" "127.0.0.1:${proxy_port}"
      check_host
    else
      log "Skipping Host routing on port 80; set WEB_ADDR_SETCAP=1 to grant the capability"
    fi
    ;;
  *) die "unexpected port 80 listener: ${host_json}" ;;
esac

log "Stopping ${app}"
alchemy app stop "$app" >/dev/null 2>&1 || die "alchemy app stop ${app} failed"
wait_for "${app}'s port address to report not running" \
  ".apps[] | select(.name == \"${app}\") | .addresses // [] | any(.kind == \"port\" and .state == \"not_running\")" \
  /api/apps
code="$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:${proxy_port}/")" \
  || die "GET on port ${proxy_port} failed after stopping ${app}"
[[ "$code" == 503 ]] || die "a stopped app's port answered ${code}, expected 503"

stop_daemon
log "Web-address test passed"
