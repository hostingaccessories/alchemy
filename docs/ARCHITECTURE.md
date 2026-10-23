# Alchemy: Architecture

> Pure logic lives in library crates. I/O and OS calls are pushed to the
> edges behind traits.

These are the rules for anyone, human or agent, changing code in this
repository. Most are enforced by lints, `cargo-deny`, or tests; the rest are
checked in review.

---

## 1. The one rule

Everything below follows from the line at the top of this page.

1. **Pure code does no I/O.** No filesystem, network, process spawning,
   clock, or environment reads. It is synchronous, runtime-free, and
   unit-testable with in-memory values. `alchemy-core` is the exemplar; keep
   it that way.
2. **Side effects go behind traits.** When logic needs Docker, the clock, the
   filesystem, or another process, it depends on a trait. Tests inject a fake;
   the real implementation lives at the edge. Don't create traits ahead of
   need: add one when the first real side effect arrives.
3. **OS decisions are pure helpers.** Keep the decision separate from the
   effect so it can be tested without the OS. For example,
   `alchemy_ipc::runtime_dir_from(xdg, temp, uid)` decides the path;
   `default_socket_path()` reads the environment and calls it.
4. **Binaries are thin.** `alchemyd` and `alchemy` are a library plus a small
   `main.rs`. `main` parses arguments, sets up logging and the runtime, and
   calls into the library. Interesting logic belongs in a crate with tests.
5. **The daemon is the single source of truth.** `alchemyd` owns all runtime
   state. The CLI, web UI, and desktop shell are clients and never
   reimplement daemon logic.

## 2. Crates and dependency direction

| Crate            | Path                  | Role                                                                          |
| ---------------- | --------------------- | ----------------------------------------------------------------------------- |
| `alchemy-core`   | `crates/alchemy-core` | Pure domain model (stacks and their invariants)                               |
| `alchemy-ipc`    | `crates/alchemy-ipc`  | IPC protocol, framing, codec; socket transport behind the `transport` feature |
| `alchemy-api`    | `crates/alchemy-api`  | HTTP API types shared by the daemon and its web clients                       |
| `alchemyd`       | `bin/alchemyd`        | Daemon: HTTP API, embedded web UI, IPC socket                                 |
| `alchemy`        | `bin/alchemy`         | CLI, an IPC client                                                            |
| `alchemy-helper` | `bin/alchemy-helper`  | Privileged one-shot                                                           |
| `alchemy-gui`    | `app/alchemy-gui`     | Tauri desktop shell around the web UI                                         |
| `alchemy-web`    | `app/alchemy-web`     | React web UI, served by the daemon and bundled into the shell                 |

Internal dependencies flow strictly downhill, with no cycles:

```
alchemy-core ◄── alchemy-ipc ◄── alchemyd, alchemy, alchemy-helper
alchemy-core ◄── alchemy-api ◄── alchemyd
```

- `alchemy-core` depends on no other `alchemy-*` crate.
- Libraries never depend on binaries.
- Clients depend on a protocol crate, not on daemon internals. `alchemy`
  may use `alchemyd` as a dev-dependency for end-to-end tests only.

## 3. Two front doors, two contracts

The daemon serves the same state through two interfaces:

- **IPC socket** (`alchemy-ipc`): used by the CLI and the helper. Per-user,
  `0o600`, in the user's runtime directory.
- **HTTP `/api`** (`alchemy-api`): used by the web UI and the desktop shell.
  Loopback only until it has authentication.

Both are stable contracts:

- Change them additively: add fields and variants; never rename or remove
  one silently.
- Bump `PROTOCOL_VERSION` on any breaking IPC change.
- Wire-shape tests (`crates/alchemy-ipc/tests/wire_stability.rs`) pin the
  bytes. If one fails, treat it as an accidental contract change, not as a
  test to update.
- `app/alchemy-web/src/lib/api.ts` mirrors `alchemy-api` by hand. Change
  both together.

## 4. Hard rules

- **No `unsafe`.** `unsafe_code` is `forbid` workspace-wide.
- **No `unwrap`, `expect`, `panic!`, `todo!`, `dbg!`, or panicking indexing
  in non-test code.** These are clippy `deny` lints. Use `?`, `get(i)`,
  pattern matching, and total functions. Test modules and files opt out
  explicitly:

  ```rust
  #![allow(
      clippy::unwrap_used,
      clippy::expect_used,
      clippy::panic,
      clippy::indexing_slicing
  )]
  ```

- **Errors:** typed `thiserror` enums in libraries, with variants a caller
  can branch on. `anyhow` only in binaries, never in a library's dependency
  graph.
- **Async only at the edge.** Pure crates and modules never depend on
  `tokio`. `alchemy-ipc` keeps its codec sync and gates async transport
  behind a feature.
- **TLS is rustls.** OpenSSL and native-tls are banned in `deny.toml`.
- **`alchemy-helper` is the security boundary.** It is the only privileged
  surface: never setuid, never long-running, strictly typed arguments, never
  a shell, one operation, then exit. The GUI never runs as root.
- **Dependencies go in `[workspace.dependencies]`** and are referenced with
  `name = { workspace = true }`. Prefer `default-features = false` with
  explicit features. New dependencies must pass `cargo deny check`.
- **Document public items.** `missing_docs` warns and pedantic clippy is on.

## 5. Comments

Comments are for people reading the code, not narration of it. Keep them
short and accurate; fix or delete one that has drifted from the code.

- No inline comments inside function bodies. If a line needs one, make the
  code clearer. The exception is a short label on an otherwise opaque value
  in protocol code (`1, // version`).
- Prefer item and module docs (`///`, `//!`). Document the non-obvious why:
  gotchas, RFCs, platform quirks. Don't restate the signature.
- Skip docs on self-evident private items. Public items always get at least
  one line.
- No em dashes in comments; use a hyphen, comma, or colon. Never start a
  wrapped doc line with `- `, because rustfmt and clippy read it as a list
  item (`doc_lazy_continuation`).

## 6. Platforms

Linux and macOS are supported; CI runs on both, x86_64 and arm64. There are
no Windows code paths.

- Select per-OS code with `#[cfg(...)]`. When you change one OS path, make
  the matching change (or a deliberate no-op) in the others.
- Watch for host-only assumptions in tests, such as paths or socket length
  limits that differ between Linux and macOS.

## 7. Definition of done

A change is complete when:

- pure logic has table-driven unit tests;
- every side effect is behind a trait and tested with a fake;
- wiring has an integration test in the crate's `tests/`;
- public items are documented;
- the full gate in [DEVELOPMENT.md](DEVELOPMENT.md#the-gate) passes.

If a task would break one of these rules, such as adding I/O to a pure
crate, routing a side effect around a trait, giving the GUI privileged
access, or breaking a contract, stop and raise it instead of working
around it.
