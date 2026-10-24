//! Resource usage at the daemon's edge: sampling the machine and its
//! containers, keeping the history, and answering for it.
//!
//! The counters come from a [`MetricsSource`]: [`SystemMetrics`] for this
//! host and its engine, a fake in tests. The [`Collector`] task samples
//! every [`INTERVAL`], publishes the latest figures in [`State`], and
//! stores them in the [`Store`](crate::store::Store) at every tier of
//! [`TIERS`](alchemy_core::metrics::TIERS). Rates, rollups, and retention
//! are pure functions in [`alchemy_core::metrics`].

mod collector;
mod host;
mod system;

use std::future::Future;

use alchemy_api::{MetricsHistory, MetricsResponse};
use alchemy_core::engine::ContainerStatus;
use alchemy_core::metrics::{ContainerSample, HostSample, MetricsRange};

pub use collector::{Collector, INTERVAL, MAX_IN_FLIGHT, PRUNE_EVERY, STATS_TIMEOUT, WARMUP};
pub use host::{counts_toward_total, disk_for, parse_proc_stat};
pub use system::SystemMetrics;

use crate::State;
use crate::engine::{ApiError, EndpointError};
use crate::store::{SharedStore, StoreError};

/// Where the [`Collector`] reads raw counters and the time.
///
/// [`SystemMetrics`] is the real one. Every method is a side effect, so
/// tests inject a fake with fixed readings and a fixed clock.
pub trait MetricsSource: Send + Sync + 'static {
    /// The wall-clock time samples are stamped with, in Unix milliseconds.
    fn now_ms(&self) -> i64;

    /// The machine's raw counters.
    fn host(&self) -> impl Future<Output = Result<HostSample, SourceError>> + Send;

    /// One reading of `container`'s raw counters from the engine at
    /// `endpoint`, such as `unix:///var/run/docker.sock`.
    fn container(
        &self,
        endpoint: &str,
        container: &ContainerStatus,
    ) -> impl Future<Output = Result<ContainerSample, SourceError>> + Send;
}

/// Why a [`MetricsSource`] could not read something.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SourceError {
    /// The machine's counters could not be read.
    #[error("cannot read the machine's usage: {0}")]
    Host(String),
    /// The engine endpoint is not one Alchemy can talk to.
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    /// The engine refused or failed the stats request.
    #[error(transparent)]
    Engine(#[from] ApiError),
    /// The engine took longer than [`STATS_TIMEOUT`].
    #[error("container stats timed out")]
    Timeout,
}

/// Whose figures a metrics route answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The machine and every project.
    All,
    /// One Compose project's totals and containers, and no machine figures.
    Project(String),
    /// An app whose Compose project is not known yet: no machine figures
    /// and no project's.
    Unresolved,
}

impl Scope {
    fn project(&self) -> Option<&str> {
        match self {
            Self::Project(project) => Some(project),
            Self::All | Self::Unresolved => None,
        }
    }
}

/// The latest figures as `GET /api/metrics` returns them, narrowed to
/// `scope`.
pub fn current(state: &State, scope: &Scope) -> MetricsResponse {
    let mut metrics = state.metrics();
    if *scope != Scope::All {
        let project = scope.project();
        metrics.machine = None;
        metrics
            .projects
            .retain(|p| project.is_some_and(|project| p.name == project));
        metrics
            .containers
            .retain(|c| project.is_some() && c.project.as_deref() == project);
    }
    metrics
}

