//! Why a container is not running: stopped by Alchemy, stopped by someone
//! else, crashed, crashing over and over, or finished its job.
//!
//! Exit codes alone cannot tell these apart: `docker stop` on an app that
//! exits 0 on `SIGTERM` looks like a finished job, and 137 is both a stop
//! that timed out and an out-of-memory kill. So the model keeps a [`Trail`]
//! of each container's recent events, and Alchemy records an [`Intent`]
//! before it stops an app. [`stop_reason`] decides from both.
//!
//! The engine's events for the cases that matter:
//!
//! | What happened                       | Events                              |
//! | ----------------------------------- | ----------------------------------- |
//! | `docker stop`, `docker compose stop` | `kill` (stop signal), `die`, `stop` |
//! | `docker kill`                       | `kill` (signal 9), `die`            |
//! | Out of memory                       | `oom`, `die`                        |
//! | The process exited, or was killed from the host | `die`                   |
//! | A restart policy restarting it      | `die`, then `start`                 |
//!
//! When the daemon starts or reconnects, the history before it is gone.
//! Containers found stopped then are inspected instead, and
//! [`Trail::from_inspect`] guesses from the exit code: 130 and 143 (`SIGINT`
//! and `SIGTERM`) as stopped outside Alchemy, 0 as finished, and anything
//! else as a crash.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{ContainerAction, ContainerEvent, ContainerState};

/// How many crashes in a row, each after a short run, make a crash loop.
pub const CRASH_LOOP: u32 = 3;

/// A run shorter than this, in milliseconds, that ends in a crash counts
/// toward [`CRASH_LOOP`]. Docker's restart backoff resets after 10 seconds
/// of running, so a minute leaves room for slow starts.
pub const SHORT_RUN_MS: i64 = 60_000;

/// How far apart, in milliseconds, the daemon's clock and the engine's may
/// be when matching an [`Intent`] to an exit. Docker Desktop's VM keeps
/// its own clock.
pub const CLOCK_SLACK_MS: i64 = 5_000;

/// What Alchemy last asked a Compose project to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IntentAction {
    /// Create and start the project's containers.
    Up,
    /// Start its existing containers.
    Start,
    /// Stop and start its containers.
    Restart,
    /// Stop its containers, keeping them.
    Stop,
    /// Stop and remove its containers.
    Down,
}

impl IntentAction {
    /// Every action, for tables and parsing.
    pub const ALL: [Self; 5] = [Self::Up, Self::Start, Self::Restart, Self::Stop, Self::Down];

    /// The action's stored name, such as `stop`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Start => "start",
            Self::Restart => "restart",
            Self::Stop => "stop",
            Self::Down => "down",
        }
    }

    /// The action named `name`, as [`Self::as_str`] spells it.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.as_str() == name)
    }

    /// Whether the action stops the containers that were running when it
    /// began. `up` and `start` leave running containers alone, so a crash
    /// after either is still a crash.
    pub const fn stops(self) -> bool {
        matches!(self, Self::Restart | Self::Stop | Self::Down)
    }
}

/// An action Alchemy asked of a Compose project, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Intent {
    /// What was asked.
    pub action: IntentAction,
    /// When, in Unix milliseconds, recorded before the action began.
    pub at: i64,
}

impl Intent {
    /// `action`, asked at `at` Unix milliseconds.
    pub const fn new(action: IntentAction, at: i64) -> Self {
        Self { action, at }
    }
}

