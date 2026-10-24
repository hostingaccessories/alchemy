//! The port registry: every host port Alchemy reserved, an app declares,
//! or something holds right now, and who holds it.
//!
//! `GET /api/ports` returns a [`PortList`]. A running container's port is
//! listed once, as [`PortSource::Live`] and held by its app's service,
//! even though Docker's `docker-proxy` or Docker Desktop's
//! `com.docker.backend` also listens on it. A port a stopped app's
//! Compose file publishes is [`PortSource::Declared`]. Any other program
//! listening on the host is a [`PortHolder::Program`], named when the OS
//! says which one.
//!
//! A [`PortConflict`] is a port an app wants that something else holds,
//! as `GET /api/apps/{name}/plan` lists it.

use crate::PublishedPort;
pub use alchemy_core::ports::{PortClaim, PortHolder, PortSource};
use serde::{Deserialize, Serialize};

/// `GET /api/ports`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortList {
    /// Every claimed host port, by port, protocol, address, then source.
    /// An app is named as Alchemy knows it; a Compose project no app
    /// folder holds is named by its project name.
    pub ports: Vec<PortClaim>,
    /// What the daemon could not find out, in sentences, such as when it
    /// cannot read which programs are listening. Empty when the list is
    /// complete.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// A port an app wants that something else holds, as an
/// [`ActionPlan`](crate::operations::ActionPlan) lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortConflict {
    /// The service that wants the port.
    pub service: String,
    /// The port it wants.
    pub wanted: PublishedPort,
    /// What holds it now: only live and reserved claims block a start.
    pub claim: PortClaim,
    /// A plain sentence naming the port and its holder, safe to show.
    pub message: String,
}

impl From<&alchemy_core::ports::PortConflict> for PortConflict {
    fn from(conflict: &alchemy_core::ports::PortConflict) -> Self {
        Self {
            service: conflict.service.clone(),
            wanted: conflict.wanted,
            claim: conflict.claim.clone(),
            message: conflict.to_string(),
        }
    }
}
