//! `alchemy machine`: the machines Alchemy runs apps on. For now, only
//! this one.

pub mod ports;
pub mod privileges;
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
    /// List every host port in use, reserved by Alchemy, or declared by
    /// an app, and who holds it.
    ///
    /// A port held by a program other than an app shows its process name
    /// and pid when this machine reveals them. alchemyd can name only its
    /// own user's processes, so a port a root process holds shows without
    /// a name, and on some Macs isn't listed.
    Ports(ports::Args),
    /// List what needs root on this machine, such as port 80 on Linux or
    /// the host firewall, or grant or revoke one.
    ///
    /// Each action shows whether it is needed, granted, or can't be done
    /// here, and why. Granting goes through alchemyd, which shows one
    /// system password prompt; where it can't, as over SSH, this runs the
    /// `sudo` command alchemyd gives in the terminal instead.
    Privileges(privileges::Args),
}