/// Why a container is or is not running, as the API reports it.
///
/// Serialized with a `kind` tag, such as
/// `{"kind": "crashed", "exit_code": 1, "oom": false}`. A kind this build
/// does not know decodes as [`StopReason::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    /// Running, or restarting on request.
    Running,
    /// Paused, with its processes frozen.
    Paused,
    /// Created but never started, or, for an app, no containers at all.
    NotStarted,
    /// Stopped because Alchemy stopped, restarted, or took down its app.
    StoppedByAlchemy,
    /// Stopped by someone else through Docker, such as `docker stop` or
    /// `docker compose stop` run by hand.
    StoppedOutside,
    /// Exited on its own with an error, ran out of memory, or had its
    /// process killed from outside Docker.
    Crashed {
        /// The exit code, if known.
        exit_code: Option<i64>,
        /// Whether it ran out of memory.
        oom: bool,
    },
    /// Crashed several times in a row soon after starting, and its restart
    /// policy keeps starting it again.
    CrashLooping {
        /// The last exit code, if known.
        exit_code: Option<i64>,
        /// Whether the last crash ran out of memory.
        oom: bool,
    },
    /// Exited with code 0 without being asked to, as a one-off job does.
    Finished,
    /// Stopped for a reason Alchemy cannot tell, or a kind this build does
    /// not know.
    #[default]
    #[serde(other)]
    Unknown,
}

impl StopReason {
    /// Whether this is a problem someone should look at.
    pub const fn is_problem(self) -> bool {
        matches!(self, Self::Crashed { .. } | Self::CrashLooping { .. })
    }

    /// Rank for [`summarize`]: higher is shown first.
    const fn severity(self) -> u8 {
        match self {
            Self::Finished => 0,
            Self::Running => 1,
            Self::Paused => 2,
            Self::NotStarted => 3,
            Self::StoppedByAlchemy => 4,
            Self::Unknown => 5,
            Self::StoppedOutside => 6,
            Self::Crashed { .. } => 7,
            Self::CrashLooping { .. } => 8,
        }
    }
}

/// Plain words for people, problem first, such as `Crashed (out of
/// memory)` or `Keeps crashing`.
impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (text, exit_code, oom) = match *self {
            Self::Running => ("Running", None, false),
            Self::Paused => ("Paused", None, false),
            Self::NotStarted => ("Not running", None, false),
            Self::StoppedByAlchemy | Self::Unknown => ("Stopped", None, false),
            Self::StoppedOutside => ("Stopped outside Alchemy", None, false),
            Self::Finished => ("Finished", None, false),
            Self::Crashed { exit_code, oom } => ("Crashed", exit_code, oom),
            Self::CrashLooping { exit_code, oom } => ("Keeps crashing", exit_code, oom),
        };
        f.write_str(text)?;
        match (oom, exit_code) {
            (true, _) => f.write_str(" (out of memory)"),
            (false, Some(code)) => write!(f, " (exit code {code})"),
            (false, None) => Ok(()),
        }
    }
}

/// The last time a container exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct LastExit {
    code: Option<i64>,
    oom: bool,
    /// Someone asked Docker to stop it: a `kill` with a stop signal before
    /// the `die`, or a `stop` after it.
    requested: bool,
    /// Unix milliseconds.
    at: Option<i64>,
    /// Whether the `die` event was seen, rather than the exit found by
    /// inspecting the container afterwards.
    seen: bool,
}

impl LastExit {
    const fn crashed(self) -> bool {
        !self.requested && (self.oom || !matches!(self.code, Some(0)))
    }
}

/// A container's recent history, kept by
/// [`Containers`](super::Containers) from its events or filled in from an
/// inspect, and read by [`stop_reason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Trail {
    /// When the current or last run started, in Unix milliseconds.
    started_at: Option<i64>,
    /// A stop was asked of Docker during the current run.
    requested: bool,
    /// An `oom` event arrived during the current run.
    oom: bool,
    /// Whether the container has exited since it last started.
    exited: bool,
    last_exit: Option<LastExit>,
    /// Crashes in a row, each after a run shorter than [`SHORT_RUN_MS`].
    crashes: u32,
}

/// What `GET /containers/{id}/json` says about a stopped container's last
/// run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InspectedExit {
    /// `State.ExitCode`.
    pub exit_code: Option<i64>,
    /// `State.OOMKilled`.
    pub oom_killed: bool,
    /// `State.StartedAt`, in Unix milliseconds.
    pub started_at: Option<i64>,
    /// `State.FinishedAt`, in Unix milliseconds.
    pub finished_at: Option<i64>,
    /// `RestartCount`: restarts by the restart policy since it was last
    /// started by hand.
    pub restart_count: u32,
}

