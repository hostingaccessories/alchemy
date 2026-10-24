//! Apps: the Compose stacks in the app folders, kept current in
//! [`State`](crate::State).
//!
//! The pieces, from the edge in:
//!
//! - [`AppFs`] lists the app folders ([`SystemFs`], or [`FakeFs`] in
//!   tests), and [`alchemy_core::stack::discover`] decides what in them is
//!   an app.
//! - [`ComposeConfigReader`](crate::compose::ComposeConfigReader) reads
//!   each app's resolved Compose project name and services.
//! - [`app_list`] combines those with the engine's containers into the
//!   [`AppList`] clients see.
//! - [`run`] ties them together for the daemon's lifetime, and the
//!   settings in [`settings`] say which folders to look in.
//! - [`AppEditor`] creates apps and edits their compose file and `.env`,
//!   staged and validated with Compose, writing only through [`AppDisk`]
//!   ([`SystemDisk`] in the daemon).
//!
//! Code that acts on an app, such as running a lifecycle action, finds it
//! with [`find`] in [`State::apps`](crate::State::apps) and gets its
//! Compose project with [`compose_project`]; the app's `images` and
//! `project` are what Compose resolved.

mod assemble;
mod disk;
mod edit;
mod fs;
mod monitor;
pub mod settings;

pub use assemble::{ConfigOutcome, Inputs, app_list};
pub use disk::{AppDisk, FileRead, ReadOutcome, SystemDisk};
pub use edit::{AppEditor, EditError, content_hash};
pub use fs::{AppFs, FakeFs, SystemFs};
pub use monitor::{AppsDeps, DEFAULT_SCAN_INTERVAL, run};

use alchemy_api::apps::{App, AppList};

use crate::compose::{ComposeProject, ComposeProjectError};

/// The app named `name` in `apps`.
pub fn find<'a>(apps: &'a AppList, name: &str) -> Option<&'a App> {
    apps.apps.iter().find(|app| app.name.as_str() == name)
}

/// The Compose project in `app`'s folder.
pub fn compose_project(app: &App) -> Result<ComposeProject, ComposeProjectError> {
    ComposeProject::new(&app.dir)
}
