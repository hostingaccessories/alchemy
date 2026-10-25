//! The last-seen-running set: which apps were running when Alchemy last
//! saw them, before a disruption.
//!
//! Per #12, an app that stops while Docker is up and Alchemy is watching
//! was stopped by hand, or crashed, and leaves the set; one that stops
//! because Docker went away, alchemyd is stopping, or the computer slept
//! stays in it, so a reconcile brings it back.
//!
//! "Watching" means Alchemy saw the app running and then saw it stop. A
//! stack that is already stopped the first time it is seen, as in the
//! listing after alchemyd starts or reconnects, stopped while nobody
//! watched, so it stays. And a stop only counts once Docker has stayed
//! up for [`STOP_GRACE`] after it with no disruption in between, since
//! quitting Docker or shutting down stops containers one by one before
//! the engine goes, and a container dying during sleep can be reported
//! just before the wake is noticed.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::StackName;
use crate::stack::StackStatus;

/// How long Docker must stay up after an app stops, with no disruption,
/// before the stop counts as one made by hand. It covers Docker's own
/// shutdown, which gives each container 10 seconds by default to stop,
/// and the tick it takes [`WakeDetector`](super::WakeDetector) to notice
/// a wake.
pub const STOP_GRACE: Duration = Duration::from_secs(20);

/// Something that stops apps without anyone asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Disruption {
    /// The engine connection ended or a ping failed.
    EngineGone,
    /// alchemyd is shutting down.
    DaemonStopping,
    /// The computer woke from sleep.
    Woke,
}

/// A change to the persisted set.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LastSeenChange {
    /// The app is running, and wasn't in the set.
    Added(StackName),
    /// The app was stopped while watched, or forgotten.
    Removed(StackName),
}

/// The last-seen-running set, and what is needed to keep it.
///
/// Only [`running`](Self::running) is persisted. The rest lasts for one
/// run of alchemyd and starts empty, so nothing counts as watched until
/// it is seen running again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastSeen {
    grace: Duration,
    running: BTreeSet<StackName>,
    watched: BTreeSet<StackName>,
    stopping: BTreeMap<StackName, Duration>,
    quiet_until: Option<Duration>,
}

impl LastSeen {
    /// The set as persisted, with the default [`STOP_GRACE`].
    pub fn new(running: impl IntoIterator<Item = StackName>) -> Self {
        Self::with_grace(running, STOP_GRACE)
    }

    /// The set as persisted, with a stop counting after `grace`.
    pub fn with_grace(running: impl IntoIterator<Item = StackName>, grace: Duration) -> Self {
        Self {
            grace,
            running: running.into_iter().collect(),
            watched: BTreeSet::new(),
            stopping: BTreeMap::new(),
            quiet_until: None,
        }
    }

    /// The apps that were running when last seen, for
    /// [`plan_reconcile`](super::plan_reconcile).
    pub fn running(&self) -> &BTreeSet<StackName> {
        &self.running
    }

    /// Note `app`'s status at `now`, as of a listing or an event, while
    /// the engine is ready. Call it for every app whenever statuses
    /// change.
    ///
    /// A running or partial app joins the set at once. A stopped one that
    /// was seen running since the last disruption leaves it after the
    /// grace, through [`settle`](Self::settle), unless it runs again or a
    /// disruption comes first. An unknown status changes nothing.
    pub fn observe(
        &mut self,
        app: &StackName,
        status: StackStatus,
        now: Duration,
    ) -> Option<LastSeenChange> {
        match status {
            StackStatus::Running | StackStatus::Partial => {
                self.stopping.remove(app);
                self.watched.insert(app.clone());
                self.running
                    .insert(app.clone())
                    .then(|| LastSeenChange::Added(app.clone()))
            }
            StackStatus::Stopped => {
                if self.quiet_until.is_some_and(|until| now < until) {
                    self.watched.remove(app);
                } else if self.watched.contains(app) && self.running.contains(app) {
                    self.stopping
                        .entry(app.clone())
                        .or_insert_with(|| now.saturating_add(self.grace));
                }
                None
            }
            StackStatus::Unknown => None,
        }
    }

