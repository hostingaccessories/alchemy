# CLAUDE.md

Alchemy is a Docker Compose management platform for homelabs: a daemon
(`alchemyd`) that owns all state, with a CLI, web UI, and Tauri desktop shell
as thin clients. See [docs/VISION.md](docs/VISION.md) for the product and
[docs/PERSONAS.md](docs/PERSONAS.md) for who it is for.

## Architecture rules

These are authoritative. Read them before changing Rust code.

@docs/ARCHITECTURE.md

## Building and testing

Prerequisites, running the daemon/CLI/GUI locally, and the smoke test are in
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md). Before considering a change done,
run the same gate as CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check
pnpm ready
```

Run `scripts/release-smoke.sh` when touching the daemon's startup, shutdown,
listeners, or the embedded web UI.

## Working agreement

- Don't commit, push, or merge unless asked.
- Match the surrounding code's style, naming, and comment density.
- When a task conflicts with the architecture rules, stop and say so instead
  of working around them.
- Never read `.env*` or `.envrc` files; they hold credentials.
