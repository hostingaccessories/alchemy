//! The task that samples usage, publishes it, and keeps its history.

use std::time::Duration;

use alchemy_api::MetricsResponse;
use alchemy_core::engine::{ContainerState, EngineFlavor, EngineState};
use alchemy_core::metrics::{
    MetricsPoint, Sample, TIERS, containers_in_vm, plan_rollups, rollup, rollup_window, usage_from,
};
use futures_util::StreamExt as _;
use futures_util::stream;
use tokio::sync::watch;

use super::{MetricsSource, SourceError};
use crate::State;
use crate::store::{SharedStore, StoreError};

/// How often the machine and its containers are sampled, whether or not
/// anyone is looking, so there is history when someone goes looking.
pub const INTERVAL: Duration = Duration::from_secs(60);

/// The gap between the first sample after start, which has no rates, and
/// the second, so live figures appear quickly.
pub const WARMUP: Duration = Duration::from_secs(10);

/// The most container stats requests in flight at once.
pub const MAX_IN_FLIGHT: usize = 4;

/// How long one container's stats may take before it is left out of the
/// sample.
pub const STATS_TIMEOUT: Duration = Duration::from_secs(5);

/// How often points past their tier's retention are deleted.
pub const PRUNE_EVERY: Duration = Duration::from_hours(1);

/// Samples usage every [`INTERVAL`] until shutdown.
///
/// Each cycle reads the machine and every running container (at most
/// [`MAX_IN_FLIGHT`] at a time, each bounded by [`STATS_TIMEOUT`]), turns
/// the counters into rates against the previous cycle, publishes the result
/// as [`State::metrics`], stores it at the finest tier, builds any coarser
/// bucket that is due, and prunes old points every [`PRUNE_EVERY`]. A
/// container whose stats fail is left out of that cycle; a failure to store
/// is logged and the next cycle carries on.
#[derive(Debug)]
pub struct Collector<M> {
    source: M,
    state: State,
    store: SharedStore,
    prev: Option<Sample>,
    last_prune: Option<i64>,
}

impl<M: MetricsSource> Collector<M> {
    /// A collector reading from `source`, publishing to `state`, and
    /// keeping history in `store`.
    pub fn new(source: M, state: State, store: SharedStore) -> Self {
        Self {
            source,
            state,
            store,
            prev: None,
            last_prune: None,
        }
    }

    /// Sample now, again after [`WARMUP`], then every [`INTERVAL`], until
    /// `shutdown` flips to `true` or its sender is dropped. A cycle in
    /// progress is abandoned on shutdown.
    pub async fn run(mut self, shutdown: watch::Receiver<bool>) {
        let mut delay = Duration::ZERO;
        loop {
            let mut stop = shutdown.clone();
            tokio::select! {
                _ = stop.wait_for(|stop| *stop) => return,
                () = async {
                    tokio::time::sleep(delay).await;
                    self.collect().await;
                } => {}
            }
            delay = if delay.is_zero() { WARMUP } else { INTERVAL };
        }
    }

    /// One cycle, returning the point it published.
    pub async fn collect(&mut self) -> MetricsPoint {
        let at_ms = self.source.now_ms();
        let host = match self.source.host().await {
            Ok(host) => Some(host),
            Err(error) => {
                tracing::warn!(%error, "cannot sample the machine");
                None
            }
        };
        let engine = self.state.engine();
        let containers = match (engine.state, engine.endpoint.as_deref()) {
            (EngineState::Ready, Some(endpoint)) => self.containers(endpoint).await,
            _ => Vec::new(),
        };
        let sample = Sample {
            at_ms,
            host,
            containers,
        };
        let point = usage_from(self.prev.as_ref(), &sample);
        self.prev = Some(sample);

        self.state.set_metrics(MetricsResponse {
            at: Some(point.t),
            interval: u32::try_from(INTERVAL.as_secs()).unwrap_or(u32::MAX),
            containers_in_vm: containers_in_vm(
                cfg!(target_os = "macos"),
                engine.flavor == Some(EngineFlavor::DockerDesktop),
            ),
            machine: point.machine.clone(),
            projects: point.projects.clone(),
            containers: point.containers.clone(),
        });

        let now = point.t;
        if let Err(error) = self.keep(&point).await {
            tracing::warn!(%error, "cannot store metrics");
        }
        if self.prune_due(now)
            && let Err(error) = self.prune(now).await
        {
            tracing::warn!(%error, "cannot prune old metrics");
        }
        point
    }

