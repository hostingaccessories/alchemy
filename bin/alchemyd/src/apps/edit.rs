//! Creating apps and editing their files: [`AppEditor`].
//!
//! Nothing reaches an app folder until Compose has accepted it. Every
//! write is staged in a hidden `.alchemy-tmp-*` directory that only its
//! owner can open, made in the folder the write lands in, validated there
//! with `docker compose config`, then moved into place with one rename on
//! that folder's filesystem:
//!
//! - A new app is staged as `<folder>/.alchemy-tmp-<name>-XXXX/<name>`, so
//!   Compose names the project as it will once in place, and renamed to
//!   `<folder>/<name>` only if nothing is there yet. An existing folder is
//!   never replaced, since it may hold data.
//! - An edit stages the one file being edited inside the app's own folder,
//!   as `<app>/.alchemy-tmp-<name>-XXXX/<file>`, so the rename over the
//!   live file stays on the app folder's filesystem even when that folder
//!   is a mount of its own, such as a ZFS dataset, a btrfs subvolume, or
//!   a bind mount. It is validated against the app's folder, so relative
//!   paths, `env_file:` entries, and override files resolve as they will.
//!   When `.env` sets `COMPOSE_FILE`, the file edited is the first it
//!   lists, validated together with the rest.
//!   It is renamed over the live file only if that file still has the
//!   hash the edit started from, checked before staging and again after
//!   Compose has run. No other file in the folder is touched, and a single
//!   rename cannot leave a mix of old and new to roll back.
//!
//! Discovery skips hidden entries and watches only an app's compose and
//! `.env` files, so a staging directory is never mistaken for an app or a
//! change to one. It is removed once the write is done, whether it was
//! saved, refused, or abandoned by a client that went away. So is a home
//! the first create made, with any parents it needed, if that create does
//! not finish and they are still empty.
//!
//! Only apps in the app folders are edited, never unmanaged projects, and
//! a symlinked folder or file is refused rather than followed. `.env` is
//! always written readable by its owner alone. File contents are never
//! logged.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alchemy_api::ErrorCode;
use alchemy_api::apps::{
    App, AppFile, AppFileKind, AppFileUpdate, AppService, AppSettings, NewApp, StackStatus,
    StopReason,
};
use alchemy_core::StackName;
use alchemy_core::stack::{
    ComposeFileError, ENV_FILE, EntryKind, FileContentError, MAX_FILE_LEN, NEW_COMPOSE_FILE,
    OVERRIDE_FILES, StackDirs, check_content, compose_file_args, compose_file_entries, create_root,
    edited_compose_file, scrub, staging_prefix,
};
use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex;

use super::disk::{AppDisk, FileRead, ReadOutcome, SystemDisk};
use crate::State;
use crate::compose::{
    CliComposeConfig, ComposeCheck, ComposeConfig, ComposeProject, ComposeValidator, ConfigError,
};
use crate::engine::Endpoint;

/// Why a create, read, or edit was refused or failed. Every message is
/// safe to show as is and never mentions a staging path.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EditError {
    /// No app in the app folders has this name.
    #[error("no app is named {0:?}")]
    AppNotFound(String),
    /// The request or the file is unusable, including Compose rejecting
    /// it, in which case the message is Compose's own.
    #[error("{0}")]
    Invalid(String),
    /// It conflicts with what is on disk: the folder exists, or the file
    /// changed since it was read.
    #[error("{0}")]
    Conflict(String),
    /// `COMPOSE_FILE` in the app's `.env` can't be followed, or doesn't
    /// list the file being edited, so Alchemy can't validate the edit
    /// against the files Compose uses.
    #[error("{0}")]
    ComposeFile(ComposeFileError),
    /// Compose could not be run to validate the file.
    #[error("Alchemy couldn't run Docker Compose to check the file: {0}")]
    ComposeUnavailable(String),
    /// Reading or writing the app folder failed.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: String,
        /// Why it failed.
        #[source]
        source: io::Error,
    },
}

impl EditError {
    /// The API error code clients branch on.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::AppNotFound(_) => ErrorCode::AppNotFound,
            Self::Invalid(_) | Self::ComposeFile(_) => ErrorCode::Invalid,
            Self::Conflict(_) => ErrorCode::Conflict,
            Self::ComposeUnavailable(_) | Self::Io { .. } => ErrorCode::Internal,
        }
    }

    fn io(what: impl Into<String>) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Io {
            what: what.into(),
            source,
        }
    }
}

/// The SHA-256 of `bytes` in lowercase hex, as [`AppFile::hash`] carries it.
pub fn content_hash(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Creates apps and reads and edits their compose file and `.env`.
///
/// Writes are serialized: one runs at a time, daemon-wide.
#[derive(Debug)]
pub struct AppEditor {
    disk: Arc<dyn AppDisk>,
    validator: Arc<dyn ComposeValidator>,
    writing: Mutex<()>,
}

/// One file of one app, located.
#[derive(Debug, Clone)]
struct Target {
    kind: AppFileKind,
    name: String,
    dir: PathBuf,
    file_name: String,
}

impl Target {
    fn live(&self) -> PathBuf {
        self.dir.join(&self.file_name)
    }
}

/// An edit staged and ready to validate.
#[derive(Debug)]
struct StagedEdit {
    target: Target,
    staging: Staging,
    staged: PathBuf,
    check: ComposeCheck,
}

/// A staging directory, removed by [`Self::remove`] once a write is done.
///
/// One dropped without being removed, as when a client goes away and its
/// request is dropped mid-write, is removed in the background instead, so
/// no path leaves a staging directory behind.
///
/// It also holds the folders a first create made for the home, deepest
/// first. Unless [`Self::keep_created`] is called, they are removed after
/// it, each only if empty, so a create that fails or is abandoned leaves
/// no new home behind and never removes one that holds anything.
#[derive(Debug)]
struct Staging {
    disk: Arc<dyn AppDisk>,
    dir: PathBuf,
    created: Vec<PathBuf>,
    removed: bool,
}

impl Staging {
    fn new(disk: Arc<dyn AppDisk>, dir: PathBuf) -> Self {
        Self::with_created(disk, dir, Vec::new())
    }

    fn with_created(disk: Arc<dyn AppDisk>, dir: PathBuf, created: Vec<PathBuf>) -> Self {
        Self {
            disk,
            dir,
            created,
            removed: false,
        }
    }

    fn path(&self) -> &Path {
        &self.dir
    }

    /// Keep the folders made for the home, now that an app is in it.
    fn keep_created(&mut self) {
        self.created.clear();
    }

    /// Remove it now, blocking.
    fn remove_now(mut self) {
        self.removed = true;
        remove_logged(&*self.disk, &self.dir);
        remove_created(&*self.disk, &self.created);
    }

    /// Remove it off the async runtime, and wait for that.
    async fn remove(self) {
        let _ = tokio::task::spawn_blocking(move || self.remove_now()).await;
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        let (disk, dir, created) = (
            self.disk.clone(),
            std::mem::take(&mut self.dir),
            std::mem::take(&mut self.created),
        );
        let remove = move || {
            remove_logged(&*disk, &dir);
            remove_created(&*disk, &created);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(remove);
            }
            Err(_) => remove(),
        }
    }
}

fn remove_logged(disk: &dyn AppDisk, dir: &Path) {
    if let Err(error) = disk.remove_staging(dir) {
        tracing::warn!(%error, dir = %dir.display(), "removing a staging folder failed");
    }
}

/// Remove the folders in `created`, deepest first, each only if empty.
/// Once one is kept, so are the rest, since they hold it.
fn remove_created(disk: &dyn AppDisk, created: &[PathBuf]) {
    for dir in created {
        match disk.remove_empty_dir(dir) {
            Ok(()) => tracing::debug!(dir = %dir.display(), "removed an unused new folder"),
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => return,
            Err(error) => {
                tracing::warn!(%error, dir = %dir.display(), "removing an unused new folder failed");
                return;
            }
        }
    }
}

