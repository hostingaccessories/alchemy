//! How a stack is doing, and where its images come from.

use serde::{Deserialize, Serialize};

/// A stack's coarse status, from its services' containers.
///
/// Stop-versus-crash detail is a separate, later refinement; this only
/// counts what is running.
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

/// The status of a stack whose services are each running or not, given
/// in any order. A stack with no services is stopped.
pub fn stack_status(services: impl IntoIterator<Item = bool>) -> StackStatus {
    let (mut total, mut running) = (0_usize, 0_usize);
    for service in services {
        total += 1;
        running += usize::from(service);
    }
    match (total, running) {
        (_, 0) => StackStatus::Stopped,
        (t, r) if t == r => StackStatus::Running,
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
        let cases: [(&[bool], StackStatus); 6] = [
            (&[], StackStatus::Stopped),
            (&[false], StackStatus::Stopped),
            (&[false, false], StackStatus::Stopped),
            (&[true], StackStatus::Running),
            (&[true, true], StackStatus::Running),
            (&[true, false, true], StackStatus::Partial),
        ];
        for (services, want) in cases {
            assert_eq!(stack_status(services.iter().copied()), want, "{services:?}");
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
