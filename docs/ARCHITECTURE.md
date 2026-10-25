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
   effect so it can be tested without the OS. For example, in
   `alchemy_api`'s socket module, the private
   `runtime_dir_from(xdg, temp, uid)` decides the path; `runtime_dir()`
   reads the environment and calls it.
4. **Binaries are thin.** `alchemyd`, `alchemy`, and `alchemy-helper` are a
   library plus a small `main.rs`. `main` parses arguments, sets up logging and the runtime, and
   calls into the library. Interesting logic belongs in a crate with tests.
5. **The daemon is the single source of truth.** `alchemyd` owns all runtime
   state. The CLI, web UI, and desktop shell are clients and never
   reimplement daemon logic.

## 2. Crates and dependency direction

| Crate            | Path                  | Role                                                                                                 |
| ---------------- | --------------------- | ---------------------------------------------------------------------------------------------------- |
| `alchemy-core`   | `crates/alchemy-core` | Pure domain model (stacks, the Docker engine, operations, logs, metrics, and their invariants)       |
| `alchemy-api`    | `crates/alchemy-api`  | HTTP API types; the socket path and the state directory behind the `socket` and `state-dir` features |
| `alchemyd`       | `bin/alchemyd`        | Daemon: HTTP API on TCP and a Unix socket, embedded web UI                                           |
| `alchemy`        | `bin/alchemy`         | CLI, an HTTP client over the Unix socket                                                             |
| `alchemy-helper` | `bin/alchemy-helper`  | Privileged one-shot                                                                                  |
| `alchemy-gui`    | `app/alchemy-gui`     | Tauri desktop shell around the web UI                                                                |
| `alchemy-web`    | `app/alchemy-web`     | React web UI, served by the daemon and bundled into the shell                                        |

Internal dependencies flow strictly downhill, with no cycles:

```
alchemy-core ◄── alchemy-api ◄── alchemyd, alchemy, alchemy-gui
```

- `alchemy-core` depends on no other `alchemy-*` crate.
- `alchemy-helper` depends on no internal crate.
- Libraries never depend on binaries.
- Clients depend on `alchemy-api`, not on daemon internals. `alchemy`
  may use `alchemyd` as a dev-dependency for end-to-end tests only.

## 3. One contract, two listeners

The daemon serves one HTTP API, `/api` (`alchemy-api`), on two listeners:

- **Unix socket**: used by the CLI. Per-user, `0o600`, in the user's
  runtime directory. It serves only `/api`.
- **TCP**: used by the web UI and the desktop shell. It also serves the
  web UI itself. Loopback only, and token-authenticated (a per-start
  token or a browser session from `alchemy open`) until full auth in M13.

Every request passes the checks in `bin/alchemyd/src/guard.rs` before any
route: `Host` and, under `/api`, `Sec-Fetch-Site` on TCP, then `Origin`
on TCP and a JSON content type on both for mutating requests. On TCP,
signing in (`bin/alchemyd/src/auth.rs`) comes after them, for every `/api`
route but health and version. Routes nested under `/api` in `http.rs` get
all of this for free; never serve a route outside them.

The API is a stable contract:

- Change it additively: add routes, fields, and variants; never rename or
  remove one silently.
- Bump `API_VERSION` on any breaking change.
- Wire-shape tests (`crates/alchemy-api/tests/wire_stability.rs`) pin the
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
  `tokio`. `alchemy-api` is pure by default and gates its socket path
  helpers behind the `socket` feature.
- **TLS is rustls.** OpenSSL and native-tls are banned in `deny.toml`.
- **`alchemy-helper` is the security boundary.** It is the only privileged
  surface: never setuid, never long-running, strictly typed arguments, never
  a shell, one operation, then exit. The GUI never runs as root.
- **Dependencies go in `[workspace.dependencies]`** and are referenced with
  `name = { workspace = true }`. Prefer `default-features = false` with
  explicit features. New dependencies must pass `cargo deny check`.
- **Document public items.** `missing_docs` warns and pedantic clippy is on.

## 5. Privileged operations

Everything that needs root goes through `alchemy-helper`, and this table is
the whole list. Every action has an undo with the same arguments, and
every operation is idempotent: running it twice reports `changed: false`
the second time. Adding an operation, or widening one, is an architecture
change: raise it before writing it.

