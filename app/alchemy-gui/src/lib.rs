//! Alchemy desktop shell.
//!
//! A thin Tauri wrapper around the bundled `alchemy-web` UI. All state
//! lives in `alchemyd`; the UI talks to it over HTTP. This crate only adds
//! native affordances:
//!
//! - single instance, which shows the window again, even when it was
//!   closed to the tray;
//! - opening links and apps' web addresses in the default browser;
//! - remembering the window's size and position;
//! - making sure a daemon runs ([`launch`], [`shell`]): attach to one that
//!   answers, or install the bundled alchemyd as the per-user service.
//!   The shell never runs alchemyd as its child;
//! - a tray ([`tray`]) with alchemyd's status, Open Alchemy, Start Docker
//!   Desktop, Start at login, and Quit. Closing the window hides it; Quit
//!   leaves alchemyd and the apps running;
//! - the token that signs the UI in to alchemyd's TCP listener
//!   ([`daemon_token`]).
//!
//! URLs open only through the UI's `openExternal` (`https:`) and
//! `openAddress` (an app's `http:` address from alchemyd), and the
//! capability scopes the opener to those two schemes. The opener plugin's
//! own click handler is off, so no second path opens URLs behind it.

pub mod client;
pub mod launch;
pub mod shell;
pub mod status;
pub mod token;
pub mod tray;

use std::sync::Arc;

use tauri::{AppHandle, Manager as _, Runtime, WindowEvent};
use tauri_plugin_window_state::StateFlags;

use crate::client::{DAEMON_ADDR, HttpDaemon};
use crate::launch::{Launch, SystemInstaller, Timing};
use crate::shell::{Retry, Shell};

type AppShell = Arc<Shell<SystemInstaller>>;

/// The TCP token alchemyd wrote when it last started, which the UI sends
/// as `Authorization: Bearer`. The UI is served from the shell's own
/// origin, so the browser's sign-in cookie never reaches it. Read on every
/// call, so a restarted daemon's new token is picked up when the UI asks
/// again after a 401.
#[tauri::command]
fn daemon_token() -> Result<String, String> {
    token::read()
}

/// How the shell stands with alchemyd, for the UI to show while it can't
/// reach the daemon itself: starting it, or why it couldn't.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn launch_state(shell: tauri::State<'_, AppShell>) -> Launch {
    shell.launch()
}

/// Start alchemyd again, if it stopped or failed to start.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn start_daemon(shell: tauri::State<'_, AppShell>) {
    shell.request_start();
}

/// Bring the main window back, from the tray, a second launch, or the
/// Dock.
pub(crate) fn show_main<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Run the shell until Quit.
pub fn run() -> tauri::Result<()> {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main(app);
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
        .invoke_handler(tauri::generate_handler![
            daemon_token,
            launch_state,
            start_daemon
        ])
        .setup(|app| {
            let shell: AppShell = Shell::new(
                HttpDaemon::new(DAEMON_ADDR, Arc::new(token::read)),
                SystemInstaller,
                launch::this_alchemyd(),
                Timing::default(),
                Retry::default(),
            );
            app.manage(Arc::clone(&shell));
            tray::create(app.handle(), &shell)?;
            tauri::async_runtime::spawn(shell.run());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && window.label() == "main"
            {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())?;

    app.run(on_run_event);
    Ok(())
}

/// A Dock icon click on macOS shows the window closed to the tray.
#[cfg(target_os = "macos")]
#[allow(clippy::needless_pass_by_value)]
fn on_run_event<R: Runtime>(app: &AppHandle<R>, event: tauri::RunEvent) {
    if let tauri::RunEvent::Reopen { .. } = event {
        show_main(app);
    }
}

/// Linux has no Dock to reopen from: launching again goes through single
/// instance.
#[cfg(not(target_os = "macos"))]
#[allow(clippy::needless_pass_by_value)]
fn on_run_event<R: Runtime>(_app: &AppHandle<R>, _event: tauri::RunEvent) {}
