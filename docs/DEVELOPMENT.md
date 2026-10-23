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
| `cargo run -p alchemyd`          | Daemon on `127.0.0.1:7240` plus the IPC socket                       |
| `cargo run -p alchemy -- status` | CLI; talks to the daemon over IPC                                    |
| `pnpm dev`                       | Web UI dev server on `localhost:5173`, proxying `/api` to the daemon |
| `pnpm gui`                       | Tauri desktop shell in dev mode (starts the web dev server)          |
| `pnpm docs`                      | Documentation site dev server                                        |

Daemon settings, as flags or environment variables:

| Flag       | Variable         | Default                                                            |
| ---------- | ---------------- | ------------------------------------------------------------------ |
| `--listen` | `ALCHEMY_LISTEN` | `127.0.0.1:7240`                                                   |
| `--socket` | `ALCHEMY_SOCKET` | `$XDG_RUNTIME_DIR/alchemy/alchemyd.sock`, else a per-user temp dir |
|            | `ALCHEMY_LOG`    | `info` (any `tracing` filter, e.g. `alchemyd=debug`)               |

The CLI honours `--socket` and `ALCHEMY_SOCKET` the same way, and takes
`--json` for machine-readable output.

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

The engine tests use fakes and run without Docker. One ignored test talks to
the real engine on this machine:

```sh
cargo test -p alchemyd --test engine -- --ignored
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

## 5. Release smoke test

```sh
scripts/release-smoke.sh
```

This builds the web UI and release binaries, runs `alchemyd` on a temporary
socket and port `17240` (override with `SMOKE_PORT`), and checks:

- `/api/health` and `/api/engine` respond;
- `/api/events` streams an `engine` event to the Tauri origin;
- `/` serves the embedded web UI;
- `alchemy status` reaches the daemon and reports the same version;
- `alchemy-helper` refuses to run without root;
- the daemon shuts down cleanly on `SIGTERM` and removes its socket.

Set `SMOKE_SKIP_BUILD=1` to reuse an existing build. CI runs it on Linux and
macOS.

## 6. Layout

| Path       | What it is                                               |
| ---------- | -------------------------------------------------------- |
| `crates/`  | Library crates                                           |
| `bin/`     | `alchemyd`, `alchemy`, `alchemy-helper`                  |
| `app/`     | `alchemy-web` (React UI) and `alchemy-gui` (Tauri shell) |
| `docs/`    | Project docs; `docs/site` is the documentation website   |
| `scripts/` | Development scripts                                      |
| `.github/` | CI workflows and shared actions                          |
