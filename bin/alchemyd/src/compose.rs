//! Docker Compose lifecycle actions, driven through the `docker` CLI.
//!
//! What to run is decided by pure helpers: [`ComposePlan`] turns an action
//! into one or more `docker compose` steps, and [`child_env`] decides the
//! child's whole environment. Running a plan sits behind [`ComposeRunner`],
//! which streams each step's output as [`ComposeEvent`]s and stops on a
//! [`CancelToken`]. [`CliComposeRunner`] runs the `docker` binary;
//! [`FakeComposeRunner`] plays back scripts for tests of the code above it.
//!
//! Reading a project's resolved configuration, for its project name and
//! whether it builds, sits behind [`ComposeConfigReader`] the same way:
//! [`CliComposeConfig`] runs `docker compose config`, and
//! [`FakeComposeConfig`] answers from a table. Validating staged files
//! before they are saved sits behind [`ComposeValidator`], which
//! [`CliComposeConfig`] also implements and [`FakeComposeValidator`] fakes.
//!
//! This lives in the daemon rather than `alchemy-core` because the argv is
//! a detail of the `docker` binary, not of the domain.

mod cli;
mod config;
mod env;
mod fake;
mod lines;
mod plan;
mod progress;
mod project;
mod runner;
mod validate;

pub use cli::CliComposeRunner;
pub use config::{
    CliComposeConfig, ComposeConfig, ComposeConfigReader, ConfigError, ExposedPort,
    FakeComposeConfig, FakeConfig, NetworkConfig, ServiceConfig, config_args, config_error_message,
};
pub use env::{ALLOWED_ENV, ALLOWED_ENV_PREFIXES, FIXED_ENV, child_env, is_allowed};
pub use fake::{FakeComposeRunner, FakeOutcome, FakeScript};
pub use plan::{
    ComposeAction, ComposeCommand, ComposePlan, ComposeStep, ImageSource, UnknownAction,
};
pub use progress::{
    BuildProgress, BuildVertex, ProgressEvent, ResourceProgress, ResourceStatus, VertexLog,
    parse_progress,
};
pub use project::{ComposeProject, ComposeProjectError};
pub use runner::{
    CancelToken, Canceller, ComposeError, ComposeEvent, ComposeRunner, OutputLine, Stream,
    cancellation,
};
pub use validate::{ComposeCheck, ComposeValidator, FakeComposeValidator, check_args};
