//! The safety flags that destructive commands share, and what they mean
//! for one invocation.
//!
//! A destructive command flattens [`Safety`] into its `Args`. Commands
//! that change nothing don't, so `--yes` on `status` is a usage error
//! rather than a silent no-op.

/// `--dry-run`, `--force`, and `--yes`, for commands that change or
/// remove something.
///
/// None of them ever deletes data: a command that can delete data takes
/// its own explicit flag for that.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::Args)]
pub struct Safety {
    /// Show what alchemyd would do, without doing it.
    #[arg(long)]
    pub dry_run: bool,

    /// Go ahead even when a safety check would stop the command. Never
    /// deletes data.
    #[arg(long)]
    pub force: bool,

    /// Don't ask for confirmation. Required when not run from a terminal.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// What a destructive command does before changing anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Go ahead without asking.
    Proceed,
    /// Ask on the terminal first.
    Ask,
    /// Stop: confirmation is needed and there is no terminal to ask on.
    Refuse,
    /// Show the daemon's plan and change nothing.
    DryRun,
}

/// Decide how to confirm a destructive command, given its flags and
/// whether there is a terminal to ask on.
///
/// `--dry-run` wins over everything, because it never changes anything.
/// `--force` is not consulted: it overrides the daemon's safety checks,
/// not the confirmation.
pub fn decide(safety: &Safety, is_tty: bool) -> Decision {
    if safety.dry_run {
        Decision::DryRun
    } else if safety.yes {
        Decision::Proceed
    } else if is_tty {
        Decision::Ask
    } else {
        Decision::Refuse
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
    use super::*;

    #[test]
    fn decide_per_flags_and_terminal() {
        use Decision::{Ask, DryRun, Proceed, Refuse};
        let cases = [
            (false, false, false, true, Ask),
            (false, false, false, false, Refuse),
            (false, false, true, true, Proceed),
            (false, false, true, false, Proceed),
            (false, true, false, true, Ask),
            (false, true, false, false, Refuse),
            (false, true, true, true, Proceed),
            (false, true, true, false, Proceed),
            (true, false, false, true, DryRun),
            (true, false, false, false, DryRun),
            (true, false, true, true, DryRun),
            (true, false, true, false, DryRun),
            (true, true, false, true, DryRun),
            (true, true, false, false, DryRun),
            (true, true, true, true, DryRun),
            (true, true, true, false, DryRun),
        ];
        for (dry_run, force, yes, is_tty, want) in cases {
            let safety = Safety {
                dry_run,
                force,
                yes,
            };
            assert_eq!(decide(&safety, is_tty), want, "{safety:?}, tty: {is_tty}");
        }
    }

    #[derive(Debug, clap::Parser)]
    struct Destructive {
        #[command(flatten)]
        safety: Safety,
    }

    #[test]
    fn flags_parse_when_flattened() {
        use clap::Parser;
        let cases: [(&[&str], Safety); 5] = [
            (&[], Safety::default()),
            (
                &["--dry-run"],
                Safety {
                    dry_run: true,
                    ..Safety::default()
                },
            ),
            (
                &["--force"],
                Safety {
                    force: true,
                    ..Safety::default()
                },
            ),
            (
                &["-y"],
                Safety {
                    yes: true,
                    ..Safety::default()
                },
            ),
            (
                &["--yes", "--force", "--dry-run"],
                Safety {
                    dry_run: true,
                    force: true,
                    yes: true,
                },
            ),
        ];
        for (args, want) in cases {
            let argv = std::iter::once("destroy").chain(args.iter().copied());
            assert_eq!(
                Destructive::try_parse_from(argv).unwrap().safety,
                want,
                "{args:?}"
            );
        }
    }
}