impl AppEditor {
    /// An editor that writes through `disk` and validates with `validator`.
    pub fn new(disk: Arc<dyn AppDisk>, validator: Arc<dyn ComposeValidator>) -> Self {
        Self {
            disk,
            validator,
            writing: Mutex::new(()),
        }
    }

    /// The real filesystem and `docker compose`.
    pub fn system() -> Self {
        Self::new(Arc::new(SystemDisk), Arc::new(CliComposeConfig::system()))
    }

    /// `kind` of the app named `name`. A missing `.env` reads as empty,
    /// with no hash.
    pub async fn read(
        &self,
        state: &State,
        name: &str,
        kind: AppFileKind,
    ) -> Result<AppFile, EditError> {
        let target = locate(state, name, kind)?;
        let disk = self.disk.clone();
        let read = blocking(move || {
            check_dir(&*disk, &target)?;
            let (target, _) = compose_files(&*disk, target)?;
            read_live(&*disk, &target).map(|live| (target, live))
        })
        .await?;
        let (target, live) = read?;
        let content = match &live {
            Some(file) => readable(&target, &file.bytes)?.to_owned(),
            None => String::new(),
        };
        Ok(AppFile {
            kind,
            name: target.file_name,
            hash: live.map(|file| content_hash(&file.bytes)),
            content,
        })
    }

    /// Replace `kind` of the app named `name` with `update.content`, if
    /// Compose accepts it and the file still has `update.base_hash`.
    pub async fn write(
        &self,
        state: &State,
        name: &str,
        kind: AppFileKind,
        update: AppFileUpdate,
    ) -> Result<AppFile, EditError> {
        let target = locate(state, name, kind)?;
        let _writing = self.writing.lock().await;
        let update = Arc::new(update);

        let staged = {
            let (disk, update) = (self.disk.clone(), update.clone());
            blocking(move || stage_edit(&disk, target, &update)).await??
        };
        let result = self
            .finish_edit(state, &staged.target, &staged, &update)
            .await;
        staged.staging.remove().await;
        result?;
        let target = staged.target;
        if !update.dry_run {
            tracing::info!(app = %target.name, file = %target.file_name, "saved app file");
            state.rescan_apps();
        }
        Ok(AppFile {
            kind,
            name: target.file_name,
            hash: Some(content_hash(update.content.as_bytes())),
            content: Arc::unwrap_or_clone(update).content,
        })
    }

    async fn finish_edit(
        &self,
        state: &State,
        target: &Target,
        staged: &StagedEdit,
        update: &Arc<AppFileUpdate>,
    ) -> Result<(), EditError> {
        self.validate(
            state,
            &staged.check,
            &[(staged.staging.path(), &target.dir)],
        )
        .await?;
        let (disk, target, staged_file, update) = (
            self.disk.clone(),
            target.clone(),
            staged.staged.clone(),
            update.clone(),
        );
        blocking(move || {
            let live = read_live(&*disk, &target)?;
            check_base(&target, live.as_ref(), update.base_hash.as_deref())?;
            if update.dry_run {
                return Ok(());
            }
            if let Some(live) = &live {
                keep_owner(&*disk, &staged_file, live, &target.file_name)?;
            }
            disk.rename_over(&staged_file, &target.live())
                .map_err(|err| {
                    rename_failed(
                        err,
                        format!(
                            "Alchemy couldn't save {} in {}",
                            target.file_name,
                            target.dir.display()
                        ),
                    )
                })
        })
        .await?
    }

    /// Create the app `new` describes, if Compose accepts it and nothing is
    /// at its folder yet.
    pub async fn create(&self, state: &State, new: NewApp) -> Result<App, EditError> {
        let name = StackName::new(new.name.as_str())
            .map_err(|e| EditError::Invalid(e.to_string().replacen("stack name", "app name", 1)))?;
        check_content(AppFileKind::Compose, new.compose.as_bytes())
            .map_err(|err| EditError::Invalid(format!("The compose file {err}")))?;
        if let Some(env) = &new.env {
            check_content(AppFileKind::Env, env.as_bytes())
                .map_err(|err| EditError::Invalid(format!("The .env file {err}")))?;
        }
        let dirs = managed_dirs(&state.app_settings()).ok_or_else(|| {
            EditError::Invalid("Choose a home folder for your apps in Settings first.".to_owned())
        })?;
        let root = create_root(&dirs, new.folder.as_deref().map(Path::new))
            .map_err(|e| EditError::Invalid(e.to_string()))?
            .to_owned();
        let is_home = root == dirs.home();
        let _writing = self.writing.lock().await;
        if let Some(existing) = super::find(&state.apps(), name.as_str()) {
            return Err(EditError::Conflict(format!(
                "An app named {name} already exists in {}.",
                existing.root
            )));
        }
        let target = root.join(name.as_str());
        let new = Arc::new(new);

        let mut staging = {
            let (disk, root, target, name) = (
                self.disk.clone(),
                root.clone(),
                target.clone(),
                name.clone(),
            );
            let dry_run = new.dry_run;
            blocking(move || {
                begin_create(&*disk, &root, is_home, dry_run, &target, &name)
                    .map(|(dir, created)| Staging::with_created(disk, dir, created))
            })
            .await??
        };
        let staged_app = staging.path().join(name.as_str());
        let result = self
            .finish_create(state, staging.path(), &staged_app, &root, &target, &new)
            .await;
        if result.is_ok() {
            staging.keep_created();
        }
        staging.remove().await;
        let config = result?;
        if !new.dry_run {
            tracing::info!(app = %name, dir = %target.display(), "created app");
            state.rescan_apps();
        }
        Ok(new_app(name, &root, &target, &config))
    }

    async fn finish_create(
        &self,
        state: &State,
        staging: &Path,
        staged_app: &Path,
        root: &Path,
        target: &Path,
        new: &Arc<NewApp>,
    ) -> Result<ComposeConfig, EditError> {
        {
            let (disk, dir, new) = (self.disk.clone(), staged_app.to_owned(), new.clone());
            blocking(move || stage_new_app(&*disk, &dir, &new)).await??;
        }
        let project = ComposeProject::new(staged_app)
            .map_err(|e| EditError::Invalid(format!("Alchemy can't use this folder: {e}")))?;
        let config = self
            .validate(
                state,
                &ComposeCheck::new(project),
                &[(staged_app, target), (staging, root)],
            )
            .await?;
        if new.dry_run {
            return Ok(config);
        }
        let (disk, from, to) = (self.disk.clone(), staged_app.to_owned(), target.to_owned());
        blocking(move || {
            disk.rename_new(&from, &to).map_err(|err| {
                if err.kind() == io::ErrorKind::AlreadyExists {
                    exists(&to)
                } else {
                    rename_failed(err, format!("Alchemy couldn't create {}", to.display()))
                }
            })
        })
        .await??;
        Ok(config)
    }

    /// Run Compose on `check`, with each `(staging, shown_as)` scrubbed
    /// from its message.
    async fn validate(
        &self,
        state: &State,
        check: &ComposeCheck,
        scrubs: &[(&Path, &Path)],
    ) -> Result<ComposeConfig, EditError> {
        match self.validator.validate(check, &endpoint(state)).await {
            Ok(config) => Ok(config),
            Err(ConfigError::Invalid { message, .. }) => {
                let message = scrubs
                    .iter()
                    .fold(message, |msg, (staging, shown)| scrub(&msg, staging, shown));
                Err(EditError::Invalid(if message.is_empty() {
                    "Compose rejected the file without saying why.".to_owned()
                } else {
                    message
                }))
            }
            Err(err) => Err(EditError::ComposeUnavailable(err.to_string())),
        }
    }
}

/// Run `f` off the async runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, EditError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| EditError::io("Alchemy's file task stopped")(io::Error::other(err)))
}

