//! The tray icon (the menu bar on macOS) and its menu.
//!
//! Linux tray icons send no click events, so everything is a menu item
//! and the menu opens on a left click on macOS too. The menu is built once
//! and kept: items change their text, state, or presence in place, as
//! [`slots`] lays them out for each [`MenuModel`].

use std::sync::{Arc, Mutex, PoisonError};

use tauri::menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Runtime};

use crate::launch::Installer;
use crate::shell::Shell;
use crate::status::MenuModel;

const OPEN: &str = "open";
const START_DAEMON: &str = "start-daemon";
const START_DOCKER: &str = "start-docker";
const LOGIN: &str = "login";
const QUIT: &str = "quit";

/// One place in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The status line.
    Status,
    /// What went wrong, under the status line.
    Notice,
    /// A separator; the number tells the three apart.
    Separator(u8),
    /// Open Alchemy.
    Open,
    /// Start Alchemy, after alchemyd stopped or failed to start.
    StartDaemon,
    /// Start Docker Desktop.
    StartDocker,
    /// Start at login.
    Login,
    /// Why Start at login can't be turned on.
    LoginNote,
    /// Quit.
    Quit,
}

/// The menu's items for `model`, top to bottom.
pub fn slots(model: &MenuModel) -> Vec<Slot> {
    let mut out = vec![Slot::Status];
    if model.notice.is_some() {
        out.push(Slot::Notice);
    }
    out.extend([Slot::Separator(0), Slot::Open]);
    if model.start_daemon {
        out.push(Slot::StartDaemon);
    }
    if model.start_docker.is_some() {
        out.push(Slot::StartDocker);
    }
    out.extend([Slot::Separator(1), Slot::Login]);
    if model.login.note.is_some() {
        out.push(Slot::LoginNote);
    }
    out.extend([Slot::Separator(2), Slot::Quit]);
    out
}

struct Items<R: Runtime> {
    menu: Menu<R>,
    status: MenuItem<R>,
    notice: MenuItem<R>,
    separators: [PredefinedMenuItem<R>; 3],
    open: MenuItem<R>,
    start_daemon: MenuItem<R>,
    start_docker: MenuItem<R>,
    login: CheckMenuItem<R>,
    login_note: MenuItem<R>,
    quit: MenuItem<R>,
    shown: Mutex<Vec<Slot>>,
}

impl<R: Runtime> Items<R> {
    fn new(app: &AppHandle<R>) -> tauri::Result<Self> {
        let none = None::<&str>;
        Ok(Self {
            menu: Menu::new(app)?,
            status: MenuItem::with_id(app, "status", "Alchemy", false, none)?,
            notice: MenuItem::with_id(app, "notice", "", false, none)?,
            separators: [
                PredefinedMenuItem::separator(app)?,
                PredefinedMenuItem::separator(app)?,
                PredefinedMenuItem::separator(app)?,
            ],
            open: MenuItem::with_id(app, OPEN, "Open Alchemy", true, none)?,
            start_daemon: MenuItem::with_id(app, START_DAEMON, "Start Alchemy", true, none)?,
            start_docker: MenuItem::with_id(app, START_DOCKER, "Start Docker Desktop", true, none)?,
            login: CheckMenuItem::with_id(app, LOGIN, "Start at login", false, false, none)?,
            login_note: MenuItem::with_id(app, "login-note", "", false, none)?,
            quit: MenuItem::with_id(app, QUIT, "Quit Alchemy", true, none)?,
            shown: Mutex::new(Vec::new()),
        })
    }

    fn item(&self, slot: Slot) -> &dyn IsMenuItem<R> {
        match slot {
            Slot::Status => &self.status,
            Slot::Notice => &self.notice,
            Slot::Separator(n) => match n {
                0 => &self.separators[0],
                1 => &self.separators[1],
                _ => &self.separators[2],
            },
            Slot::Open => &self.open,
            Slot::StartDaemon => &self.start_daemon,
            Slot::StartDocker => &self.start_docker,
            Slot::Login => &self.login,
            Slot::LoginNote => &self.login_note,
            Slot::Quit => &self.quit,
        }
    }

