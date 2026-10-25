//! Tauri build script: generates the context from `tauri.conf.json`.
//!
//! The app's own commands are listed in its manifest, so each needs a
//! permission in `capabilities/` before the UI may call it.

fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&["daemon_token", "launch_state", "start_daemon"]),
    ))
    .unwrap_or_else(|e| {
        eprintln!("tauri build failed: {e:#}");
        std::process::exit(1);
    });
}
