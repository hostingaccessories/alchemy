//! `alchemy machine`: the machines Alchemy runs apps on. For now, only
//! this one.

pub mod autostart;
pub mod docker;
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
    /// Show or set whether alchemyd starts on its own: off, at login, or
    /// at boot.
    ///
    /// `login` runs alchemyd as a per-user service when you log in, with
    /// no password. `boot` runs it as a system service when the computer
    /// starts, for a home server nobody logs in to; it needs your
    /// password once, asked the same way as `alchemy machine privileges
    /// grant`. Only one is ever installed. Without a mode, this shows the
    /// current one and where alchemyd logs.
    Autostart(autostart::Args),
    /// Work with Docker on this machine, such as starting Docker
    /// Desktop.
    Docker(docker::Args),
}
