#!/usr/bin/env bash
# Bundle the desktop app with alchemyd and alchemy-helper inside it.
#
# Builds both in release mode, copies them to app/alchemy-gui/binaries/
# with the host target triple that Tauri's externalBin expects, and runs
# `tauri build` with tauri.bundle.json merged in. externalBin stays out of
# tauri.conf.json: tauri-build would otherwise need the sidecars on every
# `cargo build` of alchemy-gui, and would copy them over target/debug.
#
# The bundler strips the triple, so both land next to the app's binary:
# Alchemy.app/Contents/MacOS, /usr/bin in the .deb, usr/bin in the AppImage.
#
# Usage: scripts/bundle-desktop.sh [tauri build args, e.g. --bundles app]

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
gui="${root}/app/alchemy-gui"

cd "$root"

triple="$(rustc --print host-tuple)"
cargo build --release --locked -p alchemyd -p alchemy-helper

mkdir -p "${gui}/binaries"
for exe in alchemyd alchemy-helper; do
  install -m 0755 "${root}/target/release/${exe}" "${gui}/binaries/${exe}-${triple}"
done

cd "$gui"
exec pnpm exec tauri build --config tauri.bundle.json "$@"
