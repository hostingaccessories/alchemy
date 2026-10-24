//! Docker Compose lifecycle actions, driven through the `docker` CLI.
//!
//! What to run is decided by pure helpers: [`ComposePlan`] turns an action
//! into one or more `docker compose` steps, and [`child_env`] decides the
//! child's whole environment. Running a plan sits behind [`ComposeRunner`],
//! implemented by [`CliComposeRunner`].
//!
//! This lives in the daemon rather than `alchemy-core` because the argv is
//! a detail of the `docker` binary, not of the domain.

mod env;
mod plan;
mod progress;
mod project;
mod runner;

pub use env::{ALLOWED_ENV, ALLOWED_ENV_PREFIXES, FIXED_ENV, child_env, is_allowed};
pub use plan::{
    ComposeAction, ComposeCommand, ComposePlan, ComposeStep, ImageSource, UnknownAction,
};
pub use progress::{
    BuildProgress, BuildVertex, ProgressEvent, ResourceProgress, ResourceStatus, parse_progress,
};
pub use project::{ComposeProject, ComposeProjectError};
pub use runner::{CliComposeRunner, ComposeError, ComposeRunner};