    /// Stats for every running container, in no particular order. Those
    /// that fail or time out are left out.
    async fn containers(&self, endpoint: &str) -> Vec<alchemy_core::metrics::ContainerSample> {
        let running: Vec<_> = self
            .state
            .containers()
            .iter()
            .filter(|c| c.state == ContainerState::Running)
            .cloned()
            .collect();
        let source = &self.source;
        stream::iter(running)
            .map(|container| async move {
                let result =
                    match tokio::time::timeout(STATS_TIMEOUT, source.container(endpoint, &container))
                        .await
                    {
                        Ok(result) => result,
                        Err(_) => Err(SourceError::Timeout),
                    };
                result
                    .inspect_err(|error| {
                        tracing::debug!(%error, container = %container.name, "no stats for container");
                    })
                    .ok()
            })
            .buffer_unordered(MAX_IN_FLIGHT)
            .filter_map(std::future::ready)
            .collect()
            .await
    }

    /// Store `point` at the finest tier, then build every coarser bucket
    /// that is due, finest first, so a bucket closing at the same time as
    /// the one below it sees that bucket.
    async fn keep(&self, point: &MetricsPoint) -> Result<(), StoreError> {
        let now = point.t;
        self.store
            .put_metric_points(TIERS[0].step, std::slice::from_ref(point))
            .await?;
        for pair in TIERS.windows(2) {
            let &[source, tier] = pair else {
                continue;
            };
            let (from, to) = rollup_window(now, tier, source);
            let built = self.store.metric_times(tier.step, from, to).await?;
            let source_times = self.store.metric_times(source.step, from, to).await?;
            for bucket in plan_rollups(now, tier, source, &built, &source_times) {
                let points = self
                    .store
                    .metric_points(source.step, bucket, bucket + tier.step)
                    .await?;
                if let Some(rolled) = rollup(bucket, &points) {
                    self.store.put_metric_points(tier.step, &[rolled]).await?;
                }
            }
        }
        Ok(())
    }

    fn prune_due(&mut self, now: i64) -> bool {
        let every = i64::try_from(PRUNE_EVERY.as_secs()).unwrap_or(i64::MAX);
        let due = self.last_prune.is_none_or(|last| now - last >= every);
        if due {
            self.last_prune = Some(now);
        }
        due
    }

