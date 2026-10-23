//! Alchemy desktop shell.
//!
//! A thin Tauri wrapper around the bundled `alchemy-web` UI. All state
//! lives in `alchemyd`; the UI talks to it over HTTP. This crate only
//! adds native affordances (single instance, opening `https:` links in the
//! default browser, remembering the window's size and position, and later a
//! tray and daemon lifecycle management).
//!
//! Links open only through the UI's `openExternal`, which the capability
//! scopes to `https:`. The opener plugin's own click handler is off, so no
//! second path opens URLs behind it.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::Manager as _;
use tauri_plugin_window_state::StateFlags;

fn main() {
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .plugin(
            tauri_plugin_window_state::Builder::new()
                .with_state_flags(StateFlags::SIZE | StateFlags::POSITION)
                .build(),
        )
        .run(tauri::generate_context!());

    if let Err(e) = result {
        eprintln!("alchemy-gui: {e}");
        std::process::exit(1);
    }
}
