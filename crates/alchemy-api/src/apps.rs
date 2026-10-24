//! Apps and where they are found.
//!
//! - `GET /api/apps` returns an [`AppList`], which the `apps` event also
//!   carries on every change.
//! - `GET /api/apps/{name}` returns one [`App`], or 404 with
//!   [`ErrorCode::AppNotFound`](crate::ErrorCode::AppNotFound).
//! - `GET /api/settings/apps` returns [`AppSettings`]; `PUT` takes an
//!   [`AppSettingsUpdate`] and returns the new [`AppSettings`], or 422
//!   [`ErrorCode::Invalid`](crate::ErrorCode::Invalid).
//! - Creating an app and editing its files are described in [`files`].
//!
//! An app is a Compose stack: a folder holding a compose file inside one
//! of the app folders. Every human-readable `message` here is written for
//! the people using Alchemy and safe to show as is.

pub mod files;

pub use alchemy_core::StackName;
pub use alchemy_core::engine::{ContainerState, Health, StopReason};
pub use alchemy_core::stack::{ImageSource, StackStatus};
pub use files::{AppFile, AppFileKind, AppFileUpdate, NewApp, Saved};
use serde::{Deserialize, Serialize};

/// Every app, every running Compose project that is not one, and how each
/// app folder was read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppList {
    /// The apps, in folder order, then by name.
    pub apps: Vec<App>,
    /// Compose projects with containers that belong to no app, by project
    /// name. Shown read-only.
    pub unmanaged: Vec<UnmanagedApp>,
    /// Each app folder, home first, as last read.
    pub dirs: Vec<AppDir>,
}

/// One app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    /// The app's name: its folder's name.
    pub name: StackName,
    /// How the app is doing.
    pub status: StackStatus,
    /// The app's folder.
    pub dir: String,
    /// The app folder it was found in, one of [`AppList::dirs`].
    pub root: String,
    /// The compose file's name inside `dir`, such as `compose.yaml`.
    pub compose_file: String,
    /// The Compose project name, as Compose resolves it from the folder
    /// name, a `name:` in the compose file, or `COMPOSE_PROJECT_NAME` in
    /// `.env`. `None` until Compose has read the app, or when it cannot.
    pub project: Option<String>,
    /// Where the app's images come from. `None` while `project` is.
    pub images: Option<ImageSource>,
    /// The services Compose lists, then any others that have containers in
    /// the project, by name.
    pub services: Vec<AppService>,
    /// What is wrong with the app, worst first. Empty when nothing is.
    pub problems: Vec<AppProblem>,
    /// Why it is or is not running: the most pressing of its containers'
    /// reasons, such as a crash in one service while the others run.
    #[serde(default)]
    pub stop_reason: StopReason,
    /// Where to open the app in a browser through Alchemy's proxy, best
    /// first. Empty, and left off the wire, when the app has no route;
    /// a [`AppProblemKind::Route`] problem then says why, if something
    /// can be done about it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<AppAddress>,
}

/// One web address of an app, served by Alchemy's proxy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppAddress {
    /// How the proxy tells this app's requests apart.
    pub kind: AddressKind,
    /// The URL to open, such as `http://blog.localhost/` or
    /// `http://192.168.1.20:8100/`.
    pub url: String,
    /// Whether it answers right now.
    pub state: AddressState,
    /// The service requests go to.
    pub service: String,
    /// The container port requests go to.
    pub port: u16,
    /// Why it doesn't answer, when `state` is not `ready`.
    pub message: Option<String>,
}

/// The kinds of [`AppAddress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AddressKind {
    /// A hostname such as `blog.localhost`, routed by the `Host` header
    /// on port 80.
    Host,
    /// The app's own proxy port on every address of this machine, so it
    /// works from other devices on the network.
    Port,
    /// A kind this build does not know.
    #[serde(other)]
    Other,
}

/// Whether an [`AppAddress`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AddressState {
    /// Requests reach the app.
    Ready,
    /// The app is running but can't be reached yet, such as while Alchemy
    /// attaches it to its network or opens the port.
    Pending,
    /// The app isn't running, so the proxy answers that it's stopped.
    NotRunning,
    /// The proxy can't listen on this address's port, such as port 80
    /// without permission.
    Unavailable,
    /// A state this build does not know.
    #[serde(other)]
    Other,
}