/// The engine Compose talks to, as the apps task picks it.
fn endpoint(state: &State) -> Endpoint {
    state
        .engine()
        .endpoint
        .and_then(|e| Endpoint::parse(&e).ok())
        .unwrap_or_else(Endpoint::system)
}

/// The app folders in effect, or `None` without a home.
fn managed_dirs(settings: &AppSettings) -> Option<StackDirs> {
    let home = settings.home.as_deref()?;
    StackDirs::new(home, settings.extra.iter().map(PathBuf::from)).ok()
}

/// Find `kind` of the app named `name`: an app in the list, whose folder
/// is `<root>/<name>` and whose root is still an app folder. The compose
/// file is the one discovery found, until [`compose_files`] reads `.env`.
fn locate(state: &State, name: &str, kind: AppFileKind) -> Result<Target, EditError> {
    let apps = state.apps();
    let app = super::find(&apps, name).ok_or_else(|| EditError::AppNotFound(name.to_owned()))?;
    let root = PathBuf::from(&app.root);
    let dir = PathBuf::from(&app.dir);
    let managed =
        managed_dirs(&state.app_settings()).is_some_and(|dirs| dirs.all().any(|d| d == root));
    if !managed || dir != root.join(app.name.as_str()) {
        return Err(EditError::AppNotFound(name.to_owned()));
    }
    let file_name = match kind {
        AppFileKind::Compose => app.compose_file.clone(),
        AppFileKind::Env => ENV_FILE.to_owned(),
    };
    Ok(Target {
        kind,
        name: name.to_owned(),
        dir,
        file_name,
    })
}

/// Refuse an app folder that is no longer a real directory.
fn check_dir(disk: &dyn AppDisk, target: &Target) -> Result<(), EditError> {
    match disk.kind(&target.dir) {
        Ok(Some(EntryKind::Dir)) => Ok(()),
        Ok(Some(EntryKind::Link | EntryKind::LinkToDir)) => Err(EditError::Invalid(format!(
            "{} is a symlink; Alchemy only edits apps in real folders.",
            target.dir.display()
        ))),
        Ok(_) => Err(EditError::Conflict(format!(
            "{} is gone; wait a moment for Alchemy to look again.",
            target.dir.display()
        ))),
        Err(err) => Err(EditError::io(format!(
            "Alchemy couldn't look at {}",
            target.dir.display()
        ))(err)),
    }
}

/// For a compose file, the files `COMPOSE_FILE` in the app's `.env` lists,
/// if it sets one, with `target` moved to the first of them: the file
/// Compose reads first, as it does when run in the folder. A `.env` that
/// is not a regular file is refused, since Compose would follow it and
/// Alchemy does not.
fn compose_files(
    disk: &dyn AppDisk,
    mut target: Target,
) -> Result<(Target, Option<Vec<PathBuf>>), EditError> {
    if target.kind != AppFileKind::Compose {
        return Ok((target, None));
    }
    let unknown = |why: &dyn std::fmt::Display| {
        EditError::Invalid(format!(
            "{ENV_FILE} in {} {why}, so Alchemy can't tell which compose files Compose uses; edit them by hand.",
            target.dir.display()
        ))
    };
    let read = disk
        .read(&target.dir.join(ENV_FILE), MAX_FILE_LEN)
        .map_err(EditError::io(format!(
            "Alchemy couldn't read {ENV_FILE} in {}",
            target.dir.display()
        )))?;
    let bytes = match read {
        ReadOutcome::Missing => return Ok((target, None)),
        ReadOutcome::Linked => return Err(unknown(&"is a symlink")),
        ReadOutcome::NotRegular => return Err(unknown(&"isn't a regular file")),
        ReadOutcome::File(file) => file.bytes,
    };
    let text = check_content(AppFileKind::Env, &bytes).map_err(|err| unknown(&err))?;
    let entries = compose_file_entries(text).map_err(EditError::ComposeFile)?;
    if let Some(entries) = &entries {
        target.file_name = edited_compose_file(entries).map_err(EditError::ComposeFile)?;
    }
    Ok((target, entries))
}

/// The live file, or `None` for a missing `.env`.
fn read_live(disk: &dyn AppDisk, target: &Target) -> Result<Option<FileRead>, EditError> {
    let read = disk
        .read(&target.live(), MAX_FILE_LEN)
        .map_err(EditError::io(format!(
            "Alchemy couldn't read {} in {}",
            target.file_name,
            target.dir.display()
        )))?;
    match read {
        ReadOutcome::File(file) => Ok(Some(file)),
        ReadOutcome::Missing if target.kind == AppFileKind::Env => Ok(None),
        ReadOutcome::Missing => Err(EditError::Conflict(format!(
            "{} is gone from {}; wait a moment for Alchemy to look again.",
            target.file_name,
            target.dir.display()
        ))),
        ReadOutcome::Linked => Err(EditError::Invalid(format!(
            "{} in {} is a symlink; edit the file it points to by hand.",
            target.file_name,
            target.dir.display()
        ))),
        ReadOutcome::NotRegular => Err(EditError::Invalid(format!(
            "{} in {} isn't a regular file.",
            target.file_name,
            target.dir.display()
        ))),
    }
}

/// `bytes` as text to edit, refused if Alchemy could not save it back.
fn readable<'a>(target: &Target, bytes: &'a [u8]) -> Result<&'a str, EditError> {
    match check_content(AppFileKind::Env, bytes) {
        Ok(text) => Ok(text),
        Err(FileContentError::Empty) => Ok(""),
        Err(err) => Err(EditError::Invalid(format!(
            "{} {err}, so Alchemy can't edit it; edit it by hand.",
            target.file_name
        ))),
    }
}

/// Refuse unless `live` still has `base`, the hash the edit started from.
fn check_base(
    target: &Target,
    live: Option<&FileRead>,
    base: Option<&str>,
) -> Result<(), EditError> {
    let current = live.map(|file| content_hash(&file.bytes));
    if current.as_deref() == base {
        Ok(())
    } else {
        Err(EditError::Conflict(format!(
            "{} changed on disk since you opened it. Reload it and make your change again.",
            target.file_name
        )))
    }
}

/// Check the folder, the content, and the base hash, then stage the new
/// content in a staging directory inside the app's folder, so saving it is
/// a rename within that folder's filesystem.
fn stage_edit(
    disk: &Arc<dyn AppDisk>,
    target: Target,
    update: &AppFileUpdate,
) -> Result<StagedEdit, EditError> {
    check_dir(&**disk, &target)?;
    let (target, entries) = compose_files(&**disk, target)?;
    check_content(target.kind, update.content.as_bytes())
        .map_err(|err| EditError::Invalid(format!("{} {err}", target.file_name)))?;
    let live = read_live(&**disk, &target)?;
    check_base(&target, live.as_ref(), update.base_hash.as_deref())?;
    let name = StackName::new(target.name.as_str())
        .map_err(|_| EditError::AppNotFound(target.name.clone()))?;
    let staging = disk
        .make_staging(&target.dir, &staging_prefix(&name))
        .map_err(EditError::io(format!(
            "Alchemy couldn't make a temporary folder in {}",
            target.dir.display()
        )))?;
    let staging = Staging::new(disk.clone(), staging);
    let staged = staging.path().join(&target.file_name);
    let mode = live.as_ref().map_or(target.kind.new_mode(), |file| {
        target.kind.replacement_mode(file.mode)
    });
    let checked = disk
        .write_new(&staged, update.content.as_bytes(), mode)
        .map_err(EditError::io(format!(
            "Alchemy couldn't write a copy of {}",
            target.file_name
        )))
        .and_then(|()| edit_check(&**disk, &target, &staged, entries.as_deref()));
    match checked {
        Ok(check) => Ok(StagedEdit {
            target,
            staging,
            staged,
            check,
        }),
        Err(err) => {
            staging.remove_now();
            Err(err)
        }
    }
}