impl Trail {
    /// The history an inspect implies, for a container stopped before the
    /// daemon was watching. 130 and 143 are taken as a stop through
    /// Docker, since a stop signal is the usual way to get them.
    pub fn from_inspect(inspected: InspectedExit) -> Self {
        let requested = !inspected.oom_killed && matches!(inspected.exit_code, Some(130 | 143));
        let last_exit = LastExit {
            code: inspected.exit_code,
            oom: inspected.oom_killed,
            requested,
            at: inspected.finished_at,
            seen: false,
        };
        let crashes = if last_exit.crashed() {
            inspected.restart_count.saturating_add(1)
        } else {
            0
        };
        Self {
            started_at: inspected.started_at,
            requested,
            oom: inspected.oom_killed,
            exited: true,
            last_exit: Some(last_exit),
            crashes,
        }
    }

    /// Whether the last exit is known, from an event or an inspect.
    pub const fn knows_exit(&self) -> bool {
        self.last_exit.is_some()
    }

    /// Record one event.
    pub fn record(&mut self, event: &ContainerEvent) {
        match event.action {
            ContainerAction::Start | ContainerAction::Restart => {
                self.started_at = event.at;
                self.requested = false;
                self.oom = false;
                self.exited = false;
            }
            ContainerAction::Kill if is_stop_signal(event.signal) => self.requested = true,
            ContainerAction::Oom => self.oom = true,
            ContainerAction::Die => self.die(event),
            ContainerAction::Stop => self.stop(),
            _ => {}
        }
    }

    fn die(&mut self, event: &ContainerEvent) {
        let exit = LastExit {
            code: event.exit_code,
            oom: self.oom,
            requested: self.requested,
            at: event.at,
            seen: true,
        };
        let short = matches!(
            (self.started_at, event.at),
            (Some(started), Some(died)) if died.saturating_sub(started) < SHORT_RUN_MS
        );
        self.crashes = match (exit.crashed(), short) {
            (false, _) => 0,
            (true, true) => self.crashes.saturating_add(1),
            (true, false) => 1,
        };
        self.last_exit = Some(exit);
        self.exited = true;
    }

    /// `stop` follows `die`, so it marks the exit just recorded as asked
    /// for. This catches stop signals [`is_stop_signal`] does not know,
    /// such as the `SIGWINCH` Apache stops on.
    fn stop(&mut self) {
        self.requested = true;
        if !self.exited {
            return;
        }
        if let Some(exit) = &mut self.last_exit
            && !exit.requested
        {
            exit.requested = true;
            self.crashes = 0;
        }
    }
}

/// Whether a `kill` event's signal asks the container to stop. `None` is
/// an event without a readable signal, taken as a stop. Signals used to
/// poke a running process, such as `SIGHUP` to reload, are not.
pub fn is_stop_signal(signal: Option<i32>) -> bool {
    matches!(signal, None | Some(2 | 3 | 6 | 9 | 15))
}

/// Why a container in `state`, with history `trail`, is not running.
///
/// `intent` is the last action Alchemy recorded for its Compose project.
/// A stopping action covers the run that was going when it was recorded:
/// a container that started after it, or exited well before it, stopped
/// for some other reason.
pub fn stop_reason(state: ContainerState, trail: &Trail, intent: Option<&Intent>) -> StopReason {
    match state {
        ContainerState::Running => StopReason::Running,
        ContainerState::Paused => StopReason::Paused,
        ContainerState::Created => StopReason::NotStarted,
        ContainerState::Restarting => {
            let exit = trail.last_exit.unwrap_or_default();
            StopReason::CrashLooping {
                exit_code: exit.code,
                oom: exit.oom,
            }
        }
        ContainerState::Exited | ContainerState::Dead | ContainerState::Removing => {
            exited_reason(trail, intent)
        }
    }
}

