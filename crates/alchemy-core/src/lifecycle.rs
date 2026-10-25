//! Service lifecycle and recovery: autostart, sleep and wake, a Docker
//! connection that went quiet, and bringing apps back afterwards.
//!
//! Everything here is a pure decision over plain values. The daemon reads
//! which service files are installed, samples its clocks, pings the
//! engine, and watches stack statuses, then passes them in. Out come the
//! [`AutostartStep`]s to reach a mode, the per-user service files, [`Woke`]
//! events, when to ping the engine, and the apps to start with a reason.
//!
//! Times are [`Duration`](std::time::Duration)s on the daemon's monotonic
//! clock unless they say otherwise: it stops while the computer sleeps,
//! which is what [`WakeDetector`] relies on.

mod autostart;
mod last_seen;
mod liveness;
mod reconcile;
mod service_file;
mod wake;

pub use autostart::{
    AutostartMode, AutostartModeError, AutostartStep, InstalledServices, LogHint, UserService,
    autostart_steps, log_hint,
};
pub use last_seen::{Disruption, LastSeen, LastSeenChange, STOP_GRACE};
pub use liveness::{IDLE_PING, Liveness, LivenessCheck, PING_TIMEOUT, PingOutcome};
pub use reconcile::{
    AppPlan, Decision, ReconcilePlan, ReconcileTrigger, SkipReason, plan_reconcile,
};
pub use service_file::{
    LAUNCH_AGENT_LABEL, LOG_DIR, LOG_FILE, SYSTEMD_USER_UNIT, ServiceFileError, launch_agent_path,
    launch_agent_plist, log_path, systemd_user_unit, systemd_user_unit_path,
};
pub use wake::{ClockSample, WAKE_THRESHOLD, WakeDetector, Woke};