| Action           | Grant                             | Undo                               | Platforms                                  |
| ---------------- | --------------------------------- | ---------------------------------- | ------------------------------------------ |
| `low_ports`      | `grant-low-ports`                 | `revoke-low-ports`                 | Linux                                      |
| `firewall`       | `open-firewall --proxy-ports S-E` | `close-firewall --proxy-ports S-E` | ufw, firewalld, macOS application firewall |
| `system_service` | `install-service --uid N`         | `remove-service --uid N`           | systemd, launchd                           |

Why each needs root:

- **Low ports:** binding port 80 on Linux needs `cap_net_bind_service`.
  The grant writes the `security.capability` xattr (that capability and
  nothing else) on the `alchemyd` next to the helper, and alchemyd
  re-execs itself to pick it up. It is refused on a `nosuid` mount, where
  the kernel would ignore it. Rebuilds and upgrades drop it.
- **Firewall:** opens 80/tcp, the proxy range, and 5353/udp with a ufw
  app profile or firewalld service named `alchemy`, only when one of them
  is active, or adds and unblocks alchemyd in the macOS application
  firewall. Raw nftables and iptables are never edited.
- **System service:** writes a fixed-template systemd unit (numeric
  `User=N`) or LaunchDaemon (`UserName`) for the sibling `alchemyd` and
  starts it. alchemyd itself never runs as root. The LaunchDaemon logs to
  `/Library/Logs/Alchemy/alchemyd.N.log`, never under the user's home:
  the root launchd opens the log, so a user-writable path would let
  malware point it at a root-owned file with a symlink. The helper creates
  that directory root-owned and `0755`, and refuses if it is a symlink or
  writable by anyone else. Removal deletes the plist before `bootout`,
  because `bootout` stops the alchemyd that may have asked for it.

`check` only reports whether the helper runs as root. Docker group
membership is root-equivalent, so Alchemy explains it and never automates
it. macOS needs no root for port 80 or mDNS.

**The helper.** It never takes a path argument: every operation acts on
the `alchemyd` next to its own canonical path, opened once with
`O_NOFOLLOW` and changed through that file descriptor. Flag values are
validated before the root check. It runs only fixed commands by absolute
path, with a cleared environment, and checks that each one is a
root-owned regular file nobody else can write. It prints one JSON result
line and exits with a sysexits code, never 126 or 127, which mean
something else under pkexec; `not_root` is 77.

**Trust rule.** alchemyd offers a system prompt only for a helper that is
owned by root and writable by neither group nor others, along with every
directory up to `/`, with no symlinks on the way. Otherwise, malware
running as the user could swap the binary just before the password
prompt. A helper that fails the rule, such as every build in `target/`,
gets `needs_terminal` instead. The rule is pure:
`alchemy_core::privilege::helper_trust`.

**Elevation.** alchemyd owns it, behind the `Elevator` trait, and never
waits for it in a request: `POST /api/machine/privileges/{action}` answers
`pending` and the prompt runs in the background.

- macOS: `osascript` with a fixed script; the helper path and arguments go
  in as `argv` and are quoted with `quoted form of`, never spliced into the
  script.
- Linux: `pkexec --disable-internal-agent` when a polkit agent answers.
- Otherwise: `needs_terminal` with the exact `sudo alchemy-helper ...`
  command. The CLI runs it itself when it has a TTY; the web UI shows it.

alchemyd builds every argv in `alchemy_core::privilege` (`HelperOp`,
`elevated_command`); clients never build one. A grant of low ports takes
effect at exec, so alchemyd shuts down cleanly and re-execs its own
canonical binary with the same argv, keeping its PID.

**Contract.** The helper depends on no internal crate, so the argv is the
contract. `bin/alchemyd/tests/helper_contract.rs` feeds every argv alchemyd
can build, through osascript, pkexec, and sudo, to `alchemy_helper::parse`
and checks it is the same operation. Add an operation to both sides and to
that test together.

## 6. Comments

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

## 7. Platforms

Linux and macOS are supported; CI runs on both, x86_64 and arm64. There are
no Windows code paths.

- Select per-OS code with `#[cfg(...)]`. When you change one OS path, make
  the matching change (or a deliberate no-op) in the others.
- Watch for host-only assumptions in tests, such as paths or socket length
  limits that differ between Linux and macOS.

## 8. Definition of done

A change is complete when:

- pure logic has table-driven unit tests;
- every side effect is behind a trait and tested with a fake;
- wiring has an integration test in the crate's `tests/`;
- public items are documented;
- the full gate in [DEVELOPMENT.md](DEVELOPMENT.md#4-the-gate) passes.

If a task would break one of these rules, such as adding I/O to a pure
crate, routing a side effect around a trait, giving the GUI privileged
access, or breaking a contract, stop and raise it instead of working
around it.
