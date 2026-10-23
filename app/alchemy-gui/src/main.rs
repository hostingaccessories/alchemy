//! Alchemy desktop shell.
//!
//! A thin Tauri wrapper around the bundled `alchemy-web` UI. All state
//! lives in `alchemyd`; the UI talks to it over HTTP. This crate only
//! adds native affordances (single instance, opening links, and later a
//! tray and daemon lifecycle management).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::Manager as _;

fn main() {
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .run(tauri::generate_context!());

    if let Err(e) = result {
        eprintln!("alchemy-gui: {e}");
        std::process::exit(1);
    }
}
