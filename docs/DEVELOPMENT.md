# Alchemy: Development

How to build, run, and test Alchemy from a fresh clone. For the rules the
code follows, see [ARCHITECTURE.md](ARCHITECTURE.md).

---

## 1. Prerequisites

- **Rust** via [rustup](https://rustup.rs). The toolchain is pinned in
  `rust-toolchain.toml` and installs on first use.
- **Node** at the version in `.node-version`.
- **[Vite+](https://viteplus.dev)** (`vp`). pnpm is pinned in `package.json`
  and downloaded automatically.
- **Linux only:** the GTK and WebKit development packages Tauri links
  against. The list is in `.github/actions/install-tauri-deps/action.yml`.
- **Docker:** Docker Desktop, or Docker Engine with the Compose v2 plugin.
  The daemon starts without it and reports Docker as not installed or not
  running.
- **Optional:** [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny)
  for the dependency policy check.

## 2. First build

```sh
vp install
vp run alchemy-web#build
cargo build --workspace
```

The web build is optional for Rust work: the workspace compiles without it,
and the daemon serves no UI until it exists.

## 3. Running locally

| Command                          | What it does                                                         |
| -------------------------------- | -------------------------------------------------------------------- |
| `cargo run -p alchemyd`          | Daemon on `127.0.0.1:7240` plus the API on a Unix socket             |
| `cargo run -p alchemy -- status` | CLI; talks to the daemon's API over its Unix socket                  |
| `pnpm dev`                       | Web UI dev server on `localhost:5173`, proxying `/api` to the daemon |
| `pnpm gui`                       | Tauri desktop shell in dev mode (starts the web dev server)          |
| `pnpm docs`                      | Documentation site dev server                                        |

The web UI on the TCP port needs signing in. Open it with
`cargo run -p alchemy -- open`, which opens a one-time sign-in link in
your browser. For the dev server, run
`cargo run -p alchemy -- open --url-base http://localhost:5173`: Vite
passes `/login` and `/api` on to the daemon, keeping the `Host`, so the
session cookie belongs to `localhost:5173`. `pnpm gui` needs nothing: the
shell reads the daemon's token itself. See
[Signing in to the TCP port](#signing-in-to-the-tcp-port).

Daemon settings, as flags or environment variables:

| Flag          | Variable            | Default                                                            |
| ------------- | ------------------- | ------------------------------------------------------------------ |
| `--listen`    | `ALCHEMY_LISTEN`    | `127.0.0.1:7240`                                                   |
| `--socket`    | `ALCHEMY_SOCKET`    | `$XDG_RUNTIME_DIR/alchemy/alchemyd.sock`, else a per-user temp dir |
| `--state-dir` | `ALCHEMY_STATE_DIR` | The per-user state directory, below                                |
|               | `ALCHEMY_LOG`       | `info` (any `tracing` filter, e.g. `alchemyd=debug`)               |
| `--daemon`    | `ALCHEMY_DAEMON`    | CLI only: the daemon as a URL; only `unix:///path` for now         |

The state directory is `$XDG_STATE_HOME/alchemy` (else
`~/.local/state/alchemy`) on Linux and `~/Library/Application Support/Alchemy`
on macOS. It holds `alchemy.db`, the daemon's SQLite database of the app
folder settings, operation and metrics history, and stop intents, and
`tcp-token`, which signs clients in to the TCP port
([Signing in to the TCP port](#signing-in-to-the-tcp-port)). The directory is `0700` and the database and token `0600`. One
daemon owns a state directory: at startup, before opening the database,
alchemyd takes an exclusive `flock` on `alchemyd.lock` in it, and a second
daemon on the same directory fails at once, naming it. The OS drops the
lock when the daemon exits, so a leftover file is harmless. A
database that is corrupt, or was written by a newer alchemyd, stops startup
with a message saying so; move it aside to start fresh. Point
`--state-dir` at a temporary directory to run a throwaway daemon. The schema
lives in `bin/alchemyd/src/store/migrations.rs`, which explains how to add
a migration.

The CLI honours `--socket` and `ALCHEMY_SOCKET` the same way as the daemon.
`--daemon unix:///path/to/alchemyd.sock` is the same as
`--socket /path/to/alchemyd.sock`; passing both is a usage error. Precedence
is `--daemon` or `--socket`, then `ALCHEMY_DAEMON`, then `ALCHEMY_SOCKET`,
then the default. Other schemes, such as `https://`, are refused with exit 2
and nothing is contacted: remote daemons need authentication, which arrives
in M13 ([#16](https://github.com/hostingaccessories/alchemy/issues/16)).

Every command prints its results on stdout and everything else on stderr.
With `--json`, results are one JSON document, or one per line for a stream,
and errors are one `{"error":{"kind","message","hint"}}` line on stderr. The
exit code says what kind of failure it was: 2 for a bad command line, 69
when alchemyd can't be reached, 76 when alchemy and alchemyd are different
versions, and so on through [the table in §8](#errors-and-exit-codes).
`alchemy version` shows the versions and protocols of the CLI and the
daemon; `alchemy --version` shows the CLI's alone, without contacting it.

### Calling the API

The socket and the TCP port serve the same `/api`; only TCP serves the web
UI and `/login`. `alchemy status` prints the socket path. Any host name
works on the socket. On the port, everything under `/api` but
`/api/health` and `/api/version` needs the daemon's token or a browser
session, as below.

```sh
curl --unix-socket /run/user/1000/alchemy/alchemyd.sock http://alchemyd/api/version
curl http://127.0.0.1:7240/api/version
curl -H "Authorization: Bearer $(cat ~/.local/state/alchemy/tcp-token)" http://127.0.0.1:7240/api/apps
curl -N --unix-socket /run/user/1000/alchemy/alchemyd.sock http://alchemyd/api/events
```

`/api/events` is a server-sent event stream; `-N` prints each event as it
arrives.

The TCP port only answers a `Host` of `localhost`, `tauri.localhost`, a
loopback address, or the `--listen` address (any address when listening on
`0.0.0.0` or `::`), with any port, so a DNS-rebinding page can't reach it. A
daemon bound to a LAN address is reached by that address, not a host name.
Anything that isn't `GET`, `HEAD`, or `OPTIONS` must send
`Content-Type: application/json`, body or not, on both the port and the
socket (415 `unsupported_media_type` otherwise). On the port it is also
refused when it carries an `Origin` other than its own or the desktop
shell's (403 `forbidden`). Any request under `/api` on the port, `GET`
included, is refused the same way when the browser marks it
`Sec-Fetch-Site: cross-site` or `same-site` and its `Origin` isn't the
shell's, so another page can't open log streams with `no-cors` fetches;
requests without the header, like `curl`'s, pass. The socket ignores
`Host`, `Origin`, and `Sec-Fetch-Site`: no browser can open it.

```sh
curl -X POST -H 'Content-Type: application/json' -d '{}' http://127.0.0.1:7240/api/...
```

### Signing in to the TCP port

The guard checks keep other web pages off the port, but any process or
other user on the machine can connect to loopback, and the API can create
an app, which runs as root through Docker, or read an app's `.env`. So the
port requires signing in (`bin/alchemyd/src/auth.rs`, the live state in
`session.rs`), after the guard checks and before any route. The socket
needs none: its `0600` mode is the access control. Full accounts, for
other machines, are M13 ([#16](https://github.com/hostingaccessories/alchemy/issues/16)).

- **Token.** Each start, once both listeners are bound and before either
  serves a request, alchemyd writes 256 random bits from the OS, as hex, to
  `tcp-token` in the state directory, `0600`, replacing the last one in
  one rename. Shutdown removes it. Every request under `/api` on the port
  but `GET /api/health` and `GET /api/version` must send it as
  `Authorization: Bearer TOKEN`, or a session cookie; anything else is 401
  `unauthorized` with `WWW-Authenticate: Bearer`. A wrong bearer is
  refused even beside a good cookie. Comparison is constant-time. A
  restart signs everything out.
- **Browser sessions.** `alchemy open` asks the socket for a launch code
  (`POST /api/session/launch`, which the port refuses with 403) and opens
  `http://127.0.0.1:7240/login?code=CODE`. `GET /login` uses the code up,
  whatever happens, and answers `303` to `/` with `Cache-Control:
  no-store`, `Referrer-Policy: no-referrer`, and
  `Set-Cookie: alchemy_session_PORT=ID; HttpOnly; SameSite=Strict;
  Path=/api`. A code works once, within 60 seconds. The cookie holds a
  random session id, never the token, and the daemon keeps only SHA-256
  digests of ids and codes (at most 64 sessions and 16 codes). It has no
  `Secure`, since browsers refuse that over plain `http://127.0.0.1`, and
  no `Max-Age`. Browsers send a cookie to every port on its host, so the
  name carries the daemon's port and the path is `/api` only, but any
  other page served from `127.0.0.1` whose path starts with `/api` gets it
  too; this is why sessions end with the daemon. `GET /api/session` says
  how a request got in (`socket`, `token`, or `cookie`).
- **Why `alchemy open` doesn't pass the link to the browser.** A process's
  arguments are readable by every user (`ps`, `/proc/PID/cmdline`), so
  another user could read the code and use it first. `alchemy open`
  writes a `0600` page with the link, `open.html` in the `0700` runtime
  directory, and opens that file with `/usr/bin/open` on macOS or the
  first `xdg-open` in an absolute `PATH` directory on Linux, never through
  a shell. Browsers confined away from that directory, such as Ubuntu's
  Firefox snap, can't read the page; use `alchemy open --print`, which
  prints the link instead, as `alchemy open` also does when it finds no
  opener. `--json` prints `{"url","opened","expires_in"}`.
- **The desktop shell** serves the UI from its own origin, so the cookie
  never reaches it. Its `daemon_token` command (`app/alchemy-gui`, allowed
  for the main window only) reads `tcp-token` from the same state
  directory as alchemyd, honouring `ALCHEMY_STATE_DIR`, and the UI sends
  it as a bearer token (`src/lib/platform.ts`). After a 401 it reads the
  file again and retries once, since a restarted daemon has a new token.
  `EventSource` can't send headers, so in the shell event streams are read
  with `fetch` (`src/lib/stream.ts`) rather than putting a credential in a
  URL. CORS allows `Authorization` from the shell's origins only.
- **The web UI** shows "You're signed out of alchemyd" while
  `GET /api/session` answers 401, and the daemon status keeps using the
  public version probe.

### Shell completions

`alchemy completions <bash|zsh|fish>` writes a completion script to stdout.
It never contacts the daemon.

```sh
alchemy completions bash > ~/.local/share/bash-completion/completions/alchemy
alchemy completions zsh > ~/.zfunc/_alchemy   # with ~/.zfunc on $fpath
alchemy completions fish > ~/.config/fish/completions/alchemy.fish
```

### Remote daemon over SSH

Until remote daemons have authentication, reach one on another machine by
forwarding its socket over SSH, as the user that runs alchemyd there, and pointing `--socket` at the
local end. `alchemy status` on the server prints the remote path.

```sh
ssh -N -o StreamLocalBindUnlink=yes \
  -L "$HOME/.alchemyd-server.sock:/run/user/1000/alchemy/alchemyd.sock" \
  user@server &
alchemy --socket "$HOME/.alchemyd-server.sock" status
```

The local socket is created `0600`, but anyone who can open it controls the
remote daemon, so keep it in a directory only you can write to.
`StreamLocalBindUnlink` replaces a socket left behind by an earlier session.

For the remote web UI, forward its port too (`-L 7240:127.0.0.1:7240`)
and run `alchemy --socket "$HOME/.alchemyd-server.sock" open`: the link
names the server's loopback address, which the tunnel brings to yours.

### Docker engine

The daemon watches the engine in the background and never fails to start
because of it. It looks for the engine socket in this order:

1. `DOCKER_HOST`, if set and non-empty. It is then the only candidate,
   as with the Docker CLI.
2. The current `docker context` (`docker context inspect`), which honours
   `DOCKER_CONTEXT`.
3. Well-known sockets: `~/.docker/run/docker.sock` on macOS (Docker
   Desktop); `~/.docker/desktop/docker.sock` (Docker Desktop) and
   `$XDG_RUNTIME_DIR/docker.sock` (rootless) on Linux.
4. `/var/run/docker.sock`.

The first socket that exists wins. If none does, the daemon reports Docker as
not running at the first one. Only `unix://` endpoints are supported: a
`tcp://` or `ssh://` `DOCKER_HOST` or context is reported as unsupported, not
replaced by a local socket. `DOCKER_HOST` is read once at startup; the
context and sockets are re-checked every 2 seconds while Docker isn't ready.

`alchemy status` prints the daemon line and a Docker line:

```console
$ cargo run -p alchemy -- status
alchemyd 0.1.0 (protocol 1) at /run/user/1000/alchemy/alchemyd.sock
Docker ready: Engine 28.0.1, Compose v2.33.0
```

`status --watch` prints the same, then a new Docker line each time it
changes, until Ctrl-C (exit 130). With `--json`, `status` prints one object
(`daemon`, `protocol`, `socket`, `engine`) and `--watch` prints one per
update. The CLI exits 69 if it can't reach the daemon or the daemon stops
during `--watch`, and 76 if the daemon is too old to report Docker status.

To exercise the not-running path without stopping Docker, point the daemon at
a socket that doesn't exist:

```sh
DOCKER_HOST=unix:///nonexistent/docker.sock cargo run -p alchemyd
```

`status` then reports `Docker isn't running at unix:///nonexistent/docker.sock`,
or `Docker Desktop isn't running at …` when Docker Desktop is installed.
`DOCKER_HOST=tcp://127.0.0.1:2375` exercises the unsupported path. Stopping
Docker Desktop or the engine service while the daemon runs shows the
transitions live.

The engine tests use fakes and run without Docker; the few that need the
real engine are listed in [Tests that need Docker](#tests-that-need-docker).

### Apps

An app is a folder holding a compose file inside one of the app folders.
The home, `~/Alchemy/apps` unless chosen otherwise, is searched first and
is where new apps go; other folders, such as an existing `/opt/stacks`,
are searched after it in order. Both are settings in the state database,
changed with `PUT /api/settings/apps`, `alchemy settings set app-home
PATH|default`, `alchemy settings set app-folders [PATH]...`, or the
Settings page. Paths must be full paths.

Discovery only reads. It never creates, changes, or removes anything in
an app folder, and never creates the home either: a missing home is
reported as normal until the first app is added, which creates it. The
rules, decided in `alchemy_core::stack::discover` over a directory
listing:

- The compose file is the first of `compose.yaml`, `compose.yml`,
  `docker-compose.yaml`, `docker-compose.yml` that exists, as Compose
  picks it. If that one is a symlink, or not a regular file, the folder is
  skipped rather than falling back to a later name, so Alchemy and a
  hand-run `docker compose` always read the same file.
- The app's name is the folder's name. A folder that is a symlink, or whose
  name isn't a valid app name (`[a-z0-9][a-z0-9_-]*`, at most 63
  characters), is skipped. So is a name already found in an earlier
  folder: the first folder wins.
- Skipped folders are reported in `dirs[].skipped` of `/api/apps`, by
  `alchemy app list`, and on the Apps page. Hidden folders, files, and
  folders without a compose file are ignored without a report.

The daemon lists the folders every 5 seconds and runs `docker compose
--project-directory DIR config --format json` for an app only when it is
new or its compose file, overrides, or `.env` changed (by size and
modification time), at most 4 at a time, to learn the project name Compose
resolves (folder name, `name:`, or `COMPOSE_PROJECT_NAME` in the app's
`.env`) and whether any
service builds. Saving the settings lists and reads everything again at
once. Containers are matched to apps by their
`com.docker.compose.project` label; Compose projects that match no app are
listed read-only as unmanaged, with the folder from their
`com.docker.compose.project.working_dir` label.

An app's status is `running` when every service Compose lists has a
running container, `partial` when some do, `stopped` when none do, and
`unknown` while Docker isn't ready or Compose can't read the app (the
reason is in `problems`). `GET /api/apps` is the whole list, also sent as
the `apps` event; `GET /api/apps/NAME` is one app, or 404
`app_not_found`. `alchemy app list` and `alchemy app show NAME` print
them. The code is in `bin/alchemyd/src/apps/`.

```sh
curl --unix-socket /run/user/1000/alchemy/alchemyd.sock http://alchemyd/api/apps
curl --unix-socket /run/user/1000/alchemy/alchemyd.sock -X PUT \
  -H 'Content-Type: application/json' -d '{"home":null,"extra":["/opt/stacks"]}' \
  http://alchemyd/api/settings/apps
```

### Creating and editing apps

Alchemy writes only an app's compose file and its `.env`, and never
leaves a broken one on disk. Each file is at most 256 KiB of UTF-8 text
without NUL bytes. A write is staged in a hidden
`.alchemy-tmp-NAME-XXXX` folder inside the app folder it goes to,
validated with `docker compose --project-directory DIR [-f FILE]
[--env-file FILE] config`, and only then moved into place with one
rename. If Compose rejects it, the answer is 422 `invalid` with
Compose's own message, staging paths removed, and nothing changes.
A staging folder left by a daemon that stopped mid-write is removed on
the first scan after the next start, or after the settings add its
folder, once it is over an hour old, is a real directory (never a
symlink), and is owned by the daemon's user.

- `POST /api/apps` (`{"name","folder","compose","env"}`) creates
  `FOLDER/NAME` holding `compose.yaml` and, if given, `.env`. `folder` is
  the home when `null` and must otherwise be one of the app folders; the
  home is created on first use. A folder that already exists, empty or
  not, or a name another app has, is 409 `conflict`: Alchemy never
  replaces a folder, since it may hold data. It answers 201 with the app.
- `GET /api/apps/NAME/files/compose` (or `env`) answers
  `{"kind","name","content","hash"}`, with `hash` (SHA-256, hex) also as
  the `ETag`. A missing `.env` is empty with a `null` hash.
- `PUT /api/apps/NAME/files/compose` (or `env`) takes
  `{"content","base_hash"}`. `base_hash` is the hash the edit started
  from, in the body rather than `If-Match` so the CLI and web UI send it
  the same way; if the file on disk no longer has it, checked before
  staging and again after Compose runs, the answer is 409 `conflict` and
  the hand edit stays. A compose file is validated against the app's own
  folder with its override file; a `.env` with `--env-file`. Only the
  edited file is replaced, keeping its mode and owner.
- Both take `"dry_run": true` to check and validate without writing.
- Both take `"start": true` to bring the app up after saving. alchemyd
  asks the apps task to look again, waits up to 10 seconds for a scan
  whose reads all started after the save (`State::rescan_apps` returns
  its generation, the apps task reports it done with
  `set_app_scan_done`), then runs `up` as `POST /api/apps/NAME/actions`
  would. The answer adds `"operation"`, the running operation, or
  `"start_error"`, `{"error","code"}` as that route would refuse with;
  either way the save stands and the status is 201 or 200. Without
  `start` neither field is sent. `start` with `dry_run` is 400.

`.env` is always written `0600`, whatever the umask, and a new app's
compose file `0644` in a `0755` folder. Symlinked app folders and files
are refused, never followed, and only apps in the app folders can be
edited. Bodies over 3 MiB are refused with 413. The code is in
`bin/alchemyd/src/apps/edit.rs` and `disk.rs`; validation is
`ComposeValidator` in `bin/alchemyd/src/compose/validate.rs`.

`alchemy app create NAME [--file compose.yaml] [--env-file PATH]
[--folder PATH] [--start]` reads `compose.yaml` in the current folder
unless told otherwise (`--file -` reads stdin). `alchemy app edit NAME [--env]` opens
a private `0600` copy in `$VISUAL` or `$EDITOR` (`vi` otherwise; the
value is split on spaces, never run by a shell), re-opens it with
Compose's message on top until Compose accepts it or it is left
unchanged, asks, then saves. On a conflict it keeps your copy and says
where; `--force` saves over the change on disk instead. Both take
`--dry-run` and `--yes`, and `--start` (not with `--dry-run`), which
follows the `up` alchemyd starts as `alchemy app up` does, with
`--json` printing the save's line and then each event. If alchemyd saved
but didn't start the app, the command prints the save and fails with
the reason, exit 1. On the web, Apps has "Add app" with Create app and
Create and start, and each app's page an Edit section with Save and
Save and start.

```sh
curl --unix-socket /run/user/1000/alchemy/alchemyd.sock -X POST \
  -H 'Content-Type: application/json' \
  -d '{"name":"hello","compose":"services:\n  web:\n    image: nginx\n","dry_run":true}' \
  http://alchemyd/api/apps
```

### Lifecycle actions

`POST /api/apps/NAME/actions` with `{"action":"up"}` (or `down`, `start`,
`stop`, `restart`, `pull`, `build`, `update`; `"force":true` acts on an
app that shares its Compose project with another) starts an operation
and answers 202 with it. Update pulls, then runs `up --detach`; for an
app that builds, the pull skips buildable services and a `build --pull`
runs between them. One operation runs per app at a time: a second action
is 409 `conflict`, as is one before Compose has read the app or while
Docker isn't running. Before Compose runs, the operation records the stop
intent (`up` for an update; nothing for `pull` and `build`), and leaves it
when it fails.

Every `docker compose` the daemon runs, for actions, reads, and
validation, is `docker compose --project-directory DIR ...` with no `-p`,
so Compose resolves the project as it would by hand (only validation adds
`-f` for its staged file). Its environment is an allowlist
(`bin/alchemyd/src/compose/env.rs`): `PATH`, `HOME`, the locale, `TMPDIR`,
`DOCKER_CONFIG`, and a few more pass through, with `DOCKER_HOST` set to
the engine the daemon found; `COMPOSE_*`, the other `DOCKER_*` variables,
proxies, and `SSH_AUTH_SOCK` never do.

`GET /api/operations/ID/events` streams the operation as server-sent
events: `operation` first and on every step change, `output` for each line
of output (from its first kept line, so a late follower misses nothing
still held), then `done`. Each step has a plain-words `label` and a
one-line `message`, such as "2 of 3 images downloaded". During a build,
the base64 logs in BuildKit's JSON status lines are decoded and split into
`output` lines tagged with their build step, `[2/3] RUN npm ci | LINE`
(the start of the step's digest before its name is known); a line still
open when its step ends is written then. Each output line is at most
64 KiB. The daemon keeps
the newest 5000 output lines per operation, and finished operations in
memory for an hour (at most 100); `GET /api/operations[?app=NAME]` lists
recent ones, newest first, from the state database too, which keeps the
latest 1000. The `operations` event carries the ones in memory.
`POST /api/operations/ID/cancel` stops one. Shutdown interrupts running
operations and stores them as `interrupted`; ones a crashed daemon left
running are marked so at the next start. `GET /api/apps/NAME/plan?action=down`
returns the command lines an action would run, changing nothing.

`alchemy app up|down|start|stop|restart|pull|build|update NAME` starts
one and follows it: each step on stderr as it starts or fails, the result
on stdout, and with `--json` every event as one line on stdout. `down`
asks first (`--yes` skips the question, and without a terminal it refuses
with exit 3); `--dry-run` prints the plan. A failed, cancelled, or
interrupted operation exits 1 with kind `operation_failed`. Ctrl-C stops
following, but the operation keeps running in the daemon.
`alchemy app cancel NAME|ID` cancels the app's running operation (found
with `?app=NAME`), or, for anything that is not an app name or is a name
with no operations, the operation with that id; with nothing running it
exits 1 with kind `nothing_to_cancel`. The app page
has the same actions, with Down, Pull, and Build under More actions. The
pure model is `alchemy_core::operation`, the manager
`bin/alchemyd/src/operations/`.

```sh
curl --unix-socket /run/user/1000/alchemy/alchemyd.sock -X POST \
  -H 'Content-Type: application/json' -d '{"action":"restart"}' \
  http://alchemyd/api/apps/blog/actions
curl -N --unix-socket /run/user/1000/alchemy/alchemyd.sock http://alchemyd/api/operations/ID/events
```

### Logs

`GET /api/apps/NAME/logs` streams an app's logs as server-sent events,
read from the Engine API, never a `docker compose logs` process. The
containers are the app's, as `/api/apps` lists them; `service=web`
limits them to one service (400 `bad_request` for one the app doesn't
have). It sends the last `tail` lines (200 by default, at most 5000, and
only the newest that fit in 2 MiB) across the containers in the order
Docker timestamped them, as one `log` event each, then `done`. At most 16
log streams are open at once across the port and the socket; past that
a request is 503 `busy`. With `follow=true` it sends `following` instead
of `done` and then each new line as it arrives, until the client leaves:
a container that stops is picked up again, without repeating a line, when
it starts, and one Compose creates later is followed from its first line.
A `done` with an `error` means the logs couldn't be read; a stream that
ends without `done` was cut off. Lines over 8 KiB are cut and end in `…`,
and bytes that aren't UTF-8 become U+FFFD. `alchemy app logs NAME
[SERVICE] [-f] [--tail N]` prints `service | line` (NDJSON with
`--json`), and the app page has a Logs section. The pure line handling
is in `alchemy_core::logs`, the reading in `bin/alchemyd/src/logs/`.

```sh
curl -N --unix-socket /run/user/1000/alchemy/alchemyd.sock 'http://alchemyd/api/apps/blog/logs?service=web&follow=true'
```

### Stopped or crashed

Every container, and every app, carries a `stop_reason`: running,
stopped by Alchemy, stopped outside Alchemy, crashed (with its exit code
and whether it ran out of memory), keeps crashing, or finished. The pure
rules live in `alchemy_core::engine::stop`. They read the container's
events (`docker stop` sends `kill`, `die`, then `stop`; an out-of-memory
kill sends `oom` then `die`; a process killed from the host sends only
`die`) and the last action Alchemy recorded for its Compose project
(`bin/alchemyd/src/intent.rs`), which is stored so it survives a restart.
Containers already stopped when the daemon connects are inspected once
for their exit code. A one-off job that exits 0 doesn't make its app
partly running.

To see each case, run an app and then:

```sh
docker compose stop web               # Stopped outside Alchemy
docker kill --signal=KILL blog-web-1  # Stopped outside Alchemy
sudo kill -9 "$(docker inspect -f '{{.State.Pid}}' blog-web-1)"  # Crashed (exit code 137), Linux
```

### Metrics

The daemon samples this machine and every running container a minute
after the last sample (the first right at startup, the second 10 seconds
later), whether or not anyone is looking. Machine figures come from
`sysinfo`, plus `/proc/stat` for raw CPU ticks on Linux; on macOS they
are the Mac's own, not the VM Docker Desktop runs containers in, and the
API, CLI, and UI say so. Container figures come from one-shot Engine API
stats, at most 4 at a time with a 5 second timeout each. CPU is a share
of the whole machine (or of Docker's VM), so an app's containers add up to
the machine's figure. A figure that could not be measured is `null`,
never 0.

History is stored in the state database in five tiers, each rolled up
from the one below and kept a little longer than its range:

| Range | Point every | Kept for |
| ----- | ----------- | -------- |
| `1h`  | 1 minute    | 2 hours  |
| `12h` | 10 minutes  | 13 hours |
| `24h` | 20 minutes  | 25 hours |
| `7d`  | 2 hours     | 8 days   |
| `30d` | 8 hours     | 31 days  |

A bucket is built once it closes, and only if the tier below has at least
half its points, so a stretch the daemon was stopped for stays a gap; it
catches up on restart. The pure logic lives in `alchemy_core::metrics`,
the collector in `bin/alchemyd/src/metrics/`.

`GET /api/metrics` is the latest sample, also sent as the `metrics`
event; `GET /api/metrics/history?range=24h` is one tier's points. Both
take `app=NAME` for one app's figures, by folder name like every other
app route: the daemon resolves it to the app's Compose project (404
`app_not_found` for an unknown app, no figures while Compose has not read
it yet). `project=NAME` still asks for a Compose project by name; giving
both is a 400. `alchemy machine stats` and `alchemy app stats NAME` print
them, and the Machines page charts them.

```sh
curl --unix-socket /run/user/1000/alchemy/alchemyd.sock 'http://alchemyd/api/metrics/history?range=24h&app=blog'
```

### Ports

The port registry lists every host port that is in use, reserved, or
declared, and who holds it. It is merged in `alchemy_core::ports` from
four sources, read fresh on each request:

- the ports Docker publishes for running containers (`in use`);
- port reservations in the state database (`reserved`);
- the ports apps' compose files publish, from the cached `docker compose
  config`, so Compose isn't run again (`declared` while the app is
  stopped);
- sockets other programs listen on (`in use`): `/proc/net` on Linux,
  and `netstat` on macOS.

A port a running container publishes is listed once, held by the
container, although `docker-proxy` or Docker Desktop also listens on it.
Ports without a fixed host port are left out. Holders are named as
`service web of app blog`, `container pihole`, `a reservation for
proxy`, or `another program (nginx, pid 42)`. Only the daemon user's
processes are named; on Linux others show as `another program`. On
macOS, `netstat` shows no sockets to an unsigned binary like a dev build
of alchemyd, so it falls back to `lsof`, which misses listeners owned by
root (the system `sshd`, `cupsd`). If listeners can't be read at all, the
list says so in `warnings`.

`GET /api/ports` returns the list, `alchemy machine ports [--json]`
prints it as a table, and the Machines page has a Ports panel. The code is
in `bin/alchemyd/src/ports.rs`.

```sh
curl --unix-socket /run/user/1000/alchemy/alchemyd.sock http://alchemyd/api/ports
```

Before `up`, `start`, `restart`, or `update`, the daemon checks the
ports the app's compose file publishes against the registry. A port held
by a running container of another app, a container outside Compose, a
reservation, or another program refuses the action with 409 `conflict`,
and the message names each one:

```
Port 8080/tcp for service app is in use by service web of app blog. Free that port, or force up to start wiki anyway.
```

The app's own containers and reservations never block it, and another
app's declared port only blocks while that app runs. `"force": true` on
the action skips the check: `alchemy app up NAME --force`, or Start anyway
under Advanced on the app page. Docker still refuses a port that is
really taken ("port is already allocated"); the check is there to name the
holder first. Save and start returns the refusal as `start_error`. `GET
/api/apps/NAME/plan?action=up` lists the conflicts in `conflicts`, and
`alchemy app up NAME --dry-run` prints a `port taken:` line for each. If
the registry can't be read, the daemon logs a warning and starts the app
anyway. The check is in `bin/alchemyd/src/operations/manager.rs`.

### Networks

Alchemy keeps one Docker network of its own, the bridge `alchemy`,
labelled `com.alchemy.managed=true`. While Docker is ready, the daemon
checks the networks at startup, 2 seconds after the engine or the
containers change, and every 30 seconds. If `alchemy` is missing, it is
created on the first `/24` from Docker's default pools (`172.17.0.0/16`
to `172.31.0.0/16`, then `192.168.0.0/16`), then from `10.200.0.0/16`,
that overlaps none of:

- the LAN: the IPv4 subnets of this machine's physical interfaces
  (`docker0`, `br-*`, `veth*`, `tun*`, `wg*`, `tailscale*`, and the other
  virtual interfaces metrics skip are left out);
- routes: `/proc/net/route` on Linux, `netstat -rn` on macOS, without the
  default route, routes inside a LAN subnet, and Docker's own;
- the default bridge and every other Docker network.

The subnet is saved in the `settings` table (`network.alchemy`) and
reused when the network has to be created again, while it is still free.
An `alchemy` network that already exists is never changed, even when it
overlaps, since containers may be on it; `docker network rm alchemy`
makes the daemon create it again on a free subnet within seconds.
Alchemy never relabels or replaces a network it didn't create, and never
edits compose files. Only IPv4 is checked.

The same check compares Docker's default bridge, its address pools, and
every Docker network with the LAN and routes. The pools are the engine's
configured `default-address-pools`, else those in
`/etc/docker/daemon.json` on Linux, else Docker's built-in defaults
(`pools_source`). Each overlap is a warning: `bridge_overlap`,
`pool_overlap`, `network_overlap`, or `alchemy_network_overlap`, plus
`no_free_subnet`, `create_failed`, `host_unreadable`, and
`engine_unreadable`. An app whose Compose networks overlap gets a
`network_overlap` problem.

A bridge or pool overlap comes with a fix: a `daemon.json` fragment that
moves the pools to `/24`s of `10.200.0.0/16` (or the default pools that
don't overlap, when that is taken too) and, for the bridge, sets `bip`.
It says where the settings go for the engine in use: `/etc/docker/daemon.json`
and `sudo systemctl restart docker` for Docker Engine, Settings, then
Docker Engine, for Docker Desktop. Networks that already exist keep their
subnets until they are removed and created again.

`GET /api/network` returns the whole report (`lan`, `routes`, `bridge`,
`pools`, `networks`, `alchemy` with its `state` and `subnet`, and
`warnings`). `alchemy status` prints each warning and its fix after the
Docker line, `status --json` adds `network`, and the Machines page shows
them too. The pure logic is in `alchemy_core::network`, the daemon side in
`bin/alchemyd/src/subnets/`.

```console
$ alchemy status
alchemyd 0.1.0 (protocol 1) at /run/user/1000/alchemy/alchemyd.sock
Docker ready: Engine 28.0.1, Compose v2.33.0
Warning: Docker's address pool 192.168.0.0/16 overlaps your local network 192.168.0.0/16, so a new network may be given addresses that hide devices on it.
  Fix: Add these settings to /etc/docker/daemon.json (create it if it doesn't exist), then restart Docker with `sudo systemctl restart docker`:
  {
    "default-address-pools": [
      { "base": "10.200.0.0/16", "size": 24 }
    ]
  }
  Networks that already exist keep their subnets until they are removed and created again.
```

### Web UI in debug vs release

The daemon serves `app/alchemy-web/dist`. Debug builds read it from disk, so
`vp run alchemy-web#build` is enough to see UI changes without rebuilding
Rust. Release builds embed it at compile time, so build the web UI **before**
`cargo build --release`.

## 4. The gate

CI runs these on every pull request. Run them before calling a change done:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check
pnpm ready   # format, lint, and type checks, web tests, web build
```

### Tests that need Docker

Everything in the gate runs without Docker. A few tests talk to the real
engine on this machine instead, so they are `#[ignore]`d and CI skips them.
Run them by name with `--ignored`:

```sh
cargo test -p alchemyd --test engine -- --ignored   # finds and pings the engine
cargo test -p alchemyd --test escape -- --ignored   # the escape hatch, below
```

The escape test (`bin/alchemyd/tests/escape.rs`) proves that Alchemy leaves
apps manageable by hand. It starts alchemyd on a temporary app home with
the real engine monitor, `docker compose config`, app editor, and
`docker compose` runner. One app is written by hand and named after its
folder; the other is created through `POST /api/apps` with a top-level
`name:`. It brings both up through `POST /api/apps/NAME/actions`. Then it
stops the daemon and, in each app's folder, runs `docker compose ps`,
`stop`, and `up --detach` with no `-p` or `-f`, checking each time that
they act on the same containers, with the same
`com.docker.compose.project` label, that Alchemy started. Finally a new
daemon must see the same containers running, and `down` through it removes
them. It needs `busybox:1.37`, pulled if missing, and takes a few seconds
once the image is present. A guard runs `docker compose down` in both
folders even when an assertion fails, and the temporary directory is
removed with it.

## 5. Release smoke test

```sh
scripts/release-smoke.sh
```

This builds the web UI and release binaries, runs `alchemyd` on a temporary
socket, state directory, and port `17240` (override with `SMOKE_PORT`), and
checks:

- the daemon creates its state database `0600` in a `0700` directory,
  and its TCP token `0600` beside it;
- `/api/health` responds on the port without signing in, while
  `/api/apps` and `/api/engine` are 401 without the token or with a wrong
  one, and `/api/engine` answers with it;
- `alchemy open --print` prints a sign-in link whose `/login` sets a
  private session cookie that `/api/apps` accepts, the link works only
  once, and the port refuses to issue launch codes;
- `/api/health` responds on the socket, where `/` is a JSON 404;
- a rebinding `Host`, a cross-origin form `POST`, and a cross-site `GET`
  are refused with 403;
- `/api/events` streams an `engine` event to the Tauri origin with the token;
- `/api/metrics` reports the machine, `/api/metrics/history` has a
  stored point, and `alchemy machine stats` prints them;
- `/api/operations` answers, and an action on an app that doesn't exist
  is refused with 404;
- `alchemy settings set app-home` points the daemon at an empty temporary
  folder, and `alchemy app list` then lists no apps and reports a folder
  with an invalid name as skipped, all without Docker;
- `/` serves the embedded web UI;
- `alchemy status` reaches the daemon and reports the same version;
- `alchemy-helper` refuses to run without root;
- a second daemon on the same state directory refuses to start, before
  binding its socket, and the first keeps serving;
- the daemon shuts down cleanly on `SIGTERM` and removes its socket and
  token;
- the daemon refuses a corrupt state database before binding its socket.

Set `SMOKE_SKIP_BUILD=1` to reuse an existing build. CI runs it on Linux and
macOS.

### Home-network test

```sh
HOME_NET_CONFIRM=1 scripts/home-network.sh
```

This proves the [network checks](#networks) on a real engine, on Linux. For
each of three home networks, `10.0.1.5/24`, `172.17.0.5/16` (on top of
Docker's default bridge), and `192.168.1.5/16` (on top of Docker's
`192.168.0.0/16` pool), it puts the address on a dummy interface,
`alchemy-lan0`, starts `alchemyd` with a fresh state directory, waits for
`/api/network` to finish checking Docker, and checks that:

- the address's subnet is among the LAN subnets, and the pools are
  Docker's defaults;
- the `alchemy` network is `ready`, exists in Docker with that subnet and
  the `com.alchemy.managed` label, overlaps no LAN subnet, route, or the
  bridge, and is `172.18.0.0/24` on an engine with only its default
  networks;
- the warnings are exactly none, `bridge_overlap` and `pool_overlap` for
  `172.17.0.0/16`, and `pool_overlap` for `192.168.0.0/16`, with their
  messages, and the fix moves the pools to `10.200.0.0/16` and, for
  `172.17.0.0/16`, the bridge to a free subnet, worded for the engine's
  flavor;
- `alchemy status` prints the warnings and the fix once, and
  `status --json` carries the report.

Between and after the runs it stops the daemon, deletes the interface, and
removes the `alchemy` network. It needs `ip`, `jq`, `curl`, `sudo` (unless
run as root), and a user that can reach Docker. Because it changes the
machine's interfaces and removes the `alchemy` network, it refuses to run
unless `CI=true` or `HOME_NET_CONFIRM=1`; run it in a VM or a throwaway
machine. Set `HOME_NET_SKIP_BUILD=1` to reuse `target/release`. CI runs it
on Linux after the release smoke test, reusing its build. macOS is covered
by the table tests on the subnet picker and the `netstat` and `lsof`
parsers.

To run it on a Mac, use a privileged container running its own engine,
on a Docker network away from `172.17.0.0/16` so the container's own
address doesn't overlap the networks under test, with its own target
directory:

```sh
docker network create --subnet 10.1.0.0/16 home-net
docker run --rm -it --privileged --network home-net -v "$PWD:/src:ro" -w /src \
  -e CARGO_TARGET_DIR=/target -v home-net-target:/target rust:1.99.1 bash
# inside:
apt-get update && apt-get install -y docker.io iproute2 jq
(dockerd >/var/log/dockerd.log 2>&1 &) && sleep 5
CI=true scripts/home-network.sh
```

## 6. Layout

| Path       | What it is                                               |
| ---------- | -------------------------------------------------------- |
| `crates/`  | Library crates                                           |
| `bin/`     | `alchemyd`, `alchemy`, `alchemy-helper`                  |
| `app/`     | `alchemy-web` (React UI) and `alchemy-gui` (Tauri shell) |
| `docs/`    | Project docs; `docs/site` is the documentation website   |
| `scripts/` | Development scripts                                      |
| `.github/` | CI workflows and shared actions                          |

## 7. Adding a feature page

A feature page is a route, the API types it reads, and the shared
components. The layout, header, daemon-unreachable banner, error boundary,
live updates, and theme already apply to every route; nothing else needs
wiring. The examples add a Volumes page; `src/` means `app/alchemy-web/src/`.

### Route

Create `src/routes/volumes.tsx`:

```tsx
export const volumesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/volumes",
  component: VolumesPage,
});

function VolumesPage() {
  return <PageHeader title="Volumes" />;
}
```

Add `volumesRoute` to `routeTree` in `src/router.ts`. A top-level page also
gets an entry in `NAV` in `src/components/Layout.tsx`, with its path added to
`NAV`'s `to` type, and its label added to the sidebar test in
`src/router.test.tsx`, which pins the sidebar's words. `PageHeader` puts the
title, and any actions passed as children, in the layout's header. A nested
route sets `getParentRoute` to its parent, which renders `<Outlet />`.

### API type

The daemon owns the data; the page only reads it.

1. Add the type to `crates/alchemy-api/src/lib.rs` with a wire-shape test,
   and its handler under `/api` in `bin/alchemyd/src/http.rs` with a test.
2. Mirror the type by hand in `src/lib/api.ts`.
3. Read it with `useQuery`, nothing else:

```tsx
const volumes = useQuery({
  queryKey: ["volumes"],
  queryFn: ({ signal }) => request<Volume[]>("GET", "/volumes", { signal }),
});
```

Mutations call `request("POST" | "PUT" | "DELETE", path, { body })`, which
sends `Content-Type: application/json` even without a body, as alchemyd
requires. A failed request rejects with an `ApiError`, whose `message` is
safe to show. A route needs no wiring of its own for the Host, Origin,
and content-type checks or for signing in: nesting it under `/api` in
`http.rs` puts it behind them (`bin/alchemyd/src/guard.rs`, `auth.rs`),
and `request` signs the shell's requests.

### Live updates

When the daemon pushes changes, add a topic instead of polling:

1. `crates/alchemy-api`: an `ApiEvent` variant, its arms in `ApiEvent::name`
   and `EventData`'s `Serialize` impl, and a wire-shape test.
2. `bin/alchemyd/src/events.rs`: one entry in `topics()`, such as
   `Topic::watch(State::watch_volumes, ApiEvent::Volumes)`.
3. `src/lib/api.ts`: the variant in `ApiEvent` and its name in `EVENT_NAMES`.
4. `src/lib/queries.ts`: one `liveQueries` entry, with the REST read above as
   its `queryFn` and `staleTime: Infinity`.

`rustc` and `tsc` flag a missing match arm or a missing step 3 or 4, but
nothing flags a missing `topics()` entry.

The page reads `useQuery(liveQueries.volumes)`. Each event replaces the
query's data, and every reconnect resends it. While the daemon is
unreachable the query keeps its last value; gate on `useDaemonConnection()`
if stale data would mislead, as Machines does.

### Components and styling

Show every state with the shared components in `src/components/`:

```tsx
if (volumes.isPending) return <Loading label="Loading volumes…" />;
if (volumes.isError) {
  return (
    <ErrorState
      error={volumes.error}
      title="Couldn't load your volumes"
      onRetry={() => void volumes.refetch()}
    />
  );
}
if (volumes.data.length === 0) return <EmptyState icon={HardDrive} title="No volumes yet" />;
```

Use `Notice` for status, `Button`, `ConfirmDialog` before anything
destructive, `ExternalLink` for every link outside Alchemy, and `Advanced`
for technical detail. `pnpm dev` shows them all at `/dev/components`; add a
new shared component there too. A page that throws is replaced by an error
state inside the layout, with no wiring.

Style with theme tokens only: `surface`, `surface-raised`, `surface-muted`,
`border`, `text`, `muted`, `accent`, `accent-contrast`, `ok`, `warn`, and
`error`, as in `text-muted`, `border-border`, or `bg-error/10`. Tailwind's
palette is off, and `src/palette.test.ts` fails on classes like
`text-stone-400`. The layout already sets the page background and text
colour; a card is `rounded-lg border border-border bg-surface-raised`. If no
token fits, add one to both blocks in `src/style.css`.

### Tests

`src/routes/volumes.test.tsx` stubs `fetch` and `EventSource` with
`vi.stubGlobal`, then renders the whole app with `renderAt("/volumes")` from
`src/test/render.tsx`. To push a live event, call
`FakeEventSource.last().emit("volumes", JSON.stringify(data))` inside `act`
and wait for the update with `findBy*`. `src/routes/machines.test.tsx` shows
each pattern. Then run [the gate](#4-the-gate).

## 8. Adding a CLI command

A command is a module, a `Command` variant, and one dispatch arm, plus a
route in `alchemy-api` and `alchemyd` if it needs data the daemon doesn't
serve yet ([Talking to alchemyd](#talking-to-alchemyd)). Usage
errors, `--json` errors, exit codes, `--daemon` and `--socket`, the version
handshake, ctrl-c, help, completions, and man pages already apply to every
command; nothing else needs wiring. The examples walk through a simplified `alchemy app list` (the real one,
in `src/commands/app/list.rs`, has no `--all`);
paths are relative to `bin/alchemy/`.

### Naming

Commands are a noun, then a verb, parallel to the web UI's sidebar:
`alchemy status` (Home), `alchemy app <verb>`, `alchemy machine <verb>`,
`alchemy settings <verb>`. Nouns are singular and use the UI's words: app
and machine, never stack, container, or node. Tools that aren't about what
Alchemy manages, such as `completions` and `version`, are single words at
the top level. A noun arrives with its first real verb; don't add an empty
group to fill in later.

### Module

Create `src/commands/app/list.rs`:

```rust
/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "app list";

/// Arguments to `alchemy app list`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// Show stopped apps too.
    #[arg(long)]
    pub all: bool,
}

/// Run `alchemy app list`.
pub(crate) async fn run<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    args: Args,
) -> Result<(), ClientError> {
    let path = if args.all { "/api/apps?all=true" } else { "/api/apps" };
    let apps = ctx.get::<Vec<App>>(path).await?;
    for line in app_lines(&apps, ctx.json()) {
        ctx.out().line(line)?;
    }
    Ok(())
}
```

A command with no flags yet still takes an empty `pub struct Args {}`, so
flags can be added later without touching the wiring.

Then register it:

1. `pub mod list;` in its parent. A top-level command goes in
   `src/commands/mod.rs`. A noun is `src/commands/app.rs`, holding
   `pub struct Args` with a `#[command(subcommand)]` field and a `Verb`
   enum with one variant per verb, such as `List(list::Args)`.
2. A variant on `Command` in `src/lib.rs`, such as
   `App(commands::app::Args)`. Its doc comment is the help text.
3. One arm in `run_with`, which builds the `Context` for that command only,
   with the module added to the `use crate::commands::{..}` list above it.
   `src/commands/version.rs` is a small top-level example:

```rust
Command::Version(args) => version::run(&mut context(out, version::NAME)?, args).await,
Command::App(args) => match args.command {
    app::Verb::List(args) => {
        let mut context = context(out, app::list::NAME)?;
        app::list::run(&mut context, args).await
    }
},
```

A command that never needs the daemon, like `completions`, takes `out` and
its `Args` instead and doesn't call `context`, so it still works when
`ALCHEMY_DAEMON` is invalid.

### Talking to alchemyd

Commands reach the daemon only through `Context`, never the socket. The
CLI speaks the same `/api` routes as the web UI, over HTTP on the Unix
socket. The `Daemon` trait (`src/daemon.rs`) works at the HTTP level, a
status and a body or raw server-sent events, and has the real
`HttpDaemon` (`src/daemon/http.rs`) and, for tests, `FakeDaemon`.

- `ctx.daemon_info()` is the daemon's version and API version, from a
  `GET /api/version` handshake that the first request performs once.
- `ctx.get::<T>(path)` sends `GET path` and decodes the JSON body as `T`,
  an `alchemy-api` type. `ctx.put::<B, T>(path, &body)` sends `body` as
  JSON the same way; every request but `GET` declares
  `Content-Type: application/json`, as alchemyd requires.
- `ctx.events()` follows `GET /api/events` and returns a `Subscription`.
  Its `next()` gives the next `ApiEvent`, skipping event names this build
  doesn't know, and `Ok(None)` when the daemon ends the stream; a command
  that follows changes returns `ClientError::Disconnected` then, as
  `status --watch` does. The SSE parser is `src/sse.rs`.

`Context` turns error statuses into errors: a 404 `not_found` or 405
`method_not_allowed` means the daemon predates the route and becomes
"alchemyd 0.1.0 is too old for `app list`" (exit 76); any other
`ErrorResponse` becomes `alchemyd: <error>` (exit 1); a body that doesn't
decode is a `protocol` error (exit 76). Each request times out after 10
seconds; a stream only until its first event.

A new route is a contract change in `crates/alchemy-api`: add its types
with a wire-shape test in `tests/wire_stability.rs`, mirror them in
`app/alchemy-web/src/lib/api.ts`, and add its handler to `api()` in
`bin/alchemyd/src/http.rs`, before the 405 fallback. The socket and the TCP
port share that router, so the CLI and the web UI see the route at once.
Adding one needs no `API_VERSION` bump, because an older daemon answers it
with `not_found`.

### Output

Results go to stdout through `ctx.out().line(..)`, which flushes each line
so `| head` and followers see it at once. Prompts and errors go to stderr,
and a command never writes either itself. Never `println!`.

Render text and JSON in one pure function that takes `ctx.json()`, such as
`status_lines` in `src/commands/status.rs`, and table-test it. With
`--json`, print one document on one line, or one per line for a stream.
JSON output is a contract like the HTTP API: add fields, never rename or
remove one. A type serialized into it, such as `EngineStatus` in
`status --json`, brings its wire shape into that contract. A command whose
output has no JSON form, like `completions`, ignores `--json` rather than
rejecting it.

`ctx.out().color()` says whether ANSI styling is allowed: only on a
terminal, without `NO_COLOR`, and never with `--json`. No command styles
output yet, so it carries `expect(dead_code)`; delete that with the first
one that does.

### Errors and exit codes

A command returns `Err(ClientError)` and never prints it: `main` reports
it on stderr as text,

```console
alchemy: cannot reach alchemyd: /run/user/1000/alchemy/alchemyd.sock: No such file or directory (os error 2)
hint: start alchemyd, or point --socket at the socket it listens on
```

or, with `--json`, as one line:

```text
{"error":{"hint":"start alchemyd, or point --socket at the socket it listens on","kind":"daemon_unreachable","message":"cannot reach alchemyd: …"}}
```

The message says what failed, then why, in lower case without a final full
stop or the `alchemy: ` prefix. The hint is the next step, kept out of the
message. `kind` is a contract.

| Exit | When                                                    | `kind`                                                                         |
| ---- | ------------------------------------------------------- | ------------------------------------------------------------------------------ |
| 0    | It did what was asked                                   |                                                                                |
| 1    | It failed, for example alchemyd refused it              | `daemon`, `runtime`, `operation_failed`, `nothing_to_cancel`, `editor`, `file` |
| 2    | The command line or daemon target is invalid            | `usage`, `remote_daemon`, `invalid_target`                                     |
| 3    | Confirmation was declined, or needed without a terminal | `declined`, `confirmation_required`                                            |
| 69   | alchemyd is unreachable or went away                    | `daemon_unreachable`, `disconnected`                                           |
| 74   | Writing the output failed                               | `output`                                                                       |
| 76   | alchemy and alchemyd don't speak the same protocol      | `daemon_too_old`, `protocol`, `unexpected_response`, `connection_closed`       |
| 130  | Interrupted with ctrl-c                                 | `interrupted`                                                                  |

Ctrl-c and a closed stdout (`| head`) only set the exit code. Most commands
need no new error. Add a `ClientError` variant only when a script would
branch on it; `exit`, `kind`, and `hint` match every variant, so it won't
compile until it has all three. Add its row to
`every_variant_has_an_exit_code_text_and_json` in `src/error.rs`.

### Commands that change things

A command that changes or removes something flattens the shared safety
flags into its `Args`; one that changes nothing doesn't, so `--yes` on it is
a usage error.

```rust
/// The shared safety flags.
#[command(flatten)]
pub safety: Safety,
```

- `--dry-run` shows alchemyd's plan and changes nothing. The daemon computes
  the plan; the CLI only renders it.
- `--force` overrides a safety check in the daemon. It never skips the
  question.
- `--yes` skips the question. Without a terminal and without `--yes`, the
  command refuses with exit 3 rather than waiting.
- None of them deletes data. A command that can takes its own explicit flag
  for that.

Its `run` also takes the prompt, and confirms before anything else:

```rust
pub(crate) async fn run<D: Daemon, W: Write, P: Prompt>(
    ctx: &mut Context<D, W>,
    prompt: &mut P,
    args: Args,
) -> Result<(), ClientError> {
    match safety::confirm(prompt, &args.safety, NAME, "Remove the app web?").await? {
        Mode::DryRun => { /* ask alchemyd for its plan and print it */ }
        Mode::Execute => { /* send the request, passing args.safety.force */ }
    }
}
```

Its arm in `run_with` passes `prompt`. alchemyd's plan request for
lifecycle actions is `GET /api/apps/NAME/plan?action=ACTION`, an
`ActionPlan` in `alchemy-api`; `src/commands/app/action.rs` shows the
whole flow, and a new destructive command adds its own plan route.
`app create` and `app edit` instead send `"dry_run": true`, which
validates without writing. The hidden `safety-probe` command in
`src/commands/safety_probe.rs` is the smallest example, and its JSON
isn't a contract.

### Help, completions, and man pages

The variant's doc comment is the command's help, its man page, and its
description in completions; its first paragraph is the one-line summary.
Doc comments on `Args` fields are each flag's help. Where the rustdoc and
the help must differ, set `help = "..."`, as `--daemon` does. The new
command appears in `alchemy --help`, `alchemy completions`, and
`generate-man`'s pages with no further wiring, but the tests pin the set of
man pages: add `alchemy-app-list.1` (and `alchemy-app.1` for a new noun) to
`one_page_per_visible_command` in `src/commands/generate_man.rs` and to
`generated_man_pages_render` in `tests/completions_and_man.rs`.

To hide a command, put `#[command(hide = true)]` on its variant; it still
runs. A hidden flag is `#[arg(hide = true)]`. A command only for tests also
goes behind the `test-command` feature, as `safety-probe` does.

### Tests

Unit-test `run` in the command's module against `FakeDaemon`, a script of
requests, by method and path, and replies, with `Context::buffered`:

```rust
let fake = FakeDaemon::new()
    .version("1.2.3", 1)
    .ok(Method::GET, "/api/apps", &vec![web()]);
let mut ctx = Context::buffered(fake, false, NAME);
run(&mut ctx, Args::default()).await.unwrap();
assert_eq!(ctx.written(), "web  running\n");
assert_eq!(ctx.daemon().requests(), ["GET /api/version", "GET /api/apps"]);
```

`.error(method, path, status, code, message)` answers with an
`ErrorResponse`, `.fail(method, path, err)` fails a request as the
transport would, and `.events(items)` scripts `/api/events` with raw
events built by `fake::sse(name, data)`. A request out of order panics.
Cover the text and JSON forms and each error. For a command that changes
things, pass a `FakePrompt::tty([answers])` or `FakePrompt::no_tty()` and
assert on `requests()` that a refusal sends nothing and a dry run sends no
change. `src/commands/status.rs` and `src/commands/safety_probe.rs` show
each pattern.

Then wire it end to end in `tests/cli_e2e.rs`, which runs a real alchemyd
on a temporary socket with `Daemon::start`. Build the command line with
`Cli::try_parse_from` and call `alchemy::run`, or `alchemy::run_with` with
a `FakePrompt`. `alchemy_bin` runs the real binary to check stdout, stderr,
and the exit code.

Finally, run [the gate](#4-the-gate). It builds `alchemy` with the
`test-command` feature, so it never lints what users get. Check that too:

```sh
cargo clippy -p alchemy --lib --bins --locked -- -D warnings
```

## 9. Adding an app action

An app action runs through every layer, but most of them are tables that
the compiler walks you through. The examples add a `pause` action; paths
are from the repository root.

1. **The model** (`crates/alchemy-core/src/operation.rs`). Add the variant
   to `Action`, `Action::ALL`, and `as_str`, and a `StepCommand` if it
   runs a `docker compose` subcommand no other action does. Give each new
   step a label in `step_label`, decide in `intent_action` what stop
   intent it records (`None` if it leaves containers alone, as `pull`
   does), and, for a new `StepCommand`, its verb in the progress message
   in `operation/track.rs` ("1 of 2 containers paused"). `Action` is
   exhaustive, so `cargo build` lists every match that needs an arm. Each
   of these functions is table-tested in its module; add the rows.
2. **The plan** (`bin/alchemyd/src/compose/plan.rs`). Add its steps to
   `ComposePlan::new`, with any flags, and a row to `plans_every_action`.
   Steps run in order, and an operation stops at the first that fails.
3. **The wire** (`crates/alchemy-api`). `Action` and `StepCommand` are
   re-exported there, so a new variant is a contract addition: add its
   rows to the action and step tests in `tests/wire_stability.rs`, and
   the name to `Action` (and `StepCommand`) in
   `app/alchemy-web/src/lib/api.ts`. No `API_VERSION` bump: an older
   daemon refuses the unknown action with 400.
4. **The CLI** (`bin/alchemy`). Add a `Verb` variant taking
   `action::Args` in `src/commands/app.rs`, whose doc comment is its help;
   its arm in `run_with` in `src/lib.rs`; and its name, progress word, and
   result in the matches in `src/commands/app/action.rs`. If it removes
   anything, make it ask first, as `down` does there. Add
   `alchemy-app-pause.1` to the man page lists, as in
   [Help, completions, and man pages](#help-completions-and-man-pages).
5. **The web UI** (`app/alchemy-web/src/components/AppActions.tsx`). Add
   its words to `BUTTONS`, `RUNNING`, and `DONE` (`tsc` fails until all
   three have it), and its button to `PRIMARY` or under More actions; a
   destructive one goes in a `ConfirmDialog`. Cover it in
   `src/routes/app-actions.test.tsx`.
6. **The docs.** Add it to [Lifecycle actions](#lifecycle-actions) and to
   the user guide's action table in `docs/site/src/pages/docs/apps.md`.

Nothing else needs wiring: the route, the one-operation-per-app rule,
progress, history, cancel, and the `operations` event apply to every
action. `bin/alchemyd/tests/operations.rs` runs actions end to end
against a `FakeComposeRunner` scripted per action with `with_script`;
add a case there when the action's steps or intent are new. Then run
[the gate](#4-the-gate), and the escape test if it changes what Compose
runs.