fn exited_reason(trail: &Trail, intent: Option<&Intent>) -> StopReason {
    let exit = trail.last_exit;
    if intent.is_some_and(|intent| covers(intent, trail.started_at, exit.and_then(|e| e.at))) {
        return StopReason::StoppedByAlchemy;
    }
    let Some(exit) = exit else {
        return if trail.requested {
            StopReason::StoppedOutside
        } else {
            StopReason::Unknown
        };
    };
    if exit.crashed() {
        let (exit_code, oom) = (exit.code, exit.oom);
        return if trail.crashes >= CRASH_LOOP {
            StopReason::CrashLooping { exit_code, oom }
        } else {
            StopReason::Crashed { exit_code, oom }
        };
    }
    if exit.requested {
        StopReason::StoppedOutside
    } else {
        StopReason::Finished
    }
}

fn covers(intent: &Intent, started_at: Option<i64>, exited_at: Option<i64>) -> bool {
    intent.action.stops()
        && started_at.is_none_or(|started| started <= intent.at.saturating_add(CLOCK_SLACK_MS))
        && exited_at.is_none_or(|exited| exited.saturating_add(CLOCK_SLACK_MS) >= intent.at)
}

/// One reason for a whole app, from its containers' reasons: the most
/// pressing one, so a crash in one service is not hidden by the others
/// running. An app with no containers is
/// [`StopReason::StoppedByAlchemy`] after Alchemy took it down, and
/// [`StopReason::NotStarted`] otherwise.
pub fn summarize(
    reasons: impl IntoIterator<Item = StopReason>,
    intent: Option<&Intent>,
) -> StopReason {
    reasons
        .into_iter()
        .max_by_key(|reason| reason.severity())
        .unwrap_or_else(|| {
            if intent.is_some_and(|intent| intent.action.stops()) {
                StopReason::StoppedByAlchemy
            } else {
                StopReason::NotStarted
            }
        })
}

/// Parse an RFC 3339 timestamp as Docker writes it, such as
/// `2024-05-01T12:00:00.123456789Z`, into Unix milliseconds.
///
/// Docker's zero time, `0001-01-01T00:00:00Z`, means "never" and is
/// `None`, as is anything before 1970 or not in that form.
pub fn parse_timestamp_ms(text: &str) -> Option<i64> {
    let (date, rest) = text.trim().split_once(['T', 't'])?;
    let mut date = date.splitn(3, '-');
    let year: i64 = number(date.next()?, 4)?;
    let month: i64 = number(date.next()?, 2)?;
    let day: i64 = number(date.next()?, 2)?;
    let zone_at = rest.find(['Z', 'z', '+', '-'])?;
    let (time, zone) = rest.split_at(zone_at);
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut clock = clock.splitn(3, ':');
    let hour: i64 = number(clock.next()?, 2)?;
    let minute: i64 = number(clock.next()?, 2)?;
    let second: i64 = number(clock.next()?, 2)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let millis = fraction
        .bytes()
        .chain(std::iter::repeat(b'0'))
        .take(3)
        .fold(0, |ms, digit| ms * 10 + i64::from(digit - b'0'));
    let offset = match zone {
        "Z" | "z" => 0,
        _ => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let (h, m) = zone.get(1..)?.split_once(':')?;
            sign * (number(h, 2)? * 60 + number(m, 2)?) * 60
        }
    };
    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset;
    (seconds >= 0).then_some(seconds * 1_000 + millis)
}

