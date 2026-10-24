//! How a stack is doing, and where its images come from.

use serde::{Deserialize, Serialize};

use crate::engine::{ContainerState, StopReason};

/// A stack's coarse status, from its services' containers.
///
/// This only counts what is running. Why the rest is not, such as a crash
/// or a stop by hand, is the stack's
/// [`StopReason`](crate::engine::StopReason), from
/// [`summarize`](crate::engine::summarize).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StackStatus {
    /// Every service has a running container.
    Running,
    /// Some services have a running container and some do not.
    Partial,
    /// No service has a running container.
    Stopped,
    /// Alchemy cannot tell, such as while Docker is not running or
    /// Compose could not read the stack. Also what a status this build
    /// does not know decodes as.
    #[serde(other)]
    Unknown,
}

/// How one service of a stack is doing, for [`stack_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceRun {
    /// A container of it is running.
    Running,
    /// It ran to completion with exit code 0, as a one-off job such as a
    /// migration does, so it is not missing.
    Finished,
    /// Nothing of it is running.
    Down,
}

impl ServiceRun {
    /// How a service whose containers have these states and reasons is
    /// doing: running if any is, finished if any finished, and down
    /// otherwise.
    pub fn of(containers: impl IntoIterator<Item = (ContainerState, StopReason)>) -> Self {
        containers
            .into_iter()
            .fold(Self::Down, |run, (state, reason)| {
                match (run, state, reason) {
                    (Self::Running, _, _) | (_, ContainerState::Running, _) => Self::Running,
                    (_, _, StopReason::Finished) => Self::Finished,
                    (run, _, _) => run,
                }
            })
    }
}

/// The status of a stack from its services, given in any order. A stack
/// with no services, or none running, is stopped. A finished one-off job
/// does not make a running stack partial.
pub fn stack_status(services: impl IntoIterator<Item = ServiceRun>) -> StackStatus {
    let (mut total, mut running, mut finished) = (0_usize, 0_usize, 0_usize);
    for service in services {
        total += 1;
        match service {
            ServiceRun::Running => running += 1,
            ServiceRun::Finished => finished += 1,
            ServiceRun::Down => {}
        }
    }
    match (total, running) {
        (_, 0) => StackStatus::Stopped,
        (t, r) if t == r + finished => StackStatus::Running,
        _ => StackStatus::Partial,
    }
}

/// Where a stack's images come from, which decides how an update refreshes
/// them.
///
/// Whoever reads the resolved compose file decides this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSource {
    /// Every service names an image to pull.
    #[default]
    Registry,
    /// At least one service has a `build:` section.
    Build,
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
    fn counts_running_services() {
        use ServiceRun::{Down, Finished, Running};
        let cases: [(&[ServiceRun], StackStatus); 10] = [
            (&[], StackStatus::Stopped),
            (&[Down], StackStatus::Stopped),
            (&[Down, Down], StackStatus::Stopped),
            (&[Running], StackStatus::Running),
            (&[Running, Running], StackStatus::Running),
            (&[Running, Down, Running], StackStatus::Partial),
            (&[Running, Finished], StackStatus::Running),
            (&[Running, Finished, Down], StackStatus::Partial),
            (&[Finished], StackStatus::Stopped),
            (&[Finished, Down], StackStatus::Stopped),
        ];
        for (services, want) in cases {
            assert_eq!(stack_status(services.iter().copied()), want, "{services:?}");
        }
    }

    #[test]
    fn service_run_of_its_containers() {
        let running = (ContainerState::Running, StopReason::Running);
        let finished = (ContainerState::Exited, StopReason::Finished);
        let crashed = (
            ContainerState::Exited,
            StopReason::Crashed {
                exit_code: Some(1),
                oom: false,
            },
        );
        let cases: [(&[(ContainerState, StopReason)], ServiceRun); 6] = [
            (&[], ServiceRun::Down),
            (&[crashed], ServiceRun::Down),
            (&[finished], ServiceRun::Finished),
            (&[finished, crashed], ServiceRun::Finished),
            (&[crashed, running], ServiceRun::Running),
            (&[running, finished], ServiceRun::Running),
        ];
        for (containers, want) in cases {
            assert_eq!(
                ServiceRun::of(containers.iter().copied()),
                want,
                "{containers:?}"
            );
        }
    }

    #[test]
    fn status_wire_names_and_unknown_fallback() {
        for (status, json) in [
            (StackStatus::Running, "\"running\""),
            (StackStatus::Partial, "\"partial\""),
            (StackStatus::Stopped, "\"stopped\""),
            (StackStatus::Unknown, "\"unknown\""),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), json);
            assert_eq!(serde_json::from_str::<StackStatus>(json).unwrap(), status);
        }
        assert_eq!(
            serde_json::from_str::<StackStatus>("\"crashed\"").unwrap(),
            StackStatus::Unknown
        );
    }
}
