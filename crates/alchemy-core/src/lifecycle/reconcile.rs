//! Reconcile: which apps to start after alchemyd starts, the computer
//! wakes, or Docker comes back.
//!
//! Per #12, an app is meant to be running when Alchemy has no stop intent
//! for it and it was running when last seen before the disruption. Each
//! such app that is down now is started once, as an ordinary operation
//! with the reason, and every other app is skipped with why.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::StackName;
use crate::engine::{Intent, IntentAction};
use crate::stack::StackStatus;

/// What set off a reconcile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileTrigger {
    /// alchemyd started and the engine was ready for the first time.
    DaemonStart,
    /// The computer woke from sleep.
    Wake,
    /// The engine was gone and is ready again.
    EngineReturned,
}

impl ReconcileTrigger {
    /// The wire name, such as `engine_returned`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DaemonStart => "daemon_start",
            Self::Wake => "wake",
            Self::EngineReturned => "engine_returned",
        }
    }

    /// Why an app is started on this trigger, for an operation's reason
    /// or a log line, such as "was running before the computer slept".
    pub const fn start_reason(self) -> &'static str {
        match self {
            Self::DaemonStart => "was running before Alchemy restarted",
            Self::Wake => "was running before the computer slept",
            Self::EngineReturned => "was running before Docker stopped",
        }
    }
}

impl fmt::Display for ReconcileTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an app is not started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The last thing asked of it in Alchemy was a stop or a down.
    StopIntent,
    /// It wasn't running when last seen, or it stopped while Alchemy was
    /// watching, which is a stop by hand or a crash.
    NotRunningBefore,
    /// It is running.
    AlreadyRunning,
    /// Some of its services are running. Starting the rest could undo a
    /// stop made by hand, so it is left alone.
    PartlyRunning,
    /// Alchemy can't tell how it is doing, such as when Compose could not
    /// read it.
    StatusUnknown,
}

impl SkipReason {
    /// Why, for the UI and CLI, such as "you stopped it in Alchemy".
    pub const fn message(self) -> &'static str {
        match self {
            Self::StopIntent => "you stopped it in Alchemy",
            Self::NotRunningBefore => "it wasn't running before",
            Self::AlreadyRunning => "it's already running",
            Self::PartlyRunning => "some of its services are already running",
            Self::StatusUnknown => "Alchemy can't tell whether it's running",
        }
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

/// What to do with one app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Decision {
    /// Start it, with the trigger's
    /// [`start_reason`](ReconcileTrigger::start_reason).
    Start,
    /// Leave it.
    Skip {
        /// Why.
        reason: SkipReason,
    },
}

/// One app's decision.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AppPlan {
    /// The app.
    pub app: StackName,
    /// What to do with it.
    pub decision: Decision,
}

/// What a reconcile does, app by app, in name order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcilePlan {
    /// What set it off.
    pub trigger: ReconcileTrigger,
    /// Every app, each with its decision.
    pub apps: Vec<AppPlan>,
}

impl ReconcilePlan {
    /// The apps to start.
    pub fn to_start(&self) -> impl Iterator<Item = &StackName> {
        self.apps
            .iter()
            .filter(|plan| plan.decision == Decision::Start)
            .map(|plan| &plan.app)
    }

    /// Why they are started, such as "was running before the computer
    /// slept".
    pub const fn start_reason(&self) -> &'static str {
        self.trigger.start_reason()
    }
}

/// Whether an intent means the app should stay down.
const fn keeps_down(action: IntentAction) -> bool {
    matches!(action, IntentAction::Stop | IntentAction::Down)
}

fn decide(
    app: &StackName,
    status: StackStatus,
    intents: &BTreeMap<StackName, Intent>,
    last_seen: &BTreeSet<StackName>,
) -> Decision {
    let skip = |reason| Decision::Skip { reason };
    if intents
        .get(app)
        .is_some_and(|intent| keeps_down(intent.action))
    {
        return skip(SkipReason::StopIntent);
    }
    if !last_seen.contains(app) {
        return skip(SkipReason::NotRunningBefore);
    }
    match status {
        StackStatus::Stopped => Decision::Start,
        StackStatus::Running => skip(SkipReason::AlreadyRunning),
        StackStatus::Partial => skip(SkipReason::PartlyRunning),
        StackStatus::Unknown => skip(SkipReason::StatusUnknown),
    }
}

