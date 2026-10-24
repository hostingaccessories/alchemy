//! Creating apps and editing their files: [`AppEditor`].
//!
//! Nothing reaches an app folder until Compose has accepted it. Every
//! write is staged in a hidden directory made inside the app folder it
//! goes to, validated there with `docker compose config`, then moved into
//! place with one rename on the same filesystem:
//!
//! - A new app is staged as `<folder>/.alchemy-tmp-<name>-XXXX/<name>`, so
//!   Compose names the project as it will once in place, and renamed to
//!   `<folder>/<name>` only if nothing is there yet. An existing folder is
//!   never replaced, since it may hold data.
//! - An edit stages the one file being edited and validates it against
//!   the app's own folder, so relative paths, `env_file:` entries, and
//!   override files resolve as they will. It is renamed over the live file
//!   only if that file still has the hash the edit started from, checked
//!   before staging and again after Compose has run. No other file in the
//!   folder is touched, and a single rename cannot leave a mix of old and
//!   new to roll back.
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
    ENV_FILE, EntryKind, FileContentError, MAX_FILE_LEN, NEW_COMPOSE_FILE, OVERRIDE_FILES,
    StackDirs, check_content, create_root, scrub, staging_prefix,
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
            Self::Invalid(_) => ErrorCode::Invalid,
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
    root: PathBuf,
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
    staging: PathBuf,
    staged: PathBuf,
    check: ComposeCheck,
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
        check_content(kind, update.content.as_bytes())
            .map_err(|err| EditError::Invalid(format!("{} {err}", target.file_name)))?;
        let _writing = self.writing.lock().await;
        let update = Arc::new(update);

        let staged = {
            let (disk, target, update) = (self.disk.clone(), target.clone(), update.clone());
            blocking(move || stage_edit(&*disk, &target, &update)).await??
        };
        let result = self.finish_edit(state, &target, &staged, &update).await;
        self.remove_staging(staged.staging).await;
        result?;
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
        self.validate(state, &staged.check, &[(&staged.staging, &target.dir)])
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
                .map_err(EditError::io(format!(
                    "Alchemy couldn't save {} in {}",
                    target.file_name,
                    target.dir.display()
                )))
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

        let staging = {
            let (disk, root, target, name) = (
                self.disk.clone(),
                root.clone(),
                target.clone(),
                name.clone(),
            );
            let dry_run = new.dry_run;
            blocking(move || begin_create(&*disk, &root, is_home, dry_run, &target, &name))
                .await??
        };
        let staged_app = staging.join(name.as_str());
        let result = self
            .finish_create(state, &staging, &staged_app, &root, &target, &new)
            .await;
        self.remove_staging(staging).await;
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
                    EditError::io(format!("Alchemy couldn't create {}", to.display()))(err)
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

    async fn remove_staging(&self, staging: PathBuf) {
        let disk = self.disk.clone();
        let removed = blocking(move || {
            let result = disk.remove_staging(&staging);
            (staging, result)
        })
        .await;
        if let Ok((staging, Err(error))) = removed {
            tracing::warn!(%error, dir = %staging.display(), "removing a staging folder failed");
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
/// is `<root>/<name>` and whose root is still an app folder.
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
        root,
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

/// Check the folder and the base hash, then stage the new content.
fn stage_edit(
    disk: &dyn AppDisk,
    target: &Target,
    update: &AppFileUpdate,
) -> Result<StagedEdit, EditError> {
    check_dir(disk, target)?;
    let live = read_live(disk, target)?;
    check_base(target, live.as_ref(), update.base_hash.as_deref())?;
    let name = StackName::new(target.name.as_str())
        .map_err(|_| EditError::AppNotFound(target.name.clone()))?;
    let staging = disk
        .make_staging(&target.root, &staging_prefix(&name))
        .map_err(EditError::io(format!(
            "Alchemy couldn't make a temporary folder in {}",
            target.root.display()
        )))?;
    let staged = staging.join(&target.file_name);
    let mode = live.as_ref().map_or(target.kind.new_mode(), |file| {
        target.kind.replacement_mode(file.mode)
    });
    let checked = disk
        .write_new(&staged, update.content.as_bytes(), mode)
        .map_err(EditError::io(format!(
            "Alchemy couldn't write a copy of {}",
            target.file_name
        )))
        .and_then(|()| edit_check(disk, target, &staged));
    match checked {
        Ok(check) => Ok(StagedEdit {
            staging,
            staged,
            check,
        }),
        Err(err) => {
            if let Err(error) = disk.remove_staging(&staging) {
                tracing::warn!(%error, dir = %staging.display(), "removing a staging folder failed");
            }
            Err(err)
        }
    }
}

/// What to validate for an edit: the app's folder with the staged compose
/// file and the app's override, or with the staged `.env`.
fn edit_check(
    disk: &dyn AppDisk,
    target: &Target,
    staged: &Path,
) -> Result<ComposeCheck, EditError> {
    let invalid = |e: crate::compose::ComposeProjectError| {
        EditError::Invalid(format!("Alchemy can't use this folder: {e}"))
    };
    let check = ComposeCheck::new(ComposeProject::new(&target.dir).map_err(invalid)?);
    match target.kind {
        AppFileKind::Env => check.with_env_file(staged).map_err(invalid),
        AppFileKind::Compose => {
            let mut check = check.with_file(staged).map_err(invalid)?;
            if let Some(over) = OVERRIDE_FILES
                .iter()
                .map(|name| target.dir.join(name))
                .find(|path| matches!(disk.kind(path), Ok(Some(EntryKind::File))))
            {
                check = check.with_file(over).map_err(invalid)?;
            }
            Ok(check)
        }
    }
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
fn begin_create(
    disk: &dyn AppDisk,
    root: &Path,
    is_home: bool,
    dry_run: bool,
    target: &Path,
    name: &StackName,
) -> Result<PathBuf, EditError> {
    let look = |path: &Path| {
        disk.kind(path).map_err(EditError::io(format!(
            "Alchemy couldn't look at {}",
            path.display()
        )))
    };
    let mut staging_root = root.to_owned();
    match look(root)? {
        Some(EntryKind::Dir | EntryKind::Link | EntryKind::LinkToDir) => {}
        None if is_home && dry_run => staging_root = std::env::temp_dir(),
        None if is_home => disk.create_dirs(root).map_err(EditError::io(format!(
            "Alchemy couldn't create {}",
            root.display()
        )))?,
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
    if look(target)?.is_some() {
        return Err(exists(target));
    }
    disk.make_staging(&staging_root, &staging_prefix(name))
        .map_err(EditError::io(format!(
            "Alchemy couldn't make a temporary folder in {}",
            staging_root.display()
        )))
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
        assert_ne!(check.files()[0].parent().unwrap(), f.blog);
        assert_eq!(check.files()[1], f.blog.join("compose.override.yaml"));
        assert_eq!(check.env_file(), None);
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

    /// [`SystemDisk`], except that replacing a file always fails.
    #[derive(Debug)]
    struct NoReplace;

    impl AppDisk for NoReplace {
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
        fn rename_new(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(18))
        }
        fn rename_over(&self, _from: &Path, _to: &Path) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(18))
        }
        fn remove_staging(&self, dir: &Path) -> io::Result<()> {
            SystemDisk.remove_staging(dir)
        }
    }

    #[tokio::test]
    async fn a_failed_rename_changes_nothing_and_cleans_up() {
        let f = Fixture::new();
        let editor = f.editor_on(Arc::new(NoReplace));
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
        assert!(matches!(err, EditError::Io { .. }), "{err:?}");
        assert!(
            err.to_string()
                .starts_with("Alchemy couldn't save compose.yaml in")
        );
        assert_eq!(
            fs::read_to_string(f.blog.join("compose.yaml")).unwrap(),
            COMPOSE
        );

        let err = editor
            .create(&f.state, new("shop", None))
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::Io { .. }), "{err:?}");
        assert_eq!(entries(&f.home), ["blog"]);
    }
}