    async fn prune(&self, now: i64) -> Result<(), StoreError> {
        for tier in TIERS {
            self.store.prune_metrics(tier.step, now - tier.keep).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]
mod tests {
    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use alchemy_core::engine::{ContainerStatus, EngineStatus};
    use alchemy_core::metrics::{ContainerSample, CpuReading, HostSample, Traffic};

    use super::*;
    use crate::store::MemoryStore;

    /// A source with a settable clock, machine, and per-container stats.
    #[derive(Debug, Default)]
    struct Fake {
        now_ms: AtomicI64,
        host: Mutex<Option<HostSample>>,
        stats: Mutex<HashMap<String, ContainerSample>>,
        slow: Mutex<Vec<String>>,
        in_flight: AtomicUsize,
        peak: AtomicUsize,
        endpoints: Mutex<Vec<String>>,
    }

    impl Fake {
        fn at(&self, secs: i64) {
            self.now_ms.store(secs * 1000, Ordering::SeqCst);
        }

        fn ticks(&self, busy: u64, total: u64, rx: u64) {
            *self.host.lock().unwrap() = Some(HostSample {
                cpu: Some(CpuReading::Ticks { busy, total }),
                cpus: 2,
                memory_used: 100,
                memory_total: 1_000,
                net: vec![("eth0".into(), Traffic { rx, tx: 0 })],
                ..HostSample::default()
            });
        }

        fn stats_for(&self, id: &str, project: &str, cpu: u64, system: u64) {
            self.stats.lock().unwrap().insert(
                id.into(),
                ContainerSample {
                    id: id.into(),
                    name: format!("{project}-{id}"),
                    project: Some(project.into()),
                    cpu_usage: cpu,
                    system_cpu: Some(system),
                    memory: 50,
                    ..ContainerSample::default()
                },
            );
        }
    }

    impl MetricsSource for Arc<Fake> {
        fn now_ms(&self) -> i64 {
            self.now_ms.load(Ordering::SeqCst)
        }

        fn host(&self) -> impl Future<Output = Result<HostSample, SourceError>> + Send {
            std::future::ready(
                self.host
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or_else(|| SourceError::Host("unreadable".into())),
            )
        }

        async fn container(
            &self,
            endpoint: &str,
            container: &ContainerStatus,
        ) -> Result<ContainerSample, SourceError> {
            self.endpoints.lock().unwrap().push(endpoint.to_owned());
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            let slow = self.slow.lock().unwrap().contains(&container.id);
            tokio::time::sleep(if slow {
                STATS_TIMEOUT * 2
            } else {
                Duration::from_millis(10)
            })
            .await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            let stats = self.stats.lock().unwrap().get(&container.id).cloned();
            stats.ok_or_else(|| SourceError::Host("no such container".into()))
        }
    }

    const ENDPOINT: &str = "unix:///var/run/docker.sock";

    fn ready_state(running: &[&str]) -> State {
        let state = State::new();
        let mut status = EngineStatus::new(EngineState::Ready);
        status.endpoint = Some(ENDPOINT.into());
        state.set_engine(status);
        state.update_containers(|c| {
            c.replace_all(
                running
                    .iter()
                    .map(|id| ContainerStatus::new(*id, *id, ContainerState::Running))
                    .chain([ContainerStatus::new(
                        "stopped",
                        "stopped",
                        ContainerState::Exited,
                    )]),
            )
        });
        state
    }

    fn collector(fake: &Arc<Fake>, state: &State) -> (Collector<Arc<Fake>>, SharedStore) {
        let store: SharedStore = Arc::new(MemoryStore::new());
        (
            Collector::new(fake.clone(), state.clone(), store.clone()),
            store,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn collect_publishes_rates_after_the_baseline() {
        let fake = Arc::new(Fake::default());
        let state = ready_state(&["a", "b"]);
        let (mut collector, store) = collector(&fake, &state);
        fake.stats_for("a", "web", 0, 0);
        fake.stats_for("b", "db", 0, 0);
        fake.ticks(0, 0, 0);
        fake.at(1_000);

        let first = collector.collect().await;
        assert_eq!(first.t, 1_000);
        assert_eq!(first.machine.as_ref().unwrap().cpu, None);
        assert_eq!(first.containers.len(), 2, "stopped containers are skipped");
        let current = state.metrics();
        assert_eq!(current.at, Some(1_000));
        assert_eq!(current.interval, 60);
        assert_eq!(current.machine.unwrap().memory_used, 100);

        fake.ticks(25, 100, 600);
        fake.stats_for("a", "web", 100, 1_000);
        fake.stats_for("b", "db", 300, 1_000);
        fake.at(1_060);
        let second = collector.collect().await;
        let machine = second.machine.unwrap();
        assert_eq!(machine.cpu, Some(25.0));
        assert_eq!(machine.net_rx, Some(10.0));
        let cpus: Vec<_> = second.containers.iter().map(|c| c.cpu).collect();
        assert_eq!(
            cpus,
            [Some(30.0), Some(10.0)],
            "sorted by name: db-b, web-a"
        );
        assert_eq!(state.metrics().projects.len(), 2);
        assert_eq!(
            fake.endpoints.lock().unwrap().iter().collect::<Vec<_>>(),
            [ENDPOINT; 4]
        );

        assert_eq!(
            store.metric_times(60, 0, 10_000).await.unwrap(),
            [1_000, 1_060]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn engine_not_ready_means_no_containers_and_a_broken_host_no_machine() {
        let fake = Arc::new(Fake::default());
        let state = State::new();
        state.update_containers(|c| {
            c.replace_all([ContainerStatus::new("a", "a", ContainerState::Running)])
        });
        fake.stats_for("a", "web", 0, 0);
        let (mut collector, _store) = collector(&fake, &state);
        let point = collector.collect().await;
        assert_eq!(point.machine, None);
        assert_eq!(point.containers, []);
        assert_eq!(*fake.endpoints.lock().unwrap(), Vec::<String>::new());
        assert_eq!(state.metrics().at, Some(0));
    }

    #[tokio::test(start_paused = true)]
    async fn at_most_four_stats_requests_and_slow_ones_are_dropped() {
        let ids: Vec<String> = (0..10).map(|i| format!("c{i}")).collect();
        let fake = Arc::new(Fake::default());
        let state = ready_state(&ids.iter().map(String::as_str).collect::<Vec<_>>());
        for id in &ids {
            fake.stats_for(id, "web", 0, 0);
        }
        fake.slow.lock().unwrap().push("c3".into());
        let (mut collector, _store) = collector(&fake, &state);
        let point = collector.collect().await;
        assert_eq!(fake.peak.load(Ordering::SeqCst), MAX_IN_FLIGHT);
        assert_eq!(point.containers.len(), 9);
        assert!(point.containers.iter().all(|c| c.id != "c3"));
    }

    #[tokio::test(start_paused = true)]
    async fn collect_rolls_up_and_catches_up_after_a_restart() {
        let fake = Arc::new(Fake::default());
        let state = ready_state(&[]);
        let store: SharedStore = Arc::new(MemoryStore::new());
        fake.ticks(0, 0, 0);

        let mut first = Collector::new(fake.clone(), state.clone(), store.clone());
        for minute in 0..14 {
            fake.at(36_000 + minute * 60);
            first.collect().await;
        }
        assert_eq!(
            store.metric_times(600, 0, 100_000).await.unwrap(),
            [36_000],
            "one closed 10 minute bucket; the next has too few points so far"
        );
        assert_eq!(
            store.metric_times(1_200, 0, 100_000).await.unwrap(),
            Vec::<i64>::new(),
            "no 20 minute bucket has closed"
        );

        drop(first);
        let mut second = Collector::new(fake.clone(), state, store.clone());
        fake.at(36_000 + 45 * 60);
        second.collect().await;
        assert_eq!(
            store.metric_times(600, 0, 100_000).await.unwrap(),
            [36_000],
            "the window the daemon was down for stays a gap"
        );
        assert_eq!(
            store.metric_times(1_200, 0, 100_000).await.unwrap(),
            [36_000],
            "caught up from the 10 minute buckets"
        );

        let ten = store.metric_points(600, 36_000, 36_001).await.unwrap();
        assert_eq!(ten[0].machine.as_ref().unwrap().memory_used, 100);
    }

    #[tokio::test(start_paused = true)]
    async fn prunes_each_tier_hourly() {
        let fake = Arc::new(Fake::default());
        let state = State::new();
        let (mut collector, store) = collector(&fake, &state);
        let old = MetricsPoint {
            t: 0,
            ..MetricsPoint::default()
        };
        for tier in TIERS {
            store
                .put_metric_points(tier.step, std::slice::from_ref(&old))
                .await
                .unwrap();
        }
        fake.at(TIERS[0].keep + 1);
        collector.collect().await;
        assert_eq!(
            store.metric_times(60, 0, 1).await.unwrap(),
            Vec::<i64>::new()
        );
        assert_eq!(store.metric_times(600, 0, 1).await.unwrap(), [0]);

        fake.at(TIERS[1].keep + 1);
        collector.collect().await;
        assert_eq!(
            store.metric_times(600, 0, 1).await.unwrap(),
            Vec::<i64>::new()
        );
        assert_eq!(store.metric_times(1_200, 0, 1).await.unwrap(), [0]);
    }

    #[tokio::test(start_paused = true)]
    async fn run_samples_now_after_the_warmup_then_every_interval_until_shutdown() {
        let fake = Arc::new(Fake::default());
        fake.ticks(0, 0, 0);
        let state = State::new();
        let (collector, store) = collector(&fake, &state);
        let (stop, stop_rx) = watch::channel(false);
        let mut metrics = state.watch_metrics();
        let task = tokio::spawn(collector.run(stop_rx));

        let start = tokio::time::Instant::now();
        for (secs, want) in [(0, 0), (10, 10), (70, 70), (130, 130)] {
            fake.at(secs);
            metrics.changed().await.unwrap();
            assert_eq!(metrics.borrow_and_update().at, Some(want));
            assert_eq!(start.elapsed().as_secs(), u64::try_from(secs).unwrap());
        }
        stop.send_replace(true);
        task.await.unwrap();
        assert_eq!(
            store.metric_times(60, 0, 200).await.unwrap(),
            [0, 10, 70, 130]
        );
    }
}
