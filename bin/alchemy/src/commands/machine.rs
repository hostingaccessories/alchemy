//! `alchemy machine`: the machines Alchemy runs apps on. For now, only
//! this one.

pub mod stats;

/// Arguments to `alchemy machine`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// What to do with the machine.
    #[command(subcommand)]
    pub command: Verb,
}

/// The `alchemy machine` verbs.
#[derive(Debug, clap::Subcommand)]
pub enum Verb {
    /// Show this machine's CPU, memory, disk, and network use, and each
    /// app's share.
    ///
    /// Figures are from alchemyd's latest sample, taken every minute. On
    /// macOS, and with Docker Desktop, the machine figures are the host
    /// alchemyd runs on, not the VM Docker runs containers in, and an
    /// app's CPU is a share of that VM.
    Stats(stats::Args),
}
