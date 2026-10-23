//! One module per subcommand.
//!
//! Each module has a clap `Args` struct, a `run` that does the command's
//! work, and pure functions that render its output.

#[cfg(feature = "test-command")]
pub mod safety_probe;
pub mod status;