    /// Something stopped apps at `now` without anyone asking. Stops not
    /// yet settled are dropped, nothing counts as watched until seen
    /// running again, and stops for the next grace don't count.
    pub fn disrupted(&mut self, _disruption: Disruption, now: Duration) {
        self.watched.clear();
        self.stopping.clear();
        self.quiet_until = Some(now.saturating_add(self.grace));
    }

    /// Remove the apps whose stop has outlasted the grace by `now`.
    pub fn settle(&mut self, now: Duration) -> Vec<LastSeenChange> {
        let due: Vec<StackName> = self
            .stopping
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(app, _)| app.clone())
            .collect();
        due.into_iter()
            .filter_map(|app| {
                self.stopping.remove(&app);
                self.watched.remove(&app);
                self.running
                    .remove(&app)
                    .then_some(LastSeenChange::Removed(app))
            })
            .collect()
    }

    /// When [`settle`](Self::settle) next has something to do, if ever.
    pub fn next_settle(&self) -> Option<Duration> {
        self.stopping.values().min().copied()
    }

    /// Drop `app` entirely, as when it no longer exists.
    pub fn forget(&mut self, app: &StackName) -> Option<LastSeenChange> {
        self.stopping.remove(app);
        self.watched.remove(app);
        self.running
            .remove(app)
            .then(|| LastSeenChange::Removed(app.clone()))
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
    use StackStatus::{Partial, Running, Stopped, Unknown};

    fn name(s: &str) -> StackName {
        StackName::new(s).unwrap()
    }

    const fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[derive(Debug, Clone, Copy)]
    enum Step {
        See(StackStatus, u64),
        Disrupt(Disruption, u64),
        Settle(u64),
    }

    use Step::{Disrupt, See, Settle};

    /// Run `steps` against the app `blog`, starting from the persisted
    /// set `before`, and return whether it is in the set at the end.
    fn run(before: bool, steps: &[Step]) -> bool {
        let blog = name("blog");
        let mut seen = LastSeen::new(before.then(|| blog.clone()));
        for step in steps {
            match *step {
                See(status, at) => {
                    seen.observe(&blog, status, secs(at));
                }
                Disrupt(disruption, at) => seen.disrupted(disruption, secs(at)),
                Settle(at) => {
                    seen.settle(secs(at));
                }
            }
        }
        seen.running().contains(&blog)
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn keeps_or_drops_by_the_rule() {
        let cases: [(&str, bool, &[Step], bool); 17] = [
            ("seen running joins", false, &[See(Running, 0)], true),
            ("partial counts as running", false, &[See(Partial, 0)], true),
            ("unknown changes nothing", true, &[See(Unknown, 0)], true),
            ("never seen running", false, &[See(Stopped, 0)], false),
            (
                "stopped at first sight, after a restart",
                true,
                &[See(Stopped, 0), Settle(100)],
                true,
            ),
            (
                "stopped by hand while watched",
                true,
                &[See(Running, 0), See(Stopped, 5), Settle(25)],
                false,
            ),
            (
                "not yet past the grace",
                true,
                &[See(Running, 0), See(Stopped, 5), Settle(24)],
                true,
            ),
            (
                "repeated stopped status keeps the first deadline",
                true,
                &[
                    See(Running, 0),
                    See(Stopped, 5),
                    See(Stopped, 20),
                    Settle(25),
                ],
                false,
            ),
            (
                "ran again within the grace",
                true,
                &[
                    See(Running, 0),
                    See(Stopped, 5),
                    See(Running, 10),
                    Settle(60),
                ],
                true,
            ),
            (
                "docker quit right after",
                true,
                &[
                    See(Running, 0),
                    See(Stopped, 5),
                    Disrupt(Disruption::EngineGone, 12),
                    Settle(60),
                ],
                true,
            ),
            (
                "alchemyd stopping",
                true,
                &[
                    See(Running, 0),
                    See(Stopped, 5),
                    Disrupt(Disruption::DaemonStopping, 6),
                    Settle(60),
                ],
                true,
            ),
            (
                "died just before the wake was noticed",
                true,
                &[
                    See(Running, 0),
                    See(Stopped, 5),
                    Disrupt(Disruption::Woke, 6),
                    Settle(60),
                ],
                true,
            ),
            (
                "stopped at first sight after reconnecting",
                true,
                &[
                    See(Running, 0),
                    Disrupt(Disruption::EngineGone, 5),
                    See(Stopped, 100),
                    Settle(200),
                ],
                true,
            ),
            (
                "stopped in the quiet after a wake",
                true,
                &[
                    Disrupt(Disruption::Woke, 0),
                    See(Running, 1),
                    See(Stopped, 3),
                    See(Stopped, 30),
                    Settle(100),
                ],
                true,
            ),
            (
                "stopped by hand after the quiet",
                true,
                &[
                    Disrupt(Disruption::Woke, 0),
                    See(Running, 1),
                    See(Stopped, 30),
                    Settle(50),
                ],
                false,
            ),
            (
                "seen running again after reconnecting, then stopped",
                true,
                &[
                    Disrupt(Disruption::EngineGone, 0),
                    See(Running, 100),
                    See(Stopped, 110),
                    Settle(130),
                ],
                false,
            ),
            (
                "a settled stop stays out",
                true,
                &[
                    See(Running, 0),
                    See(Stopped, 5),
                    Settle(25),
                    See(Stopped, 30),
                    Settle(60),
                ],
                false,
            ),
        ];
        for (name, before, steps, want) in cases {
            assert_eq!(run(before, steps), want, "{name}");
        }
    }

    #[test]
    fn reports_changes_to_persist() {
        let (blog, shop) = (name("blog"), name("shop"));
        let mut seen = LastSeen::new([blog.clone()]);
        assert_eq!(seen.observe(&blog, Running, secs(0)), None);
        assert_eq!(
            seen.observe(&shop, Running, secs(0)),
            Some(LastSeenChange::Added(shop.clone()))
        );
        assert_eq!(seen.observe(&shop, Running, secs(1)), None);
        assert_eq!(seen.observe(&blog, Stopped, secs(2)), None);
        assert_eq!(seen.observe(&shop, Stopped, secs(4)), None);
        assert_eq!(seen.next_settle(), Some(secs(22)));
        assert_eq!(seen.settle(secs(21)), Vec::new());
        assert_eq!(
            seen.settle(secs(22)),
            [LastSeenChange::Removed(blog.clone())]
        );
        assert_eq!(seen.next_settle(), Some(secs(24)));
        assert_eq!(
            seen.settle(secs(99)),
            [LastSeenChange::Removed(shop.clone())]
        );
        assert_eq!(seen.next_settle(), None);
        assert!(seen.running().is_empty());
    }

    #[test]
    fn forgets_apps() {
        let blog = name("blog");
        let mut seen = LastSeen::new([blog.clone()]);
        seen.observe(&blog, Running, secs(0));
        seen.observe(&blog, Stopped, secs(1));
        assert_eq!(
            seen.forget(&blog),
            Some(LastSeenChange::Removed(blog.clone()))
        );
        assert_eq!(seen.forget(&blog), None);
        assert_eq!(seen.next_settle(), None);
        assert_eq!(seen.settle(secs(99)), Vec::new());
    }

    #[test]
    fn grace_is_a_parameter() {
        let blog = name("blog");
        let mut seen = LastSeen::with_grace([blog.clone()], secs(2));
        seen.observe(&blog, Running, secs(0));
        seen.observe(&blog, Stopped, secs(1));
        assert_eq!(seen.settle(secs(3)), [LastSeenChange::Removed(blog)]);
    }

    #[test]
    fn only_apps_in_the_set_go_pending() {
        let blog = name("blog");
        let mut seen = LastSeen::new([]);
        seen.observe(&blog, Running, secs(0));
        seen.forget(&blog);
        seen.observe(&blog, Stopped, secs(1));
        assert_eq!(seen.next_settle(), None);
    }
}