/// The stored points of `range` up to `now`, in Unix seconds, oldest
/// first, narrowed to `scope`.
///
/// A point where the project had no container is left out rather than
/// sent as zeros, so the gap shows. [`Scope::Unresolved`] has no points
/// and reads nothing.
pub async fn history(
    store: &SharedStore,
    range: MetricsRange,
    scope: Scope,
    now: i64,
) -> Result<MetricsHistory, StoreError> {
    let tier = range.tier();
    let points = match &scope {
        Scope::Unresolved => Vec::new(),
        Scope::All => {
            store
                .metric_points(tier.step, now - tier.span, now + 1)
                .await?
        }
        Scope::Project(project) => store
            .metric_points(tier.step, now - tier.span, now + 1)
            .await?
            .into_iter()
            .map(|point| point.for_project(project))
            .filter(|point| !point.containers.is_empty())
            .collect(),
    };
    Ok(MetricsHistory {
        range,
        step: u32::try_from(tier.step).unwrap_or(u32::MAX),
        project: scope.project().map(str::to_owned),
        points,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::sync::Arc;

    use alchemy_core::metrics::{
        ContainerUsage, MachineUsage, MetricsPoint, ProjectUsage, TIERS, projects_of,
    };

    use super::*;
    use crate::store::MemoryStore;

    fn container(name: &str, project: &str) -> ContainerUsage {
        ContainerUsage {
            id: name.into(),
            name: name.into(),
            project: Some(project.into()),
            memory: 10,
            ..ContainerUsage::default()
        }
    }

    fn point(t: i64, containers: Vec<ContainerUsage>) -> MetricsPoint {
        MetricsPoint {
            t,
            machine: Some(MachineUsage::default()),
            projects: projects_of(&containers),
            containers,
        }
    }

    #[test]
    fn current_filters_to_one_project() {
        let state = State::new();
        let containers = vec![container("web-1", "web"), container("db-1", "db")];
        state.set_metrics(MetricsResponse {
            at: Some(60),
            interval: 60,
            containers_in_vm: true,
            machine: Some(MachineUsage::default()),
            projects: projects_of(&containers),
            containers,
        });
        let all = current(&state, &Scope::All);
        assert_eq!(all.projects.len(), 2);
        assert!(all.machine.is_some());

        let web = current(&state, &Scope::Project("web".into()));
        assert_eq!(web.machine, None);
        assert_eq!(web.at, Some(60));
        assert!(web.containers_in_vm);
        assert_eq!(web.containers, [container("web-1", "web")]);
        assert_eq!(
            web.projects,
            [ProjectUsage {
                name: "web".into(),
                containers: 1,
                memory: 10,
                ..ProjectUsage::default()
            }]
        );

        for scope in [Scope::Project("nope".into()), Scope::Unresolved] {
            let none = current(&state, &scope);
            assert_eq!((none.at, &none.machine), (Some(60), &None), "{scope:?}");
            assert!(
                none.projects.is_empty() && none.containers.is_empty(),
                "{scope:?}"
            );
        }
    }

    #[tokio::test]
    async fn history_reads_the_ranges_tier_and_leaves_gaps() {
        let store: SharedStore = Arc::new(MemoryStore::new());
        let day = TIERS[2];
        let now = 1_000_000;
        store
            .put_metric_points(
                day.step,
                &[
                    point(now - day.span - 1, vec![container("web-1", "web")]),
                    point(now - day.span, vec![container("web-1", "web")]),
                    point(now - 2_400, vec![container("db-1", "db")]),
                    point(
                        now - 1_200,
                        vec![container("web-1", "web"), container("db-1", "db")],
                    ),
                ],
            )
            .await
            .unwrap();
        store
            .put_metric_points(TIERS[0].step, &[point(now, vec![])])
            .await
            .unwrap();

        let all = history(&store, MetricsRange::Day, Scope::All, now)
            .await
            .unwrap();
        assert_eq!(
            (all.range, all.step, all.project.as_deref()),
            (MetricsRange::Day, 1_200, None)
        );
        let times: Vec<_> = all.points.iter().map(|p| p.t).collect();
        assert_eq!(times, [now - day.span, now - 2_400, now - 1_200]);
        assert!(all.points[0].machine.is_some());

        let web = history(&store, MetricsRange::Day, Scope::Project("web".into()), now)
            .await
            .unwrap();
        assert_eq!(web.project.as_deref(), Some("web"));
        let times: Vec<_> = web.points.iter().map(|p| p.t).collect();
        assert_eq!(times, [now - day.span, now - 1_200], "the gap stays a gap");
        for point in &web.points {
            assert_eq!(point.machine, None);
            assert_eq!(point.containers, [container("web-1", "web")]);
            assert_eq!(point.projects.len(), 1);
        }

        let unresolved = history(&store, MetricsRange::Day, Scope::Unresolved, now)
            .await
            .unwrap();
        assert_eq!(unresolved.project, None);
        assert_eq!(unresolved.points, []);
    }
}