/// What to validate for an edit: the app's folder with the staged `.env`,
/// or with the compose files Compose would read there and the staged
/// compose file in place of the live one. Those are the `entries` of
/// `COMPOSE_FILE` in `.env` if it sets one, and the compose file and the
/// app's override if not.
///
/// A staged `.env` replaces the live one with `--env-file`, so a
/// `COMPOSE_FILE` in it is honoured by Compose itself.
fn edit_check(
    disk: &dyn AppDisk,
    target: &Target,
    staged: &Path,
    entries: Option<&[PathBuf]>,
) -> Result<ComposeCheck, EditError> {
    let invalid = |e: crate::compose::ComposeProjectError| {
        EditError::Invalid(format!("Alchemy can't use this folder: {e}"))
    };
    let check = ComposeCheck::new(ComposeProject::new(&target.dir).map_err(invalid)?);
    let files = match (target.kind, entries) {
        (AppFileKind::Env, _) => return check.with_env_file(staged).map_err(invalid),
        (AppFileKind::Compose, Some(entries)) => {
            compose_file_args(entries, &target.dir, &target.file_name, staged)
                .map_err(EditError::ComposeFile)?
        }
        (AppFileKind::Compose, None) => std::iter::once(staged.to_owned())
            .chain(
                OVERRIDE_FILES
                    .iter()
                    .map(|name| target.dir.join(name))
                    .find(|path| matches!(disk.kind(path), Ok(Some(EntryKind::File)))),
            )
            .collect(),
    };
    files
        .into_iter()
        .try_fold(check, ComposeCheck::with_file)
        .map_err(invalid)
}

/// Keep the live file's owner and group on its replacement. A group the
/// daemon's user is not in cannot be kept, and the edit is refused rather
/// than changing who can read the file.
fn keep_owner(
    disk: &dyn AppDisk,
    staged: &Path,
    live: &FileRead,
    file_name: &str,
) -> Result<(), EditError> {
    let owner = disk.owner(staged).map_err(EditError::io(format!(
        "Alchemy couldn't check its copy of {file_name}"
    )))?;
    if owner == (live.uid, live.gid) {
        return Ok(());
    }
    disk.set_owner(staged, live.uid, live.gid)
        .map_err(EditError::io(format!(
            "Alchemy couldn't give the new {file_name} the same owner and group as the old one"
        )))
}

/// What a failed rename into place means. A rename that would cross
/// filesystems is reported as such, since it means the folder changed
/// under Alchemy, not that saving can work on a retry.
fn rename_failed(err: io::Error, what: String) -> EditError {
    if err.kind() == io::ErrorKind::CrossesDevices {
        EditError::Conflict(format!(
            "{what}: its temporary copy ended up on a different filesystem, so it couldn't be moved into place. Check that nothing is mounted inside the folder and try again."
        ))
    } else {
        EditError::io(what)(err)
    }
}

fn exists(target: &Path) -> EditError {
    EditError::Conflict(format!(
        "{} already exists. Alchemy won't replace a folder, since it may hold data; choose another name.",
        target.display()
    ))
}

/// Check where a new app goes, creating the home if this is its first
/// app, and make its staging folder. A dry run never creates the home: it
/// stages in the system's temporary folder instead, which a new app's lone
/// folder cannot tell apart.
///
/// Returns the staging folder and the folders made for the home, deepest
/// first, which are removed again if making the staging folder fails.
fn begin_create(
    disk: &dyn AppDisk,
    root: &Path,
    is_home: bool,
    dry_run: bool,
    target: &Path,
    name: &StackName,
) -> Result<(PathBuf, Vec<PathBuf>), EditError> {
    let look = |path: &Path| {
        disk.kind(path).map_err(EditError::io(format!(
            "Alchemy couldn't look at {}",
            path.display()
        )))
    };
    let mut staging_root = root.to_owned();
    let mut created = Vec::new();
    match look(root)? {
        Some(EntryKind::Dir | EntryKind::Link | EntryKind::LinkToDir) => {}
        None if is_home && dry_run => staging_root = std::env::temp_dir(),
        None if is_home => created = create_home(disk, root)?,
        None => {
            return Err(EditError::Invalid(format!(
                "{} doesn't exist. Create it, or choose another app folder.",
                root.display()
            )));
        }
        Some(_) => {
            return Err(EditError::Invalid(format!(
                "{} isn't a folder.",
                root.display()
            )));
        }
    }
    let staging = look(target).and_then(|found| match found {
        Some(_) => Err(exists(target)),
        None => disk
            .make_staging(&staging_root, &staging_prefix(name))
            .map_err(EditError::io(format!(
                "Alchemy couldn't make a temporary folder in {}",
                staging_root.display()
            ))),
    });
    match staging {
        Ok(staging) => Ok((staging, created)),
        Err(err) => {
            remove_created(disk, &created);
            Err(err)
        }
    }
}

/// Create the missing home `root` with any missing parents, and return
/// the folders that were missing, deepest first: `root` up to, but not
/// including, the first that already existed.
fn create_home(disk: &dyn AppDisk, root: &Path) -> Result<Vec<PathBuf>, EditError> {
    let mut missing = Vec::new();
    for dir in root.ancestors() {
        let found = disk.kind(dir).map_err(EditError::io(format!(
            "Alchemy couldn't look at {}",
            dir.display()
        )))?;
        if found.is_some() {
            break;
        }
        missing.push(dir.to_owned());
    }
    disk.create_dirs(root).map_err(EditError::io(format!(
        "Alchemy couldn't create {}",
        root.display()
    )))?;
    Ok(missing)
}

/// Write the new app's folder and files inside its staging folder.
fn stage_new_app(disk: &dyn AppDisk, dir: &Path, new: &NewApp) -> Result<(), EditError> {
    disk.make_dir(dir, 0o755)
        .map_err(EditError::io("Alchemy couldn't make the app's folder"))?;
    disk.write_new(
        &dir.join(NEW_COMPOSE_FILE),
        new.compose.as_bytes(),
        AppFileKind::Compose.new_mode(),
    )
    .map_err(EditError::io(format!(
        "Alchemy couldn't write {NEW_COMPOSE_FILE}"
    )))?;
    if let Some(env) = &new.env {
        disk.write_new(
            &dir.join(ENV_FILE),
            env.as_bytes(),
            AppFileKind::Env.new_mode(),
        )
        .map_err(EditError::io(format!("Alchemy couldn't write {ENV_FILE}")))?;
    }
    Ok(())
}

