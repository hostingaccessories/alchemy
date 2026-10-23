//! The safety flags that destructive commands share, and what they mean
//! for one invocation.
//!
//! A destructive command flattens [`Safety`] into its `Args`. Commands
//! that change nothing don't, so `--yes` on `status` is a usage error
//! rather than a silent no-op.

use crate::error::ClientError;
use crate::prompt::Prompt;

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

/// What a confirmed destructive command goes on to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Ask the daemon for its plan, render it, and change nothing.
    DryRun,
    /// Make the change.
    Execute,
}

/// Confirm `command` according to `safety`, asking `question` on `prompt`
/// when there is a terminal and `--yes` wasn't given.
///
/// Returns an error, exit code 3, when the answer is no or when there is
/// no terminal to ask on. It never reads stdin without a terminal.
pub async fn confirm<P: Prompt>(
    prompt: &mut P,
    safety: &Safety,
    command: &'static str,
    question: &str,
) -> Result<Mode, ClientError> {
    match decide(safety, prompt.is_interactive()) {
        Decision::DryRun => Ok(Mode::DryRun),
        Decision::Proceed => Ok(Mode::Execute),
        Decision::Refuse => Err(ClientError::ConfirmationRequired { command }),
        Decision::Ask => {
            if prompt.confirm(question).await {
                Ok(Mode::Execute)
            } else {
                Err(ClientError::Declined { command })
            }
        }
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
    use crate::prompt::FakePrompt;

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

    #[tokio::test]
    async fn confirm_asks_only_when_it_must() {
        use Mode::{DryRun, Execute};
        let yes = Safety {
            yes: true,
            ..Safety::default()
        };
        let dry_run = Safety {
            dry_run: true,
            ..Safety::default()
        };
        let force = Safety {
            force: true,
            ..Safety::default()
        };
        let none = Safety::default();
        let cases: [(Safety, FakePrompt, Result<Mode, u8>, usize); 7] = [
            (none, FakePrompt::tty([true]), Ok(Execute), 1),
            (none, FakePrompt::tty([false]), Err(3), 1),
            (force, FakePrompt::tty([false]), Err(3), 1),
            (none, FakePrompt::no_tty(), Err(3), 0),
            (force, FakePrompt::no_tty(), Err(3), 0),
            (yes, FakePrompt::no_tty(), Ok(Execute), 0),
            (dry_run, FakePrompt::tty([true]), Ok(DryRun), 0),
        ];
        for (safety, mut prompt, want, asked) in cases {
            let got = confirm(&mut prompt, &safety, "app rm", "Remove app web?").await;
            assert_eq!(got.map_err(|e| e.exit_code()), want, "{safety:?}");
            assert_eq!(prompt.asked().len(), asked, "{safety:?}");
        }
    }

    #[tokio::test]
    async fn refusals_name_the_command_and_the_way_out() {
        let mut prompt = FakePrompt::no_tty();
        let err = confirm(&mut prompt, &Safety::default(), "app rm", "Remove?")
            .await
            .unwrap_err();
        assert_eq!(
            err.to_text(),
            "alchemy: `app rm` needs confirmation, but there is no terminal to ask on\n\
             hint: pass --yes to go ahead without a prompt\n"
        );

        let mut prompt = FakePrompt::tty([false]);
        let err = confirm(&mut prompt, &Safety::default(), "app rm", "Remove?")
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "`app rm` cancelled: not confirmed");
    }
}
