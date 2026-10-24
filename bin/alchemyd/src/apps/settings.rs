//! The app folder settings: how they are stored and what they mean.

use std::path::{Path, PathBuf};

use alchemy_api::apps::{AppSettings, AppSettingsUpdate};
use alchemy_core::stack::{StackDirs, StackDirsError};
use serde::{Deserialize, Serialize};

/// The [`Store`](crate::store::Store) key holding [`StoredDirs`].
pub const SETTINGS_KEY: &str = "apps.dirs";

/// The app folders as stored. Unset means the defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredDirs {
    /// The chosen home, or `None` for the default.
    #[serde(default)]
    pub home: Option<PathBuf>,
    /// More folders to search after the home.
    #[serde(default)]
    pub extra: Vec<PathBuf>,
}

/// Why an [`AppSettingsUpdate`] was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SettingsError {
    /// No home was given and there is no default to fall back on.
    #[error("choose a home folder for your apps; alchemyd has no home directory to default to")]
    NoHome,
    /// A folder was unusable.
    #[error(transparent)]
    Dirs(#[from] StackDirsError),
}

/// Validate `update` into what to store.
///
/// A home equal to `default_home` is stored as the default, so it keeps
/// following the default.
pub fn validate(
    update: &AppSettingsUpdate,
    default_home: Option<&Path>,
) -> Result<StoredDirs, SettingsError> {
    let chosen = update.home.as_deref().map(PathBuf::from);
    let home = chosen
        .clone()
        .or_else(|| default_home.map(Path::to_owned))
        .ok_or(SettingsError::NoHome)?;
    let dirs = StackDirs::new(home, update.extra.iter().map(PathBuf::from))?;
    let default = default_home.and_then(|d| StackDirs::new(d, []).ok());
    let is_default = default.is_some_and(|d| d.home() == dirs.home());
    Ok(StoredDirs {
        home: (chosen.is_some() && !is_default).then(|| dirs.home().to_owned()),
        extra: dirs.extra().to_vec(),
    })
}

/// The folders `stored` means, with `default_home` standing in for an unset
/// home, and the settings to report for them.
///
/// Stored folders that no longer validate, which only a hand-edited
/// database can produce, are dropped rather than failing the whole list.
pub fn effective(
    stored: &StoredDirs,
    default_home: Option<&Path>,
) -> (Option<StackDirs>, AppSettings) {
    let home = stored
        .home
        .clone()
        .or_else(|| default_home.map(Path::to_owned));
    let dirs = home.and_then(|home| {
        StackDirs::new(&home, stored.extra.iter().cloned())
            .or_else(|_| StackDirs::new(home, []))
            .ok()
    });
    let settings = AppSettings {
        home: dirs.as_ref().map(|d| display(d.home())),
        default_home: default_home.map(display),
        extra: dirs
            .as_ref()
            .map(|d| d.extra().iter().map(|p| display(p)).collect())
            .unwrap_or_default(),
    };
    (dirs, settings)
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
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

    const DEFAULT: &str = "/home/me/Alchemy/apps";

    fn update(home: Option<&str>, extra: &[&str]) -> AppSettingsUpdate {
        AppSettingsUpdate {
            home: home.map(str::to_owned),
            extra: extra.iter().map(|e| (*e).to_owned()).collect(),
        }
    }

    fn stored(home: Option<&str>, extra: &[&str]) -> StoredDirs {
        StoredDirs {
            home: home.map(PathBuf::from),
            extra: extra.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn validates_updates_into_stored_dirs() {
        let cases = [
            (update(None, &[]), Some(DEFAULT), Ok(stored(None, &[]))),
            (
                update(None, &["/opt/stacks/"]),
                Some(DEFAULT),
                Ok(stored(None, &["/opt/stacks"])),
            ),
            (
                update(Some("/home/me/Alchemy/apps/"), &[]),
                Some(DEFAULT),
                Ok(stored(None, &[])),
            ),
            (
                update(Some("/srv/apps"), &["/opt/stacks"]),
                Some(DEFAULT),
                Ok(stored(Some("/srv/apps"), &["/opt/stacks"])),
            ),
            (
                update(Some("/srv/apps"), &[]),
                None,
                Ok(stored(Some("/srv/apps"), &[])),
            ),
            (update(None, &[]), None, Err(SettingsError::NoHome)),
            (
                update(Some("apps"), &[]),
                Some(DEFAULT),
                Err(StackDirsError::NotAbsolute("apps".into()).into()),
            ),
            (
                update(None, &[DEFAULT]),
                Some(DEFAULT),
                Err(StackDirsError::Duplicate(DEFAULT.into()).into()),
            ),
        ];
        for (update, default, want) in cases {
            assert_eq!(
                validate(&update, default.map(Path::new)),
                want,
                "{update:?} {default:?}"
            );
        }
    }

    #[test]
    fn effective_settings_fill_in_the_default() {
        let (dirs, settings) = effective(&stored(None, &["/opt/stacks"]), Some(Path::new(DEFAULT)));
        let dirs = dirs.unwrap();
        assert_eq!(dirs.home(), Path::new(DEFAULT));
        assert_eq!(
            settings,
            AppSettings {
                home: Some(DEFAULT.into()),
                default_home: Some(DEFAULT.into()),
                extra: vec!["/opt/stacks".into()],
            }
        );

        let (dirs, settings) = effective(&stored(Some("/srv/apps"), &[]), Some(Path::new(DEFAULT)));
        assert_eq!(dirs.unwrap().home(), Path::new("/srv/apps"));
        assert_eq!(settings.home.as_deref(), Some("/srv/apps"));
        assert_eq!(settings.default_home.as_deref(), Some(DEFAULT));
    }

    #[test]
    fn effective_settings_without_any_home_have_no_dirs() {
        let (dirs, settings) = effective(&stored(None, &["/opt/stacks"]), None);
        assert_eq!(dirs, None);
        assert_eq!(
            settings,
            AppSettings {
                home: None,
                default_home: None,
                extra: vec![],
            }
        );
    }

    #[test]
    fn effective_settings_drop_extra_dirs_that_no_longer_validate() {
        let (dirs, settings) = effective(&stored(None, &["relative"]), Some(Path::new(DEFAULT)));
        assert_eq!(dirs.unwrap().extra(), [] as [PathBuf; 0]);
        assert_eq!(settings.extra, [] as [String; 0]);
    }
}