/// The app just created, as Compose resolved it. Its containers and status
/// follow once the apps task lists it.
fn new_app(name: StackName, root: &Path, dir: &Path, config: &ComposeConfig) -> App {
    App {
        name,
        status: StackStatus::Unknown,
        dir: dir.to_string_lossy().into_owned(),
        root: root.to_string_lossy().into_owned(),
        compose_file: NEW_COMPOSE_FILE.to_owned(),
        project: Some(config.name.clone()),
        images: Some(config.images()),
        services: config
            .services
            .iter()
            .map(|service| AppService {
                name: service.name.clone(),
                containers: Vec::new(),
            })
            .collect(),
        problems: Vec::new(),
        stop_reason: StopReason::default(),
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
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use alchemy_api::apps::AppList;

    use super::*;
    use crate::compose::{FakeComposeValidator, FakeConfig, ServiceConfig};

    const COMPOSE: &str = "services:\n  web:\n    image: nginx\n";

    /// A daemon's view of one app folder: `root/apps` is the home, with
    /// `blog` in it, and `root/extra` is configured but empty.
    struct Fixture {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        extra: PathBuf,
        blog: PathBuf,
        state: State,
        validator: FakeComposeValidator,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("apps");
            let extra = tmp.path().join("extra");
            let blog = home.join("blog");
            fs::create_dir_all(&blog).unwrap();
            fs::create_dir_all(&extra).unwrap();
            fs::write(blog.join("compose.yaml"), COMPOSE).unwrap();
            fs::set_permissions(blog.join("compose.yaml"), fs::Permissions::from_mode(0o640))
                .unwrap();
            let state = State::new();
            state.set_app_settings(AppSettings {
                home: Some(home.to_string_lossy().into_owned()),
                default_home: None,
                extra: vec![extra.to_string_lossy().into_owned()],
            });
            state.set_apps(AppList {
                apps: vec![new_app(
                    StackName::new("blog").unwrap(),
                    &home,
                    &blog,
                    &ComposeConfig {
                        name: "blog".into(),
                        services: vec![],
                    },
                )],
                ..AppList::default()
            });
            Self {
                _tmp: tmp,
                home,
                extra,
                blog,
                state,
                validator: FakeComposeValidator::new(),
            }
        }

        fn editor(&self) -> AppEditor {
            self.editor_on(Arc::new(SystemDisk))
        }

        fn editor_on(&self, disk: Arc<dyn AppDisk>) -> AppEditor {
            AppEditor::new(disk, Arc::new(self.validator.clone()))
        }

        async fn read(&self, kind: AppFileKind) -> AppFile {
            self.editor().read(&self.state, "blog", kind).await.unwrap()
        }

        async fn write(
            &self,
            kind: AppFileKind,
            content: &str,
            base_hash: Option<String>,
        ) -> Result<AppFile, EditError> {
            let update = AppFileUpdate {
                content: content.into(),
                base_hash,
                dry_run: false,
            };
            self.editor().write(&self.state, "blog", kind, update).await
        }
    }

    fn new(name: &str, folder: Option<&Path>) -> NewApp {
        NewApp {
            name: name.into(),
            folder: folder.map(|f| f.to_string_lossy().into_owned()),
            compose: COMPOSE.into(),
            env: Some("SECRET=1\n".into()),
            dry_run: false,
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn hashes_are_sha256_hex() {
        let cases = [
            (
                "",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                "abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
        ];
        for (text, want) in cases {
            assert_eq!(content_hash(text.as_bytes()), want, "{text:?}");
        }
    }

    #[tokio::test]
    async fn creates_an_app_in_the_home_with_private_env() {
        let f = Fixture::new();
        f.validator.answer(FakeConfig::Config(ComposeConfig {
            name: "shop".into(),
            services: vec![ServiceConfig {
                name: "web".into(),
                builds: true,
            }],
        }));
        let app = f
            .editor()
            .create(&f.state, new("shop", None))
            .await
            .unwrap();
        let dir = f.home.join("shop");
        assert_eq!(app.dir, dir.to_string_lossy());
        assert_eq!(app.root, f.home.to_string_lossy());
        assert_eq!(app.project.as_deref(), Some("shop"));
        assert_eq!(app.images, Some(alchemy_api::apps::ImageSource::Build));
        assert_eq!(app.services[0].name, "web");
        assert_eq!(
            fs::read_to_string(dir.join("compose.yaml")).unwrap(),
            COMPOSE
        );
        assert_eq!(fs::read_to_string(dir.join(".env")).unwrap(), "SECRET=1\n");
        assert_eq!(mode(&dir), 0o755);
        assert_eq!(mode(&dir.join("compose.yaml")), 0o644);
        assert_eq!(mode(&dir.join(".env")), 0o600);
        assert_eq!(entries(&f.home), ["blog", "shop"]);

        let checks = f.validator.checks();
        let staged = checks[0].project().dir();
        assert_eq!(staged.file_name().unwrap(), "shop");
        assert_eq!(staged.parent().unwrap().parent().unwrap(), f.home);
        assert_eq!(checks[0].files(), [] as [PathBuf; 0]);
    }

    #[tokio::test]
    async fn creates_the_home_on_first_use_and_in_a_named_folder() {
        let f = Fixture::new();
        fs::remove_dir_all(&f.home).unwrap();
        f.state.set_apps(AppList::default());
        let editor = f.editor();
        let mut dry = new("blog", None);
        dry.dry_run = true;
        editor.create(&f.state, dry).await.unwrap();
        assert!(!f.home.exists(), "a dry run created the home");
        editor.create(&f.state, new("blog", None)).await.unwrap();
        assert!(f.home.join("blog/compose.yaml").is_file());

        let mut no_env = new("shop", Some(&f.extra));
        no_env.env = None;
        editor.create(&f.state, no_env).await.unwrap();
        assert_eq!(entries(&f.extra.join("shop")), ["compose.yaml"]);
    }

    #[tokio::test]
    async fn refuses_to_create_where_it_must_not() {
        let f = Fixture::new();
        fs::create_dir(f.home.join("data")).unwrap();
        fs::write(f.home.join("data/db.sqlite"), "keep").unwrap();
        fs::write(f.home.join("notes"), "keep").unwrap();
        symlink(&f.extra, f.home.join("linked")).unwrap();
        let elsewhere = f.home.parent().unwrap().join("elsewhere");
        let missing_extra = f.home.parent().unwrap().join("gone");
        f.state.set_app_settings(AppSettings {
            home: Some(f.home.to_string_lossy().into_owned()),
            default_home: None,
            extra: vec![
                f.extra.to_string_lossy().into_owned(),
                missing_extra.to_string_lossy().into_owned(),
            ],
        });
        let mut empty = new("empty", None);
        empty.compose = " \n".into();
        let mut nul_env = new("nul", None);
        nul_env.env = Some("A=\0".into());
        let cases: Vec<(NewApp, &str)> = vec![
            (new("data", None), "conflict"),
            (new("notes", None), "conflict"),
            (new("linked", None), "conflict"),
            (new("blog", Some(&f.extra)), "conflict"),
            (new("Bad Name", None), "invalid"),
            (new("../etc", None), "invalid"),
            (new("x", Some(&elsewhere)), "invalid"),
            (new("x", Some(Path::new("relative"))), "invalid"),
            (new("x", Some(&missing_extra)), "invalid"),
            (empty, "invalid"),
            (nul_env, "invalid"),
        ];
        let editor = f.editor();
        for (request, want) in cases {
            let name = request.name.clone();
            let err = editor.create(&f.state, request).await.unwrap_err();
            let got = match err.code() {
                ErrorCode::Conflict => "conflict",
                ErrorCode::Invalid => "invalid",
                other => panic!("{name}: {other:?} {err}"),
            };
            assert_eq!(got, want, "{name}: {err}");
        }
        assert_eq!(
            fs::read_to_string(f.home.join("data/db.sqlite")).unwrap(),
            "keep"
        );
        assert_eq!(entries(&f.home), ["blog", "data", "linked", "notes"]);
        assert_eq!(entries(&f.extra), [] as [String; 0]);
        assert!(!missing_extra.exists());
        assert_eq!(f.validator.checks(), []);
    }

    #[tokio::test]
    async fn a_rejected_or_dry_run_create_leaves_nothing_behind() {
        let f = Fixture::new();
        let editor = f.editor();
        let mut dry = new("shop", None);
        dry.dry_run = true;
        let app = editor.create(&f.state, dry).await.unwrap();
        assert_eq!(app.name.as_str(), "shop");
        assert_eq!(entries(&f.home), ["blog"]);

        f.validator.answer(FakeConfig::Invalid(String::new()));
        let err = editor
            .create(&f.state, new("shop", None))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Compose rejected the file without saying why."
        );
        f.validator.on_validate({
            let validator = f.validator.clone();
            move |check| {
                let staged = check.project().dir();
                validator.answer(FakeConfig::Invalid(format!(
                    "{}/compose.yaml: services.web.image must be a string, in {}",
                    staged.display(),
                    staged.parent().unwrap().display()
                )));
            }
        });
        let err = editor
            .create(&f.state, new("shop", None))
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::Invalid);
        assert_eq!(
            err.to_string(),
            format!(
                "compose.yaml: services.web.image must be a string, in {}",
                f.home.display()
            )
        );
        assert!(!err.to_string().contains(".alchemy-tmp"));
        assert_eq!(entries(&f.home), ["blog"]);

        f.validator.on_validate(|_| {});
        f.validator.answer(FakeConfig::NoDocker);
        let err = editor
            .create(&f.state, new("shop", None))
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::ComposeUnavailable(_)), "{err:?}");
        assert_eq!(entries(&f.home), ["blog"]);
    }

    #[tokio::test]
    async fn a_failed_first_create_removes_the_home_it_made() {
        enum Fail {
            Rejected,
            NoDocker,
            Rename,
            Exists,
        }
        let cases = [
            (Fail::Rejected, ErrorCode::Invalid),
            (Fail::NoDocker, ErrorCode::Internal),
            (Fail::Rename, ErrorCode::Internal),
            (Fail::Exists, ErrorCode::Conflict),
        ];
        for (fail, want) in cases {
            let f = Fixture::new();
            let tmp = f.extra.parent().unwrap().to_owned();
            let home = tmp.join("Alchemy/apps");
            f.state.set_app_settings(AppSettings {
                home: Some(home.to_string_lossy().into_owned()),
                default_home: None,
                extra: vec![],
            });
            f.state.set_apps(AppList::default());
            let mut editor = f.editor();
            match fail {
                Fail::Rejected => f.validator.answer(FakeConfig::Invalid("bad".into())),
                Fail::NoDocker => f.validator.answer(FakeConfig::NoDocker),
                Fail::Rename => editor = f.editor_on(Renames::failing(io::ErrorKind::Other)),
                Fail::Exists => f.validator.on_validate({
                    let home = home.clone();
                    move |_| fs::create_dir(home.join("blog")).unwrap()
                }),
            }
            let err = editor
                .create(&f.state, new("blog", None))
                .await
                .unwrap_err();
            assert_eq!(err.code(), want, "{err}");
            match fail {
                Fail::Exists => assert_eq!(entries(&home), ["blog"]),
                _ => assert_eq!(entries(&tmp), ["apps", "extra"], "{err}"),
            }
        }
    }

    #[tokio::test]
    async fn a_failed_create_keeps_a_home_that_was_already_there() {
        let f = Fixture::new();
        f.state.set_app_settings(AppSettings {
            home: Some(f.extra.to_string_lossy().into_owned()),
            default_home: None,
            extra: vec![],
        });
        f.state.set_apps(AppList::default());
        f.validator.answer(FakeConfig::Invalid("bad".into()));
        f.editor()
            .create(&f.state, new("shop", None))
            .await
            .unwrap_err();
        assert!(f.extra.is_dir());
        assert_eq!(entries(&f.extra), [] as [String; 0]);

        f.validator.answer(FakeConfig::Config(ComposeConfig {
            name: "shop".into(),
            services: vec![],
        }));
        fs::remove_dir(&f.extra).unwrap();
        f.editor()
            .create(&f.state, new("shop", None))
            .await
            .unwrap();
        assert_eq!(entries(&f.extra.join("shop")), [".env", "compose.yaml"]);
    }

    #[tokio::test]
    async fn reads_files_with_their_hashes() {
        let f = Fixture::new();
        let compose = f.read(AppFileKind::Compose).await;
        assert_eq!(compose.name, "compose.yaml");
        assert_eq!(compose.content, COMPOSE);
        assert_eq!(compose.hash, Some(content_hash(COMPOSE.as_bytes())));
        let env = f.read(AppFileKind::Env).await;
        assert_eq!(env.name, ".env");
        assert_eq!(env.content, "");
        assert_eq!(env.hash, None);
    }

    #[tokio::test]
    async fn refuses_to_read_or_edit_what_it_must_not() {
        let f = Fixture::new();
        let editor = f.editor();
        let err = editor
            .read(&f.state, "nope", AppFileKind::Compose)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::AppNotFound);

        fs::write(f.blog.join("real.env"), "A=1\n").unwrap();
        symlink(f.blog.join("real.env"), f.blog.join(".env")).unwrap();
        let err = editor
            .read(&f.state, "blog", AppFileKind::Env)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::Invalid, "{err}");
        let err = f.write(AppFileKind::Env, "B=2\n", None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::Invalid, "{err}");
        assert_eq!(
            fs::read_to_string(f.blog.join("real.env")).unwrap(),
            "A=1\n"
        );
        fs::remove_file(f.blog.join(".env")).unwrap();

        fs::write(f.blog.join("compose.yaml"), b"x: \xff").unwrap();
        let err = editor
            .read(&f.state, "blog", AppFileKind::Compose)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("isn't UTF-8"), "{err}");

        let moved = f.home.join("moved");
        fs::rename(&f.blog, &moved).unwrap();
        symlink(&moved, &f.blog).unwrap();
        let err = editor
            .read(&f.state, "blog", AppFileKind::Compose)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("is a symlink"), "{err}");

        f.state.set_app_settings(AppSettings {
            home: Some(f.extra.to_string_lossy().into_owned()),
            default_home: None,
            extra: vec![],
        });
        let err = editor
            .read(&f.state, "blog", AppFileKind::Compose)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::AppNotFound, "{err}");
        assert_eq!(f.validator.checks(), []);
    }

    #[tokio::test]
    async fn edits_replace_only_the_edited_file_keeping_its_mode() {
        let f = Fixture::new();
        fs::write(f.blog.join("compose.override.yaml"), "services: {}\n").unwrap();
        fs::create_dir(f.blog.join("data")).unwrap();
        fs::write(f.blog.join("data/db"), "keep").unwrap();
        let before = f.read(AppFileKind::Compose).await;
        let edited = format!("{COMPOSE}    restart: always\n");

        let saved = f
            .write(AppFileKind::Compose, &edited, before.hash.clone())
            .await
            .unwrap();
        assert_eq!(saved.content, edited);
        assert_eq!(saved.hash, Some(content_hash(edited.as_bytes())));
        assert_eq!(
            fs::read_to_string(f.blog.join("compose.yaml")).unwrap(),
            edited
        );
        assert_eq!(mode(&f.blog.join("compose.yaml")), 0o640);
        assert_eq!(fs::read_to_string(f.blog.join("data/db")).unwrap(), "keep");
        assert_eq!(
            entries(&f.blog),
            ["compose.override.yaml", "compose.yaml", "data"]
        );
        assert_eq!(entries(&f.home), ["blog"]);

        let check = &f.validator.checks()[0];
        assert_eq!(check.project().dir(), f.blog);
        assert_eq!(check.files().len(), 2);
        assert_eq!(check.files()[0].file_name().unwrap(), "compose.yaml");
        let staging = check.files()[0].parent().unwrap();
        assert_eq!(staging.parent().unwrap(), f.blog);
        assert_eq!(check.files()[1], f.blog.join("compose.override.yaml"));
        assert_eq!(check.env_file(), None);
    }

    #[tokio::test]
    async fn edits_validate_against_the_files_compose_file_in_env_lists() {
        let cases: [(&str, &str, &[&str]); 3] = [
            (
                "COMPOSE_FILE=compose.yaml:compose.prod.yaml\n",
                "compose.yaml",
                &["<staged>", "compose.prod.yaml"],
            ),
            (
                "export COMPOSE_FILE='./base.yaml;compose.yaml' # main\nCOMPOSE_PATH_SEPARATOR=;\n",
                "base.yaml",
                &["<staged>", "compose.yaml"],
            ),
            (
                "A=1\n",
                "compose.yaml",
                &["<staged>", "compose.override.yaml"],
            ),
        ];
        for (env, edited, want) in cases {
            let f = Fixture::new();
            for name in ["compose.prod.yaml", "compose.override.yaml", "base.yaml"] {
                fs::write(f.blog.join(name), format!("# {name}\n{COMPOSE}")).unwrap();
            }
            fs::write(f.blog.join(".env"), env).unwrap();
            let before = f.read(AppFileKind::Compose).await;
            assert_eq!(before.name, edited, "{env:?}");
            let live = fs::read_to_string(f.blog.join(edited)).unwrap();
            assert_eq!(before.content, live, "{env:?}");
            let saved = f
                .write(AppFileKind::Compose, "services: {}\n", before.hash)
                .await
                .unwrap();
            assert_eq!(saved.name, edited, "{env:?}");
            assert_eq!(
                fs::read_to_string(f.blog.join(edited)).unwrap(),
                "services: {}\n",
                "{env:?}"
            );
            let check = &f.validator.checks()[0];
            let staged = &check.files()[0];
            assert_eq!(staged.file_name().unwrap(), edited, "{env:?}");
            assert_eq!(staged.parent().unwrap().parent().unwrap(), f.blog);
            let files: Vec<PathBuf> = want
                .iter()
                .map(|name| match *name {
                    "<staged>" => staged.clone(),
                    name => f.blog.join(name),
                })
                .collect();
            assert_eq!(check.files(), files, "{env:?}");
        }
    }

    #[tokio::test]
    async fn refuses_to_edit_when_compose_file_in_env_cant_be_followed() {
        let cases = [
            ("F=compose.yaml\nCOMPOSE_FILE=${F}\n", "uses a variable"),
            ("COMPOSE_FILE=\"$F\"\n", "uses a variable"),
            ("COMPOSE_FILE='compose.yaml\n", "isn't closed"),
            (
                "COMPOSE_FILE=/etc/compose.yaml\n",
                "/etc/compose.yaml, which isn't",
            ),
            (
                "COMPOSE_FILE=sub/compose.yaml\n",
                "sub/compose.yaml, which isn't",
            ),
            (
                "COMPOSE_FILE=../blog2/compose.yaml:compose.yaml\n",
                "../blog2/compose.yaml, which isn't",
            ),
            ("COMPOSE_FILE=compose.yaml::x.yaml\n", "empty entry"),
        ];
        for (env, want) in cases {
            let f = Fixture::new();
            fs::write(f.blog.join(".env"), env).unwrap();
            let err = f
                .editor()
                .read(&f.state, "blog", AppFileKind::Compose)
                .await
                .unwrap_err();
            assert!(err.to_string().contains(want), "{env:?}: {err}");
            let base = Some(content_hash(COMPOSE.as_bytes()));
            let err = f
                .write(AppFileKind::Compose, "services: {}\n", base)
                .await
                .unwrap_err();
            assert_eq!(err.code(), ErrorCode::Invalid, "{env:?}: {err}");
            assert!(err.to_string().contains(want), "{env:?}: {err}");
            assert_eq!(
                fs::read_to_string(f.blog.join("compose.yaml")).unwrap(),
                COMPOSE
            );
            assert_eq!(f.validator.checks(), [], "{env:?}");
            assert_eq!(entries(&f.blog), [".env", "compose.yaml"], "{env:?}");
        }

        let f = Fixture::new();
        fs::write(f.blog.join("real.env"), "COMPOSE_FILE=compose.yaml\n").unwrap();
        symlink(f.blog.join("real.env"), f.blog.join(".env")).unwrap();
        let err = f
            .editor()
            .read(&f.state, "blog", AppFileKind::Compose)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("is a symlink"), "{err}");
    }

    #[tokio::test]
    async fn an_env_edit_leaves_compose_file_to_compose() {
        let f = Fixture::new();
        fs::write(f.blog.join("compose.prod.yaml"), COMPOSE).unwrap();
        f.write(
            AppFileKind::Env,
            "COMPOSE_FILE=compose.yaml:compose.prod.yaml\n",
            None,
        )
        .await
        .unwrap();
        let check = &f.validator.checks()[0];
        assert_eq!(check.files(), [] as [PathBuf; 0]);
        assert_eq!(check.env_file().unwrap().file_name().unwrap(), ".env");
    }

    #[tokio::test]
    async fn stages_an_edit_privately_inside_the_app_folder_unseen_by_discovery() {
        use alchemy_core::stack::{ListedDir, discover};

        use crate::apps::{AppFs as _, SystemFs};

        type Seen = (PathBuf, u32, u32, Vec<String>, Vec<(String, Vec<String>)>);

        let f = Fixture::new();
        let base = f.read(AppFileKind::Compose).await.hash;
        let seen: Arc<std::sync::Mutex<Option<Seen>>> = Arc::default();
        f.validator.on_validate({
            let (seen, home) = (seen.clone(), f.home.clone());
            move |check| {
                let staged = check.files()[0].clone();
                let staging = staged.parent().unwrap().to_owned();
                let found = discover(&[ListedDir {
                    dir: home.clone(),
                    listing: SystemFs.list(&home),
                }]);
                let stacks = found
                    .stacks
                    .iter()
                    .map(|s| (s.name.to_string(), s.files.keys().cloned().collect()))
                    .collect();
                *seen.lock().unwrap() = Some((
                    staging.clone(),
                    mode(&staging),
                    mode(&staged),
                    entries(&staging),
                    stacks,
                ));
            }
        });
        f.write(AppFileKind::Compose, COMPOSE, base).await.unwrap();

        let (staging, dir_mode, file_mode, staged, stacks) = seen.lock().unwrap().take().unwrap();
        assert_eq!(staging.parent().unwrap(), f.blog);
        assert!(
            staging
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".alchemy-tmp-blog-"),
            "{}",
            staging.display()
        );
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o640);
        assert_eq!(staged, ["compose.yaml"]);
        assert_eq!(
            stacks,
            [("blog".to_owned(), vec!["compose.yaml".to_owned()])]
        );
        assert!(!staging.exists());
        assert_eq!(entries(&f.blog), ["compose.yaml"]);
    }

    #[tokio::test]
    async fn edits_env_privately_whether_or_not_it_existed() {
        let f = Fixture::new();
        f.write(AppFileKind::Env, "A=1\n", None).await.unwrap();
        assert_eq!(mode(&f.blog.join(".env")), 0o600);
        fs::set_permissions(f.blog.join(".env"), fs::Permissions::from_mode(0o644)).unwrap();
        let base = f.read(AppFileKind::Env).await.hash;
        f.write(AppFileKind::Env, "A=2\n", base).await.unwrap();
        assert_eq!(fs::read_to_string(f.blog.join(".env")).unwrap(), "A=2\n");
        assert_eq!(mode(&f.blog.join(".env")), 0o600);

        let check = &f.validator.checks()[1];
        assert_eq!(check.project().dir(), f.blog);
        assert_eq!(check.files(), [] as [PathBuf; 0]);
        assert_eq!(check.env_file().unwrap().file_name().unwrap(), ".env");
    }

    #[tokio::test]
    async fn a_rejected_or_dry_run_edit_leaves_the_file_untouched() {
        let f = Fixture::new();
        let base = f.read(AppFileKind::Compose).await.hash;
        let update = AppFileUpdate {
            content: "services: [\n".into(),
            base_hash: base.clone(),
            dry_run: true,
        };
        let saved = f
            .editor()
            .write(&f.state, "blog", AppFileKind::Compose, update)
            .await
            .unwrap();
        assert_eq!(saved.content, "services: [\n");
        assert_eq!(
            fs::read_to_string(f.blog.join("compose.yaml")).unwrap(),
            COMPOSE
        );

        f.validator.on_validate({
            let validator = f.validator.clone();
            move |check| {
                validator.answer(FakeConfig::Invalid(format!(
                    "{}: yaml: line 1: did not find expected node content",
                    check.files()[0].display()
                )));
            }
        });
        let err = f
            .write(AppFileKind::Compose, "services: [\n", base)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::Invalid);
        assert_eq!(
            err.to_string(),
            "compose.yaml: yaml: line 1: did not find expected node content"
        );
        assert_eq!(
            fs::read_to_string(f.blog.join("compose.yaml")).unwrap(),
            COMPOSE
        );
        assert_eq!(entries(&f.home), ["blog"]);
    }

    #[tokio::test]
    async fn a_hand_edit_is_never_overwritten() {
        let f = Fixture::new();
        let base = f.read(AppFileKind::Compose).await.hash;
        fs::write(f.blog.join("compose.yaml"), "services: {}\n# by hand\n").unwrap();
        let err = f
            .write(AppFileKind::Compose, COMPOSE, base)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::Conflict);
        assert_eq!(
            err.to_string(),
            "compose.yaml changed on disk since you opened it. Reload it and make your change again."
        );
        assert_eq!(f.validator.checks(), []);

        let base = f.read(AppFileKind::Compose).await.hash;
        let live = f.blog.join("compose.yaml");
        f.validator.on_validate({
            let live = live.clone();
            move |_| fs::write(&live, "services: {}\n# again\n").unwrap()
        });
        let err = f
            .write(AppFileKind::Compose, COMPOSE, base)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::Conflict);
        assert_eq!(
            fs::read_to_string(&live).unwrap(),
            "services: {}\n# again\n"
        );

        fs::write(f.blog.join(".env"), "A=1\n").unwrap();
        let err = f.write(AppFileKind::Env, "A=2\n", None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::Conflict);
        assert_eq!(fs::read_to_string(f.blog.join(".env")).unwrap(), "A=1\n");
        assert_eq!(entries(&f.home), ["blog"]);
    }

    type RenameFault = dyn Fn(&Path, &Path) -> Option<io::Error> + Send + Sync;

    /// [`SystemDisk`], except that a rename fails with the error the fault
    /// gives for it, if any.
    struct Renames(Box<RenameFault>);

    impl std::fmt::Debug for Renames {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Renames")
        }
    }

    impl Renames {
        fn failing(kind: io::ErrorKind) -> Arc<dyn AppDisk> {
            Arc::new(Self(Box::new(move |_, _| Some(kind.into()))))
        }

        /// As if `mount` were a filesystem of its own: a rename into or out
        /// of it fails with `EXDEV`.
        fn mounted(mount: PathBuf) -> Arc<dyn AppDisk> {
            Arc::new(Self(Box::new(move |from, to| {
                (from.starts_with(&mount) != to.starts_with(&mount))
                    .then(|| io::ErrorKind::CrossesDevices.into())
            })))
        }

        fn rename(
            &self,
            from: &Path,
            to: &Path,
            rename: impl FnOnce() -> io::Result<()>,
        ) -> io::Result<()> {
            match (self.0)(from, to) {
                Some(err) => Err(err),
                None => rename(),
            }
        }
    }

    impl AppDisk for Renames {
        fn kind(&self, path: &Path) -> io::Result<Option<EntryKind>> {
            SystemDisk.kind(path)
        }
        fn read(&self, path: &Path, max: usize) -> io::Result<ReadOutcome> {
            SystemDisk.read(path, max)
        }
        fn create_dirs(&self, dir: &Path) -> io::Result<()> {
            SystemDisk.create_dirs(dir)
        }
        fn make_staging(&self, root: &Path, prefix: &str) -> io::Result<PathBuf> {
            SystemDisk.make_staging(root, prefix)
        }
        fn make_dir(&self, dir: &Path, mode: u32) -> io::Result<()> {
            SystemDisk.make_dir(dir, mode)
        }
        fn write_new(&self, path: &Path, content: &[u8], mode: u32) -> io::Result<()> {
            SystemDisk.write_new(path, content, mode)
        }
        fn owner(&self, path: &Path) -> io::Result<(u32, u32)> {
            SystemDisk.owner(path)
        }
        fn set_owner(&self, path: &Path, uid: u32, gid: u32) -> io::Result<()> {
            SystemDisk.set_owner(path, uid, gid)
        }
        fn rename_new(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.rename(from, to, || SystemDisk.rename_new(from, to))
        }
        fn rename_over(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.rename(from, to, || SystemDisk.rename_over(from, to))
        }
        fn remove_staging(&self, dir: &Path) -> io::Result<()> {
            SystemDisk.remove_staging(dir)
        }
        fn remove_empty_dir(&self, dir: &Path) -> io::Result<()> {
            SystemDisk.remove_empty_dir(dir)
        }
    }

    #[tokio::test]
    async fn saves_in_an_app_folder_that_is_a_mount_of_its_own() {
        let f = Fixture::new();
        let editor = f.editor_on(Renames::mounted(f.blog.clone()));
        let base = f.read(AppFileKind::Compose).await.hash;
        let edited = format!("{COMPOSE}    restart: always\n");
        let updates = [
            (AppFileKind::Compose, edited.clone(), base),
            (AppFileKind::Env, "A=1\n".to_owned(), None),
        ];
        for (kind, content, base_hash) in updates {
            let update = AppFileUpdate {
                content,
                base_hash,
                dry_run: false,
            };
            editor
                .write(&f.state, "blog", kind, update)
                .await
                .unwrap_or_else(|err| panic!("{kind:?}: {err}"));
        }
        assert_eq!(
            fs::read_to_string(f.blog.join("compose.yaml")).unwrap(),
            edited
        );
        assert_eq!(fs::read_to_string(f.blog.join(".env")).unwrap(), "A=1\n");
        assert_eq!(entries(&f.blog), [".env", "compose.yaml"]);
        assert_eq!(entries(&f.home), ["blog"]);
    }

    #[tokio::test]
    async fn a_failed_rename_changes_nothing_and_cleans_up() {
        let cases = [
            (io::ErrorKind::CrossesDevices, ErrorCode::Conflict),
            (io::ErrorKind::PermissionDenied, ErrorCode::Internal),
        ];
        for (kind, want) in cases {
            let f = Fixture::new();
            let editor = f.editor_on(Renames::failing(kind));
            let base = f.read(AppFileKind::Compose).await.hash;
            let update = AppFileUpdate {
                content: "services: {}\n".into(),
                base_hash: base,
                dry_run: false,
            };
            let err = editor
                .write(&f.state, "blog", AppFileKind::Compose, update)
                .await
                .unwrap_err();
            assert_eq!(err.code(), want, "{kind:?}: {err}");
            assert!(
                err.to_string()
                    .starts_with("Alchemy couldn't save compose.yaml in"),
                "{err}"
            );
            assert!(!err.to_string().contains(".alchemy-tmp"), "{err}");
            assert_eq!(
                fs::read_to_string(f.blog.join("compose.yaml")).unwrap(),
                COMPOSE
            );
            assert_eq!(entries(&f.blog), ["compose.yaml"], "{kind:?}");

            let err = editor
                .create(&f.state, new("shop", None))
                .await
                .unwrap_err();
            assert_eq!(err.code(), want, "{kind:?}: {err}");
            assert_eq!(entries(&f.home), ["blog"], "{kind:?}");
        }
    }

    #[tokio::test]
    async fn an_abandoned_staging_folder_is_removed_in_the_background() {
        let f = Fixture::new();
        let staging = SystemDisk
            .make_staging(&f.blog, ".alchemy-tmp-blog-")
            .unwrap();
        drop(Staging::new(Arc::new(SystemDisk), staging.clone()));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while staging.exists() {
            assert!(tokio::time::Instant::now() < deadline, "never removed");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(entries(&f.blog), ["compose.yaml"]);
    }

    #[tokio::test]
    async fn an_abandoned_first_create_removes_the_home_it_made() {
        let f = Fixture::new();
        let made = f.extra.join("Alchemy");
        let home = made.join("apps");
        fs::create_dir_all(&home).unwrap();
        let staging = SystemDisk
            .make_staging(&home, ".alchemy-tmp-blog-")
            .unwrap();
        fs::create_dir(staging.join("blog")).unwrap();
        drop(Staging::with_created(
            Arc::new(SystemDisk),
            staging,
            vec![home, made.clone()],
        ));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while made.exists() {
            assert!(tokio::time::Instant::now() < deadline, "never removed");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(entries(&f.extra), [] as [String; 0]);
    }
}
