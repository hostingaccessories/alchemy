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

The CLI honours `--socket` and `ALCHEMY_SOCKET` the same way.
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

### Shell completions

`alchemy completions <bash|zsh|fish>` writes a completion script to stdout.
It never contacts the daemon.

```sh
alchemy completions bash > ~/.local/share/bash-completion/completions/alchemy
alchemy completions zsh > ~/.zfunc/_alchemy   # with ~/.zfunc on $fpath
alchemy completions fish > ~/.config/fish/completions/alchemy.fish
```

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

## 8. Adding a CLI command

A command is a module, a `Command` variant, and one dispatch arm. Usage
errors, `--json` errors, exit codes, `--daemon` and `--socket`, the version
handshake, ctrl-c, help, completions, and man pages already apply to every
command; nothing else needs wiring. The examples add `alchemy app list`;
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
    let apps = match ctx.request(&Request::Apps { all: args.all }).await? {
        Response::Apps(apps) => apps,
        other => return Err(ClientError::from_response(other)),
    };
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
`Daemon` trait (`src/daemon.rs`) has the real `IpcDaemon` and, for tests,
`FakeDaemon`.

- `ctx.daemon_info()` is the daemon's version and protocol, from a
  handshake that the first request performs once.
- `ctx.request(&req)` returns one response. Turn one of any other variant
  into an error with `ClientError::from_response`, as above.
- `ctx.stream(&req)` returns a `Subscription`. Its `next()` gives
  `Ok(None)` when the daemon ends the stream; a command that follows
  changes returns `ClientError::Disconnected` then, as `status --watch`
  does.

`Context` turns error responses into errors: `BadRequest` means the daemon
predates the request and becomes "alchemyd 0.1.0 is too old for
`app list`" (exit 76); anything else becomes `alchemyd: <message>` (exit 1).
Each request times out after 10 seconds; a stream only until its first
response.

A new request is a contract change in `crates/alchemy-ipc`: add the
`Request` and `Response` variants with a wire-shape test in
`tests/wire_stability.rs`, and answer it in `dispatch` in
`bin/alchemyd/src/ipc.rs`. Adding one needs no `PROTOCOL_VERSION` bump,
because an older daemon answers it with `BadRequest`.

### Output

Results go to stdout through `ctx.out().line(..)`, which flushes each line
so `| head` and followers see it at once. Prompts and errors go to stderr,
and a command never writes either itself. Never `println!`.

Render text and JSON in one pure function that takes `ctx.json()`, such as
`status_lines` in `src/commands/status.rs`, and table-test it. With
`--json`, print one document on one line, or one per line for a stream.
JSON output is a contract like the IPC and HTTP APIs: add fields, never
rename or remove one. A type serialized into it, such as `EngineStatus` in
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

| Exit | When                                                    | `kind`                                                                   |
| ---- | ------------------------------------------------------- | ------------------------------------------------------------------------ |
| 0    | It did what was asked                                   |                                                                          |
| 1    | It failed, for example alchemyd refused it              | `daemon`, `runtime`                                                      |
| 2    | The command line or daemon target is invalid            | `usage`, `remote_daemon`, `invalid_target`                               |
| 3    | Confirmation was declined, or needed without a terminal | `declined`, `confirmation_required`                                      |
| 69   | alchemyd is unreachable or went away                    | `daemon_unreachable`, `disconnected`                                     |
| 74   | Writing the output failed                               | `output`                                                                 |
| 76   | alchemy and alchemyd don't speak the same protocol      | `daemon_too_old`, `protocol`, `unexpected_response`, `connection_closed` |
| 130  | Interrupted with ctrl-c                                 | `interrupted`                                                            |

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

Its arm in `run_with` passes `prompt`. The first such command must delete
the `expect(unused_variables)` on `run_with`, which stands in until one
exists. alchemyd has no plan request yet: the first destructive command
adds one to `alchemy-ipc`, along with its JSON shape. The hidden
`safety-probe` command in `src/commands/safety_probe.rs` shows the flow,
and its JSON isn't a contract.

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
requests and replies, with `Context::buffered`:

```rust
let fake = FakeDaemon::new()
    .version("1.2.3", 1)
    .reply(Request::Apps { all: false }, Response::Apps(vec![web()]));
let mut ctx = Context::buffered(fake, false, NAME);
run(&mut ctx, Args::default()).await.unwrap();
assert_eq!(ctx.written(), "web  running\n");
assert_eq!(ctx.daemon().requests(), [Request::Version, Request::Apps { all: false }]);
```

`.fail(req, err)` fails a request as the transport would, and
`.stream(req, items)` scripts a stream. A request out of order panics.
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
