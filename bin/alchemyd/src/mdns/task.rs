//! [`run`]: the task that keeps one announcer in step with what routing
//! wants announced.

use std::time::Duration;

use alchemy_core::route::{HostLabel, HostSuffix};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use super::plan::{self, Published, Step, Wanted};
use super::{AnnounceError, RecordState, RecordStates, Responder, SharedAnnouncer};
use crate::State;
use crate::routing::MdnsStatus;

/// How long shutdown waits for the announcer to withdraw every name.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Announce what `wanted` asks for through the announcer `responder`
/// gives, and keep [`State::mdns`] current, until `shutdown` flips to
/// `true` or its sender is dropped.
///
/// It catches up whenever routing wants something else or the responder
/// reports on a name, and every `interval`, which retries calls that
/// failed. When [`Wanted::reannounce`] changes, after a wake, every name
/// is withdrawn and announced again. A responder that can't start leaves `.local` names
/// unavailable and the rest of the daemon running. On shutdown every name
/// is withdrawn, for at most [`SHUTDOWN_GRACE`].
pub async fn run(
    responder: Responder,
    mut wanted: watch::Receiver<Option<Wanted>>,
    state: State,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let started = tokio::select! {
        biased;
        _ = shutdown.wait_for(|stop| *stop) => return,
        started = responder.start() => started,
    };
    let announcer = match started {
        Ok(announcer) => announcer,
        Err(error) => {
            tracing::warn!(
                %error,
                "cannot start announcing .local names; apps keep their IP:port addresses"
            );
            let backend = match &error {
                AnnounceError::Responder { backend, .. } => Some(*backend),
                AnnounceError::Closed => None,
            };
            state.set_mdns(MdnsStatus {
                report: plan::start_failed(backend, &error.to_string()),
                ..MdnsStatus::default()
            });
            let _ = shutdown.wait_for(|stop| *stop).await;
            return;
        }
    };
    tracing::info!(backend = %announcer.backend(), "announcing .local names");
    let mut states = announcer.states();
    let mut task = Announcing {
        announcer,
        state,
        published: Published::default(),
        failure: None,
        seen: RecordStates::default(),
        reannounced: 0,
    };
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let want = wanted.borrow_and_update().clone();
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => break,
            () = task.sync(want.as_ref()) => {}
        }
        let now = states.borrow_and_update().clone();
        task.publish(want.as_ref(), &now);
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => break,
            Ok(()) = wanted.changed() => {}
            Ok(()) = states.changed() => {}
            _ = tick.tick() => {}
        }
    }
    task.stop().await;
}

struct Announcing {
    announcer: SharedAnnouncer,
    state: State,
    published: Published,
    /// Why the last call failed, until a round of calls all succeed.
    failure: Option<String>,
    /// The record states last logged.
    seen: RecordStates,
    /// The [`Wanted::reannounce`] last acted on.
    reannounced: u64,
}

impl Announcing {
    /// Make the calls that bring the announcer to `wanted`, recording
    /// each that succeeds.
    async fn sync(&mut self, wanted: Option<&Wanted>) {
        let Some(wanted) = wanted else {
            return;
        };
        let mut failure = None;
        let steps = if wanted.reannounce == self.reannounced {
            plan::steps(&self.published, wanted)
        } else {
            self.reannounced = wanted.reannounce;
            if !self.published.names.is_empty() {
                tracing::info!(
                    count = self.published.names.len(),
                    "announcing .local names again after a wake"
                );
            }
            plan::reannounce(&self.published, wanted)
        };
        for step in steps {
            let result = match &step {
                Step::Announce(announcement, port) => {
                    self.announcer.announce(announcement.clone(), *port).await
                }
                Step::Withdraw(label) => self.announcer.withdraw(label.clone()).await,
            };
            match result {
                Ok(()) => {
                    log_step(&step);
                    self.published.apply(&step);
                }
                Err(error) => {
                    let why = error.to_string();
                    if self.failure.as_ref() == Some(&why) {
                        tracing::debug!(%error, "cannot announce a .local name");
                    } else {
                        tracing::warn!(%error, "cannot announce a .local name; retrying");
                    }
                    failure = Some(why);
                }
            }
        }
        self.failure = failure;
    }

    /// Tell [`State`] which names are on the network, which another
    /// device answers for, and how the announcer is doing.
    fn publish(&mut self, wanted: Option<&Wanted>, states: &RecordStates) {
        for (label, state) in plan::changed_states(&self.seen, states) {
            log_state(label, state);
        }
        self.seen = states.clone();
        self.state.set_mdns(MdnsStatus {
            announced: plan::established(&self.published, states),
            conflicts: states.conflicts().cloned().collect(),
            report: plan::report(
                self.announcer.backend(),
                wanted,
                states,
                self.failure.as_deref(),
            ),
        });
    }

    /// Withdraw every name and stop the announcer, giving up after
    /// [`SHUTDOWN_GRACE`].
    async fn stop(self) {
        let count = self.published.names.len();
        if tokio::time::timeout(SHUTDOWN_GRACE, self.announcer.shutdown())
            .await
            .is_ok()
        {
            tracing::info!(count, "withdrew .local names");
        } else {
            tracing::warn!(
                count,
                "timed out withdrawing .local names; other devices forget them within a few minutes"
            );
        }
    }
}

fn log_step(step: &Step) {
    match step {
        Step::Announce(announcement, port) => tracing::info!(
            name = %announcement.name(),
            addr = %announcement.lan.addr(),
            interface = announcement.lan.interface(),
            port,
            "announcing .local name"
        ),
        Step::Withdraw(label) => {
            tracing::info!(name = %label.host(HostSuffix::Local), "withdrew .local name");
        }
    }
}

fn log_state(label: &HostLabel, state: &RecordState) {
    let name = label.host(HostSuffix::Local);
    match state {
        RecordState::Registering => tracing::debug!(name, "registering .local name"),
        RecordState::Established => tracing::info!(name, ".local name is on the network"),
        RecordState::Conflict => {
            tracing::warn!(name, "another device on the network uses this .local name");
        }
        RecordState::Denied => tracing::warn!(
            name,
            "the OS denied announcing .local names; allow Alchemy in Local Network settings"
        ),
        RecordState::Failed(why) => tracing::warn!(name, why, ".local name failed; retrying"),
    }
}