/// One service of an app and its containers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppService {
    /// The service name from the compose file.
    pub name: String,
    /// Its containers, by name. Empty when it has none.
    pub containers: Vec<AppContainer>,
}

/// One container of an app's service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppContainer {
    /// Full container ID.
    pub id: String,
    /// Container name.
    pub name: String,
    /// Image reference it was created from.
    pub image: Option<String>,
    /// Lifecycle state.
    pub state: ContainerState,
    /// Healthcheck result, if it has a healthcheck.
    pub health: Option<Health>,
    /// Why it is or is not running.
    #[serde(default)]
    pub stop_reason: StopReason,
}

/// Something wrong with an app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppProblem {
    /// What kind of problem it is, for clients that branch on it.
    pub kind: AppProblemKind,
    /// What is wrong, in a sentence.
    pub message: String,
}

/// The kinds of [`AppProblem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppProblemKind {
    /// Compose could not read the app's files; the message is Compose's.
    ComposeError,
    /// Compose could not be run at all, such as when Docker is not
    /// installed.
    ComposeUnavailable,
    /// Another app resolves to the same Compose project, so both see the
    /// same containers.
    SharedProject,
    /// One of the app's networks overlaps the LAN or a route, so its
    /// containers can't reach the devices or addresses behind it.
    NetworkOverlap,
    /// A port the app publishes is already in use or reserved.
    PortConflict,
    /// The app can't be given a web address, or only part of one; the
    /// message says what to add to its compose file.
    Route,
    /// A kind this build does not know.
    #[serde(other)]
    Other,
}

/// A Compose project with containers that belongs to no app, such as one
/// started by hand from a folder Alchemy does not look in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnmanagedApp {
    /// The Compose project name.
    pub project: String,
    /// The folder Compose ran it from, from its containers' labels.
    pub dir: Option<String>,
    /// How it is doing, judged by its containers alone.
    pub status: StackStatus,
    /// Its services, by name.
    pub services: Vec<AppService>,
    /// Why it is or is not running: the most pressing of its containers'
    /// reasons, such as a crash in one service while the others run.
    #[serde(default)]
    pub stop_reason: StopReason,
}

/// One app folder and what reading it found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppDir {
    /// The folder.
    pub path: String,
    /// Whether this is the home, where new apps go.
    pub home: bool,
    /// Whether it could be read.
    pub state: AppDirState,
    /// Why not, when `state` is not `ok`.
    pub message: Option<String>,
    /// Folders in it that look like apps but were skipped, by name.
    pub skipped: Vec<SkippedApp>,
}

/// Whether an [`AppDir`] could be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppDirState {
    /// It was read.
    Ok,
    /// It does not exist. Normal for the home until the first app is
    /// added.
    Missing,
    /// Something other than a folder is at its path.
    NotADirectory,
    /// It could not be read, such as for lack of permission.
    Unreadable,
    /// A state this build does not know.
    #[serde(other)]
    Other,
}

/// A folder that looks like an app but is not listed as one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedApp {
    /// The folder's name.
    pub name: String,
    /// The folder.
    pub path: String,
    /// Why it was skipped, for clients that branch on it.
    pub reason: SkipReason,
    /// Why it was skipped, in a sentence.
    pub message: String,
}

/// Why a [`SkippedApp`] was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SkipReason {
    /// Its name is not a valid app name.
    InvalidName,
    /// An earlier app folder already has an app with this name.
    Duplicate,
    /// Its compose file is a symlink, which Alchemy ignores.
    LinkedComposeFile,
    /// Its compose file is not a regular file.
    ComposeFileNotRegular,
    /// The folder itself is a symlink, which Alchemy ignores.
    LinkedFolder,
    /// A reason this build does not know.
    #[serde(other)]
    Other,
}

/// The app folders, from `GET /api/settings/apps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    /// The home, where new apps go and the first folder searched: the
    /// chosen one, else `default_home`. `None` only when neither exists.
    pub home: Option<String>,
    /// The home used when none is chosen, `~/Alchemy/apps`. `None` when
    /// the daemon has no home directory.
    pub default_home: Option<String>,
    /// More folders to search after the home, in order.
    pub extra: Vec<String>,
}

/// The body of `PUT /api/settings/apps`, replacing both settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettingsUpdate {
    /// The new home, as a full path, or `None` for the default.
    #[serde(default)]
    pub home: Option<String>,
    /// More folders to search after the home, as full paths, in order.
    #[serde(default)]
    pub extra: Vec<String>,
}
