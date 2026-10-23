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
| `--daemon` | `ALCHEMY_DAEMON` | CLI only: the daemon as a URL; only `unix:///path` for now         |

The CLI honours `--socket` and `ALCHEMY_SOCKET` the same way, and takes
`--json` for machine-readable output. `--daemon unix:///path/to/alchemyd.sock`
is the same as `--socket /path/to/alchemyd.sock`; passing both is a usage
error. Precedence is `--daemon` or `--socket`, then `ALCHEMY_DAEMON`, then
`ALCHEMY_SOCKET`, then the default. Other schemes, such as `https://`, are
refused with exit 2 and nothing is contacted: remote daemons need
authentication, which arrives in M13
([#16](https://github.com/hostingaccessories/alchemy/issues/16)).

### Remote daemon over SSH

Until then, reach a daemon on another machine by forwarding its socket over
SSH, as the user that runs alchemyd there, and pointing `--socket` at the
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

Mutations call `request("POST" | "PUT" | "DELETE", path, { body })`. A failed
request rejects with an `ApiError`, whose `message` is safe to show.

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