    fn sync(&self, model: &MenuModel) -> tauri::Result<()> {
        self.status.set_text(&model.status)?;
        self.notice
            .set_text(model.notice.as_deref().unwrap_or_default())?;
        self.start_docker
            .set_enabled(model.start_docker.unwrap_or(false))?;
        self.login.set_checked(model.login.checked)?;
        self.login.set_enabled(model.login.enabled)?;
        self.login_note
            .set_text(model.login.note.as_deref().unwrap_or_default())?;

        let want = slots(model);
        let mut shown = self.shown.lock().unwrap_or_else(PoisonError::into_inner);
        if *shown != want {
            for slot in shown.iter() {
                self.menu.remove(self.item(*slot))?;
            }
            for slot in &want {
                self.menu.append(self.item(*slot))?;
            }
            *shown = want;
        }
        Ok(())
    }
}

/// Create the tray for `shell`, and keep its menu in step with it.
pub fn create<R, I>(app: &AppHandle<R>, shell: &Arc<Shell<I>>) -> tauri::Result<()>
where
    R: Runtime,
    I: Installer + Send + Sync + 'static,
{
    let items = Arc::new(Items::new(app)?);
    items.sync(&shell.menu())?;

    let actions = Arc::clone(shell);
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Alchemy")
        .menu(&items.menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| on_menu(app, &event, &actions));
    #[cfg(target_os = "macos")]
    {
        tray = tray
            .icon(tauri::include_image!("icons/tray-template.png"))
            .icon_as_template(true);
    }
    #[cfg(not(target_os = "macos"))]
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;

    let synced = Arc::clone(&items);
    shell.on_change(move |model| {
        if let Err(e) = synced.sync(model) {
            eprintln!("alchemy-gui: cannot update the tray menu: {e}");
        }
    });
    Ok(())
}

fn on_menu<R, I>(app: &AppHandle<R>, event: &MenuEvent, shell: &Arc<Shell<I>>)
where
    R: Runtime,
    I: Installer + Send + Sync + 'static,
{
    match event.id().as_ref() {
        OPEN => crate::show_main(app),
        START_DAEMON => shell.request_start(),
        START_DOCKER => {
            let shell = Arc::clone(shell);
            tauri::async_runtime::spawn(async move { shell.start_docker().await });
        }
        LOGIN => {
            let shell = Arc::clone(shell);
            tauri::async_runtime::spawn(async move { shell.toggle_login().await });
        }
        QUIT => app.exit(0),
        _ => {}
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::status::LoginItem;

    fn model() -> MenuModel {
        MenuModel {
            status: "alchemyd running".to_owned(),
            notice: None,
            start_daemon: false,
            start_docker: None,
            login: LoginItem {
                checked: false,
                enabled: true,
                note: None,
            },
        }
    }

    #[test]
    fn slots_table() {
        use Slot::{
            Login, LoginNote, Notice, Open, Quit, Separator, StartDaemon, StartDocker, Status,
        };
        let base = model();
        let everything = MenuModel {
            notice: Some("n".to_owned()),
            start_daemon: true,
            start_docker: Some(false),
            login: LoginItem {
                note: Some("why".to_owned()),
                ..base.login.clone()
            },
            ..base.clone()
        };
        let cases = [
            (
                base,
                vec![
                    Status,
                    Separator(0),
                    Open,
                    Separator(1),
                    Login,
                    Separator(2),
                    Quit,
                ],
            ),
            (
                everything,
                vec![
                    Status,
                    Notice,
                    Separator(0),
                    Open,
                    StartDaemon,
                    StartDocker,
                    Separator(1),
                    Login,
                    LoginNote,
                    Separator(2),
                    Quit,
                ],
            ),
        ];
        for (model, want) in cases {
            assert_eq!(slots(&model), want, "{model:?}");
        }
    }
}