fn number(text: &str, digits: usize) -> Option<i64> {
    (text.len() == digits && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// Days from 1970-01-01 to a proleptic Gregorian date, after Howard
/// Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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

    use ContainerAction as A;
    use ContainerState as S;

    /// One event at `at` seconds.
    fn ev(action: ContainerAction, at: i64) -> ContainerEvent {
        let mut event = ContainerEvent::new("abc", action);
        event.at = Some(at * 1_000);
        event
    }

    fn die(at: i64, code: i64) -> ContainerEvent {
        let mut event = ev(A::Die, at);
        event.exit_code = Some(code);
        event
    }

    fn kill(at: i64, signal: i32) -> ContainerEvent {
        let mut event = ev(A::Kill, at);
        event.signal = Some(signal);
        event
    }

    fn replay(events: &[ContainerEvent]) -> Trail {
        let mut trail = Trail::default();
        for event in events {
            trail.record(event);
        }
        trail
    }

    fn crashed(code: i64) -> StopReason {
        StopReason::Crashed {
            exit_code: Some(code),
            oom: false,
        }
    }

    fn looping(code: i64) -> StopReason {
        StopReason::CrashLooping {
            exit_code: Some(code),
            oom: false,
        }
    }

    fn intent(action: IntentAction, at: i64) -> Intent {
        Intent::new(action, at * 1_000)
    }

    #[test]
    #[allow(clippy::too_many_lines, clippy::type_complexity)]
    fn classifies_event_sequences() {
        use IntentAction as I;
        let start = ev(A::Start, 100);
        let stop = ev(A::Stop, 111);
        let oom = ev(A::Oom, 109);
        let crash_loop = [
            ev(A::Start, 100),
            die(101, 1),
            ev(A::Start, 102),
            die(103, 1),
            ev(A::Start, 105),
            die(106, 1),
        ];
        let cases: Vec<(&str, S, Vec<ContainerEvent>, Option<Intent>, StopReason)> = vec![
            (
                "running",
                S::Running,
                vec![start.clone()],
                None,
                StopReason::Running,
            ),
            (
                "paused",
                S::Paused,
                vec![start.clone()],
                None,
                StopReason::Paused,
            ),
            ("created", S::Created, vec![], None, StopReason::NotStarted),
            (
                "docker stop, exit 143",
                S::Exited,
                vec![start.clone(), kill(110, 15), die(110, 143), stop.clone()],
                None,
                StopReason::StoppedOutside,
            ),
            (
                "docker stop, app exits 0 on SIGTERM",
                S::Exited,
                vec![start.clone(), kill(110, 15), die(110, 0), stop.clone()],
                None,
                StopReason::StoppedOutside,
            ),
            (
                "docker stop times out, SIGKILL, exit 137",
                S::Exited,
                vec![
                    start.clone(),
                    kill(110, 15),
                    kill(120, 9),
                    die(120, 137),
                    ev(A::Stop, 120),
                ],
                None,
                StopReason::StoppedOutside,
            ),
            (
                "docker stop with SIGWINCH, caught by the stop event",
                S::Exited,
                vec![start.clone(), kill(110, 28), die(110, 0), stop.clone()],
                None,
                StopReason::StoppedOutside,
            ),
            (
                "docker kill",
                S::Exited,
                vec![start.clone(), kill(110, 9), die(110, 137)],
                None,
                StopReason::StoppedOutside,
            ),
            (
                "kill event without a signal",
                S::Exited,
                vec![start.clone(), ev(A::Kill, 110), die(110, 137)],
                None,
                StopReason::StoppedOutside,
            ),
            (
                "SIGHUP to reload, then a crash",
                S::Exited,
                vec![start.clone(), kill(105, 1), die(500, 2)],
                None,
                crashed(2),
            ),
            (
                "process killed from the host",
                S::Exited,
                vec![start.clone(), die(500, 137)],
                None,
                crashed(137),
            ),
            (
                "SIGTERM from the host, no Docker events",
                S::Exited,
                vec![start.clone(), die(500, 143)],
                None,
                crashed(143),
            ),
            (
                "exits with an error",
                S::Exited,
                vec![start.clone(), die(500, 1)],
                None,
                crashed(1),
            ),
            (
                "out of memory",
                S::Exited,
                vec![start.clone(), oom.clone(), die(110, 137)],
                None,
                StopReason::Crashed {
                    exit_code: Some(137),
                    oom: true,
                },
            ),
            (
                "die without an exit code",
                S::Exited,
                vec![start.clone(), ev(A::Die, 500)],
                None,
                StopReason::Crashed {
                    exit_code: None,
                    oom: false,
                },
            ),
            (
                "finished job",
                S::Exited,
                vec![start.clone(), die(500, 0)],
                None,
                StopReason::Finished,
            ),
            (
                "stopped by Alchemy",
                S::Exited,
                vec![start.clone(), kill(110, 15), die(110, 143), stop.clone()],
                Some(intent(I::Stop, 109)),
                StopReason::StoppedByAlchemy,
            ),
            (
                "taken down by Alchemy, exit 0",
                S::Exited,
                vec![start.clone(), kill(110, 15), die(110, 0)],
                Some(intent(I::Down, 109)),
                StopReason::StoppedByAlchemy,
            ),
            (
                "restart by Alchemy that failed to start again",
                S::Exited,
                vec![start.clone(), kill(110, 15), die(110, 0), stop.clone()],
                Some(intent(I::Restart, 109)),
                StopReason::StoppedByAlchemy,
            ),
            (
                "out of memory while Alchemy stops it",
                S::Exited,
                vec![start.clone(), oom.clone(), die(110, 137)],
                Some(intent(I::Stop, 109)),
                StopReason::StoppedByAlchemy,
            ),
            (
                "engine clock a little behind",
                S::Exited,
                vec![start.clone(), kill(107, 15), die(107, 143)],
                Some(intent(I::Stop, 109)),
                StopReason::StoppedByAlchemy,
            ),
            (
                "crashed well before Alchemy stopped it",
                S::Exited,
                vec![start.clone(), die(101, 1)],
                Some(intent(I::Stop, 200)),
                crashed(1),
            ),
            (
                "started by hand after Alchemy stopped it, then crashed",
                S::Exited,
                vec![ev(A::Start, 300), die(400, 1)],
                Some(intent(I::Stop, 200)),
                crashed(1),
            ),
            (
                "crash after Alchemy started it",
                S::Exited,
                vec![start.clone(), die(500, 1)],
                Some(intent(I::Up, 99)),
                crashed(1),
            ),
            (
                "crash long after an up that left it running",
                S::Exited,
                vec![start.clone(), die(5_000, 1)],
                Some(intent(I::Start, 200)),
                crashed(1),
            ),
            (
                "crash loop",
                S::Exited,
                crash_loop.to_vec(),
                None,
                looping(1),
            ),
            (
                "crash loop while restarting",
                S::Restarting,
                crash_loop.to_vec(),
                None,
                looping(1),
            ),
            (
                "crash loop running between crashes",
                S::Running,
                [crash_loop.as_slice(), &[ev(A::Start, 108)]].concat(),
                None,
                StopReason::Running,
            ),
            (
                "two quick crashes are not a loop yet",
                S::Exited,
                crash_loop[..4].to_vec(),
                None,
                crashed(1),
            ),
            (
                "a long run resets the count",
                S::Exited,
                [crash_loop.as_slice(), &[ev(A::Start, 110), die(1_000, 1)]].concat(),
                None,
                crashed(1),
            ),
            (
                "a stop resets the count",
                S::Exited,
                [
                    crash_loop.as_slice(),
                    &[
                        ev(A::Start, 110),
                        kill(112, 15),
                        die(112, 143),
                        ev(A::Stop, 112),
                    ],
                    &[ev(A::Start, 120), die(121, 1)],
                ]
                .concat(),
                None,
                crashed(1),
            ),
            (
                "out-of-memory loop",
                S::Exited,
                vec![
                    ev(A::Start, 100),
                    ev(A::Oom, 101),
                    die(101, 137),
                    ev(A::Start, 102),
                    ev(A::Oom, 103),
                    die(103, 137),
                    ev(A::Start, 104),
                    ev(A::Oom, 105),
                    die(105, 137),
                ],
                None,
                StopReason::CrashLooping {
                    exit_code: Some(137),
                    oom: true,
                },
            ),
            (
                "restart policy brought a finished job back, then it was stopped",
                S::Exited,
                vec![
                    start.clone(),
                    die(101, 0),
                    ev(A::Start, 102),
                    kill(110, 15),
                    die(110, 0),
                    stop,
                ],
                None,
                StopReason::StoppedOutside,
            ),
            (
                "exited, nothing known",
                S::Exited,
                vec![],
                None,
                StopReason::Unknown,
            ),
            (
                "exited, nothing known, after Alchemy stopped it",
                S::Exited,
                vec![],
                Some(intent(I::Stop, 1)),
                StopReason::StoppedByAlchemy,
            ),
            (
                "dead after a crash",
                S::Dead,
                vec![start.clone(), die(500, 1)],
                None,
                crashed(1),
            ),
        ];
        for (name, state, events, intent, want) in cases {
            let trail = replay(&events);
            assert_eq!(stop_reason(state, &trail, intent.as_ref()), want, "{name}");
        }
    }

    #[test]
    fn classifies_inspected_exits() {
        let inspected = |code: i64, oom: bool, restarts: u32| InspectedExit {
            exit_code: Some(code),
            oom_killed: oom,
            started_at: Some(100_000),
            finished_at: Some(200_000),
            restart_count: restarts,
        };
        let cases = [
            (
                "SIGTERM",
                inspected(143, false, 0),
                None,
                StopReason::StoppedOutside,
            ),
            (
                "SIGINT",
                inspected(130, false, 0),
                None,
                StopReason::StoppedOutside,
            ),
            ("zero", inspected(0, false, 0), None, StopReason::Finished),
            ("error", inspected(1, false, 0), None, crashed(1)),
            ("SIGKILL", inspected(137, false, 0), None, crashed(137)),
            (
                "OOMKilled",
                inspected(137, true, 0),
                None,
                StopReason::Crashed {
                    exit_code: Some(137),
                    oom: true,
                },
            ),
            (
                "gave up restarting",
                inspected(1, false, 5),
                None,
                looping(1),
            ),
            ("restarted once", inspected(1, false, 1), None, crashed(1)),
            (
                "stopped by Alchemy",
                inspected(137, false, 0),
                Some(intent(IntentAction::Down, 199)),
                StopReason::StoppedByAlchemy,
            ),
            (
                "stopped long before Alchemy's stop",
                inspected(1, false, 0),
                Some(intent(IntentAction::Stop, 900)),
                crashed(1),
            ),
        ];
        for (name, inspected, intent, want) in cases {
            let trail = Trail::from_inspect(inspected);
            assert!(trail.knows_exit(), "{name}");
            assert_eq!(
                stop_reason(S::Exited, &trail, intent.as_ref()),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn stop_signals() {
        let cases = [
            (None, true),
            (Some(2), true),
            (Some(3), true),
            (Some(6), true),
            (Some(9), true),
            (Some(15), true),
            (Some(1), false),
            (Some(10), false),
            (Some(12), false),
            (Some(28), false),
        ];
        for (signal, want) in cases {
            assert_eq!(is_stop_signal(signal), want, "{signal:?}");
        }
    }

    #[test]
    fn summarize_shows_the_most_pressing_reason() {
        let down = intent(IntentAction::Down, 1);
        let up = intent(IntentAction::Up, 1);
        let cases: [(&[StopReason], Option<&Intent>, StopReason); 10] = [
            (&[], None, StopReason::NotStarted),
            (&[], Some(&up), StopReason::NotStarted),
            (&[], Some(&down), StopReason::StoppedByAlchemy),
            (
                &[StopReason::Running, StopReason::Running],
                None,
                StopReason::Running,
            ),
            (
                &[StopReason::Running, StopReason::Finished],
                None,
                StopReason::Running,
            ),
            (&[StopReason::Finished], None, StopReason::Finished),
            (
                &[StopReason::Running, crashed(1), StopReason::StoppedOutside],
                None,
                crashed(1),
            ),
            (&[crashed(1), looping(2)], None, looping(2)),
            (
                &[StopReason::StoppedByAlchemy, StopReason::StoppedByAlchemy],
                Some(&down),
                StopReason::StoppedByAlchemy,
            ),
            (
                &[StopReason::StoppedByAlchemy, StopReason::StoppedOutside],
                None,
                StopReason::StoppedOutside,
            ),
        ];
        for (reasons, intent, want) in cases {
            assert_eq!(
                summarize(reasons.iter().copied(), intent),
                want,
                "{reasons:?}"
            );
        }
    }

    #[test]
    fn reasons_in_plain_words() {
        let cases = [
            (StopReason::Running, "Running"),
            (StopReason::Paused, "Paused"),
            (StopReason::NotStarted, "Not running"),
            (StopReason::StoppedByAlchemy, "Stopped"),
            (StopReason::StoppedOutside, "Stopped outside Alchemy"),
            (StopReason::Finished, "Finished"),
            (StopReason::Unknown, "Stopped"),
            (crashed(1), "Crashed (exit code 1)"),
            (
                StopReason::Crashed {
                    exit_code: Some(137),
                    oom: true,
                },
                "Crashed (out of memory)",
            ),
            (
                StopReason::Crashed {
                    exit_code: None,
                    oom: false,
                },
                "Crashed",
            ),
            (looping(2), "Keeps crashing (exit code 2)"),
            (
                StopReason::CrashLooping {
                    exit_code: None,
                    oom: true,
                },
                "Keeps crashing (out of memory)",
            ),
        ];
        for (reason, want) in cases {
            assert_eq!(reason.to_string(), want, "{reason:?}");
        }
        assert!(crashed(1).is_problem());
        assert!(looping(1).is_problem());
        assert!(!StopReason::StoppedOutside.is_problem());
    }

    #[test]
    fn stop_reason_wire_shape() {
        let cases = [
            (StopReason::Running, r#"{"kind":"running"}"#),
            (StopReason::Paused, r#"{"kind":"paused"}"#),
            (StopReason::NotStarted, r#"{"kind":"not_started"}"#),
            (
                StopReason::StoppedByAlchemy,
                r#"{"kind":"stopped_by_alchemy"}"#,
            ),
            (StopReason::StoppedOutside, r#"{"kind":"stopped_outside"}"#),
            (StopReason::Finished, r#"{"kind":"finished"}"#),
            (StopReason::Unknown, r#"{"kind":"unknown"}"#),
            (
                StopReason::Crashed {
                    exit_code: Some(1),
                    oom: false,
                },
                r#"{"kind":"crashed","exit_code":1,"oom":false}"#,
            ),
            (
                StopReason::CrashLooping {
                    exit_code: None,
                    oom: true,
                },
                r#"{"kind":"crash_looping","exit_code":null,"oom":true}"#,
            ),
        ];
        for (reason, json) in cases {
            assert_eq!(serde_json::to_string(&reason).unwrap(), json);
            assert_eq!(serde_json::from_str::<StopReason>(json).unwrap(), reason);
        }
        assert_eq!(
            serde_json::from_str::<StopReason>(r#"{"kind":"vanished"}"#).unwrap(),
            StopReason::Unknown
        );
    }

    #[test]
    fn intent_actions_round_trip() {
        for action in IntentAction::ALL {
            assert_eq!(IntentAction::parse(action.as_str()), Some(action));
            assert_eq!(
                serde_json::to_value(action).unwrap(),
                serde_json::json!(action.as_str())
            );
        }
        assert_eq!(IntentAction::parse("pull"), None);
        let stops: Vec<_> = IntentAction::ALL
            .into_iter()
            .filter(|a| a.stops())
            .collect();
        assert_eq!(
            stops,
            [
                IntentAction::Restart,
                IntentAction::Stop,
                IntentAction::Down
            ]
        );
    }

    #[test]
    fn parses_docker_timestamps() {
        let cases = [
            ("1970-01-01T00:00:00Z", Some(0)),
            ("2024-05-01T12:00:00Z", Some(1_714_564_800_000)),
            ("2024-05-01T12:00:00.123456789Z", Some(1_714_564_800_123)),
            ("2024-05-01T12:00:00.5Z", Some(1_714_564_800_500)),
            ("2024-05-01T14:00:00+02:00", Some(1_714_564_800_000)),
            ("2024-05-01T07:30:00.25-04:30", Some(1_714_564_800_250)),
            ("2000-02-29T00:00:00Z", Some(951_782_400_000)),
            ("0001-01-01T00:00:00Z", None),
            ("", None),
            ("2024-05-01", None),
            ("2024-13-01T00:00:00Z", None),
            ("2024-05-01T25:00:00Z", None),
            ("2024-05-01T12:00:00", None),
            ("2024-05-01T12:00:00.xZ", None),
            ("24-05-01T12:00:00Z", None),
        ];
        for (text, want) in cases {
            assert_eq!(parse_timestamp_ms(text), want, "{text:?}");
        }
    }
}