/// Plan a reconcile for `trigger` over every app and its current status,
/// given Alchemy's last intent for each app and the
/// [`LastSeen`](super::LastSeen) set. Apps only in `last_seen`, such as
/// ones since deleted, are not planned.
pub fn plan_reconcile(
    trigger: ReconcileTrigger,
    apps: impl IntoIterator<Item = (StackName, StackStatus)>,
    intents: &BTreeMap<StackName, Intent>,
    last_seen: &BTreeSet<StackName>,
) -> ReconcilePlan {
    let mut apps: Vec<AppPlan> = apps
        .into_iter()
        .map(|(app, status)| {
            let decision = decide(&app, status, intents, last_seen);
            AppPlan { app, decision }
        })
        .collect();
    apps.sort_by(|a, b| a.app.cmp(&b.app));
    ReconcilePlan { trigger, apps }
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

    fn skip(reason: SkipReason) -> Decision {
        Decision::Skip { reason }
    }

    #[test]
    fn decides_each_app() {
        use IntentAction::{Down, Restart, Start, Stop, Up};
        let cases: [(Option<IntentAction>, bool, StackStatus, Decision); 17] = [
            (None, true, Stopped, Decision::Start),
            (None, true, Running, skip(SkipReason::AlreadyRunning)),
            (None, true, Partial, skip(SkipReason::PartlyRunning)),
            (None, true, Unknown, skip(SkipReason::StatusUnknown)),
            (None, false, Stopped, skip(SkipReason::NotRunningBefore)),
            (None, false, Running, skip(SkipReason::NotRunningBefore)),
            (Some(Up), true, Stopped, Decision::Start),
            (Some(Start), true, Stopped, Decision::Start),
            (Some(Restart), true, Stopped, Decision::Start),
            (Some(Up), false, Stopped, skip(SkipReason::NotRunningBefore)),
            (Some(Stop), true, Stopped, skip(SkipReason::StopIntent)),
            (Some(Down), true, Stopped, skip(SkipReason::StopIntent)),
            (Some(Stop), false, Stopped, skip(SkipReason::StopIntent)),
            (Some(Stop), true, Running, skip(SkipReason::StopIntent)),
            (Some(Down), true, Unknown, skip(SkipReason::StopIntent)),
            (Some(Start), true, Running, skip(SkipReason::AlreadyRunning)),
            (Some(Up), true, Partial, skip(SkipReason::PartlyRunning)),
        ];
        let blog = name("blog");
        for (intent, seen, status, want) in cases {
            let intents: BTreeMap<_, _> = intent
                .map(|action| (blog.clone(), Intent::new(action, 1_000)))
                .into_iter()
                .collect();
            let last_seen: BTreeSet<_> = seen.then(|| blog.clone()).into_iter().collect();
            let plan = plan_reconcile(
                ReconcileTrigger::Wake,
                [(blog.clone(), status)],
                &intents,
                &last_seen,
            );
            assert_eq!(
                plan.apps,
                [AppPlan {
                    app: blog.clone(),
                    decision: want
                }],
                "{intent:?} {seen} {status:?}"
            );
        }
    }

    #[test]
    fn plans_every_app_in_name_order() {
        let intents = BTreeMap::from([(name("shop"), Intent::new(IntentAction::Stop, 5))]);
        let last_seen = BTreeSet::from([name("blog"), name("shop"), name("gone"), name("wiki")]);
        let plan = plan_reconcile(
            ReconcileTrigger::EngineReturned,
            [
                (name("wiki"), Stopped),
                (name("blog"), Stopped),
                (name("shop"), Stopped),
                (name("chat"), Stopped),
            ],
            &intents,
            &last_seen,
        );
        let got: Vec<(&str, Decision)> = plan
            .apps
            .iter()
            .map(|p| (p.app.as_str(), p.decision))
            .collect();
        assert_eq!(
            got,
            [
                ("blog", Decision::Start),
                ("chat", skip(SkipReason::NotRunningBefore)),
                ("shop", skip(SkipReason::StopIntent)),
                ("wiki", Decision::Start),
            ]
        );
        let start: Vec<&str> = plan.to_start().map(StackName::as_str).collect();
        assert_eq!(start, ["blog", "wiki"]);
        assert_eq!(plan.start_reason(), "was running before Docker stopped");
    }

    #[test]
    fn reasons_read_plainly() {
        let cases = [
            (
                ReconcileTrigger::DaemonStart,
                "daemon_start",
                "was running before Alchemy restarted",
            ),
            (
                ReconcileTrigger::Wake,
                "wake",
                "was running before the computer slept",
            ),
            (
                ReconcileTrigger::EngineReturned,
                "engine_returned",
                "was running before Docker stopped",
            ),
        ];
        for (trigger, wire, reason) in cases {
            assert_eq!(trigger.as_str(), wire);
            assert_eq!(trigger.to_string(), wire);
            assert_eq!(trigger.start_reason(), reason);
            let json = format!("\"{wire}\"");
            assert_eq!(serde_json::to_string(&trigger).unwrap(), json);
            assert_eq!(
                serde_json::from_str::<ReconcileTrigger>(&json).unwrap(),
                trigger
            );
        }
    }

    #[test]
    fn skip_reasons_wire_and_text() {
        let cases = [
            (
                SkipReason::StopIntent,
                "stop_intent",
                "you stopped it in Alchemy",
            ),
            (
                SkipReason::NotRunningBefore,
                "not_running_before",
                "it wasn't running before",
            ),
            (
                SkipReason::AlreadyRunning,
                "already_running",
                "it's already running",
            ),
            (
                SkipReason::PartlyRunning,
                "partly_running",
                "some of its services are already running",
            ),
            (
                SkipReason::StatusUnknown,
                "status_unknown",
                "Alchemy can't tell whether it's running",
            ),
        ];
        for (reason, wire, text) in cases {
            assert_eq!(reason.to_string(), text);
            assert_eq!(
                serde_json::to_value(Decision::Skip { reason }).unwrap(),
                serde_json::json!({ "kind": "skip", "reason": wire })
            );
        }
        assert_eq!(
            serde_json::to_value(Decision::Start).unwrap(),
            serde_json::json!({ "kind": "start" })
        );
    }
}
