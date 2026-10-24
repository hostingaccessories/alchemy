//! Where the state database lives.
//!
//! The state directory itself is resolved in `alchemy-api`, so the
//! desktop shell finds the TCP token in the same place.

pub use alchemy_api::{STATE_DIR_ENV, default_state_dir};

/// File name of the SQLite database inside the state directory.
pub const DB_FILE: &str = "alchemy.db";
