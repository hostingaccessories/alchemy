//! [`stream()`]: one app's logs, merged from its containers.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use alchemy_api::apps::{App, AppList, ContainerState};
use alchemy_api::logs::{LogEvent, LogLine, LogStream, LogsDone, LogsFollowing};
use alchemy_core::engine::EngineState;
use alchemy_core::logs::{
    Cursor, LineSplitter, LogQueryError, MAX_TAIL_BYTES, Tail, Timestamp, merge_tail, parse_follow,
    parse_tail, split_timestamp,
};
use futures_util::StreamExt as _;
use futures_util::future::join_all;
use futures_util::stream::{self, Stream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::{LogChunk, LogError, LogRequest, SharedLogSource};
use crate::State;
use crate::apps;

/// How long a followed container's stream must have been over before it
/// is opened again, so a stop the app list has yet to show cannot spin.
pub const RESUME_DELAY: Duration = Duration::from_millis(500);

/// Events buffered for a slow client before reading pauses.
const BUFFER: usize = 256;

/// A validated logs request for one app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogQuery {
    /// The app's name.
    pub app: String,
    /// Only this service's containers, or every container when `None`.
    pub service: Option<String>,
    /// How many of the last lines to send first.
    pub tail: u32,
    /// Whether to keep sending new lines.
    pub follow: bool,
}

/// Why a logs request was refused before any line was read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LogsRefusal {
    /// No app has the name.
    #[error("no app is named {0:?}")]
    AppNotFound(String),
    /// The app has no service with the name.
    #[error("the app {app} has no service named {service:?}")]
    ServiceNotFound {
        /// The app's name.
        app: String,
        /// The service asked for.
        service: String,
    },
    /// `tail` or `follow` is not valid.
    #[error(transparent)]
    Query(#[from] LogQueryError),
    /// [`MAX_STREAMS`](super::MAX_STREAMS) log streams are already open.
    #[error(
        "alchemyd is already serving {} log streams; close one and try again",
        super::MAX_STREAMS
    )]
    Busy,
}

impl LogQuery {
    /// Check a request for `app`'s logs against `apps`. An empty
    /// `service` means every service.
    pub fn new(
        apps: &AppList,
        app: &str,
        service: Option<&str>,
        tail: Option<&str>,
        follow: Option<&str>,
    ) -> Result<Self, LogsRefusal> {
        let tail = parse_tail(tail)?;
        let follow = parse_follow(follow)?;
        let found =
            apps::find(apps, app).ok_or_else(|| LogsRefusal::AppNotFound(app.to_owned()))?;
        let service = service.filter(|s| !s.is_empty());
        if let Some(service) = service
            && !found.services.iter().any(|s| s.name == service)
        {
            return Err(LogsRefusal::ServiceNotFound {
                app: app.to_owned(),
                service: service.to_owned(),
            });
        }
        Ok(Self {
            app: app.to_owned(),
            service: service.map(str::to_owned),
            tail,
            follow,
        })
    }
}

/// `query`'s app's logs, read through `source`, as the events
/// `GET /api/apps/{name}/logs` sends.
///
/// The stream ends after [`LogEvent::Done`]. A followed stream sends
/// that only on an error, and otherwise lasts until it is dropped, which
/// stops reading every container.
///
/// It takes a place in [`State::log_streams`] until it is dropped, and is
/// refused with [`LogsRefusal::Busy`] when none is free.
pub fn stream(
    source: SharedLogSource,
    state: State,
    query: LogQuery,
) -> Result<impl Stream<Item = LogEvent> + Send + 'static, LogsRefusal> {
    let permit = state.log_streams().try_open().ok_or(LogsRefusal::Busy)?;
    let (tx, rx) = mpsc::channel(BUFFER);
    let task = AbortOnDrop(tokio::spawn(drive(source, state, query, tx)));
    Ok(stream::unfold(
        (rx, task, permit),
        |(mut rx, task, permit)| async move { rx.recv().await.map(|event| (event, (rx, task, permit))) },
    ))
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// How reading stopped short.
enum Stop {
    /// The client went away.
    Closed,
    /// The logs could not be read; the message is for the client.
    Failed(String),
}

type Events = mpsc::Sender<LogEvent>;

async fn drive(source: SharedLogSource, state: State, query: LogQuery, tx: Events) {
    let error = match run(&source, &state, &query, &tx).await {
        Ok(()) => None,
        Err(Stop::Closed) => return,
        Err(Stop::Failed(message)) => Some(message),
    };
    let _ = tx.send(LogEvent::Done(LogsDone { error })).await;
}

async fn run(
    source: &SharedLogSource,
    state: &State,
    query: &LogQuery,
    tx: &Events,
) -> Result<(), Stop> {
    let targets = apps::find(&state.apps(), &query.app)
        .map(|app| targets(app, query.service.as_deref()))
        .ok_or_else(|| Stop::Failed(format!("the app {} is gone", query.app)))?;
    let endpoint = if targets.is_empty() {
        None
    } else {
        Some(endpoint(state).ok_or_else(|| Stop::Failed("Docker isn't running".to_owned()))?)
    };

    let backlogs = join_all(targets.iter().map(|target| {
        let source = source.clone();
        let endpoint = endpoint.clone().unwrap_or_default();
        async move { backlog(&source, &endpoint, target, query.tail.max(1)).await }
    }))
    .await;
    let mut cursors = HashMap::new();
    let mut sources = Vec::with_capacity(backlogs.len());
    for (target, backlog) in targets.iter().zip(backlogs) {
        let (entries, place) = backlog.map_err(|error| {
            Stop::Failed(format!(
                "couldn't read the logs of {}: {error}",
                target.name
            ))
        })?;
        cursors.insert(target.id.clone(), place);
        sources.push(entries);
    }
    let tail = usize::try_from(query.tail).unwrap_or(usize::MAX);
    let mut kept = Tail::new(tail, MAX_TAIL_BYTES);
    for entry in merge_tail(sources, tail, |entry| entry.at) {
        let size = entry.size();
        kept.push(entry, size);
    }
    for entry in kept.into_vec() {
        send(tx, LogEvent::Log(entry.line)).await?;
    }
    if !query.follow {
        return Ok(());
    }
    send(tx, LogEvent::Following(LogsFollowing {})).await?;
    follow_all(source, state, query, tx, targets, cursors).await
}

/// Follow every target, and every container that joins the app later,
/// until the client goes away.
async fn follow_all(
    source: &SharedLogSource,
    state: &State,
    query: &LogQuery,
    tx: &Events,
    initial: Vec<Target>,
    mut cursors: HashMap<String, Place>,
) -> Result<(), Stop> {
    let mut followers = JoinSet::new();
    let mut active = HashSet::new();
    let mut apps = state.watch_apps();
    let mut joined = initial;
    loop {
        for target in joined.drain(..) {
            if active.insert(target.id.clone()) {
                let cursor = cursors.remove(&target.id).unwrap_or_default();
                followers.spawn(follow(
                    source.clone(),
                    state.clone(),
                    query.app.clone(),
                    target,
                    cursor,
                    tx.clone(),
                ));
            }
        }
        tokio::select! {
            changed = apps.changed() => {
                if changed.is_err() {
                    return Err(Stop::Closed);
                }
                let list = apps.borrow_and_update().clone();
                if let Some(app) = apps::find(&list, &query.app) {
                    joined = targets(app, query.service.as_deref())
                        .into_iter()
                        .filter(|target| !active.contains(&target.id))
                        .collect();
                }
            }
            Some(done) = followers.join_next() => {
                if let Ok((id, cursor)) = done {
                    active.remove(&id);
                    cursors.insert(id, cursor);
                }
            }
            () = tx.closed() => return Err(Stop::Closed),
        }
    }
}

/// Follow one container, opening its log again each time it starts,
/// until it leaves the app or the client goes away. Returns where it
/// was left.
async fn follow(
    source: SharedLogSource,
    state: State,
    app: String,
    target: Target,
    mut cursor: Place,
    tx: Events,
) -> (String, Place) {
    loop {
        if let Some(endpoint) = endpoint(&state) {
            let request = LogRequest {
                follow: true,
                tail: None,
                since: cursor.since(),
            };
            let mut logs = source.logs(&endpoint, &target.id, request);
            cursor.resume();
            let mut reader = Reader::new(&target);
            let mut entries = Vec::new();
            while let Some(item) = logs.next().await {
                match item {
                    Ok(chunk) => entries.extend(reader.push(&chunk)),
                    Err(error) => {
                        tracing::debug!(%error, container = %target.name, "log stream failed");
                        break;
                    }
                }
                if !send_new(&tx, &mut cursor, entries.drain(..)).await {
                    return (target.id, cursor);
                }
            }
            if !send_new(&tx, &mut cursor, reader.finish().into_iter()).await {
                return (target.id, cursor);
            }
        }
        tokio::time::sleep(RESUME_DELAY).await;
        if !runs_again(&state, &app, &target.id).await {
            return (target.id, cursor);
        }
    }
}

async fn send_new(tx: &Events, cursor: &mut Place, entries: impl Iterator<Item = Entry>) -> bool {
    for entry in entries {
        if cursor.of(entry.line.stream).admit(entry.at)
            && tx.send(LogEvent::Log(entry.line)).await.is_err()
        {
            return false;
        }
    }
    true
}

/// Wait until container `id` of `app` is running. `false` once it is no
/// longer in the app.
async fn runs_again(state: &State, app: &str, id: &str) -> bool {
    let mut apps = state.watch_apps();
    loop {
        let container_state = {
            let list = apps.borrow_and_update();
            container_state(&list, app, id)
        };
        match container_state {
            None => return false,
            Some(ContainerState::Running) => return true,
            Some(_) => {}
        }
        if apps.changed().await.is_err() {
            return false;
        }
    }
}

fn container_state(apps: &AppList, app: &str, id: &str) -> Option<ContainerState> {
    apps::find(apps, app)?
        .services
        .iter()
        .flat_map(|service| &service.containers)
        .find(|container| container.id == id)
        .map(|container| container.state)
}

async fn send(tx: &Events, event: LogEvent) -> Result<(), Stop> {
    tx.send(event).await.map_err(|_| Stop::Closed)
}

/// The engine's endpoint, while it is ready.
fn endpoint(state: &State) -> Option<String> {
    let engine = state.engine();
    (engine.state == EngineState::Ready)
        .then_some(engine.endpoint)
        .flatten()
}

/// Where each output of one container's log was left.
#[derive(Debug, Clone, Copy, Default)]
struct Place {
    stdout: Cursor,
    stderr: Cursor,
}

impl Place {
    fn of(&mut self, stream: LogStream) -> &mut Cursor {
        match stream {
            LogStream::Stderr => &mut self.stderr,
            _ => &mut self.stdout,
        }
    }

    /// Start an output that sent nothing in the tail where the other was
    /// left, so resuming doesn't send it from the start of the log.
    fn seed(&mut self) {
        match (self.stdout.last(), self.stderr.last()) {
            (Some(at), None) => self.stderr = Cursor::starting_at(at),
            (None, Some(at)) => self.stdout = Cursor::starting_at(at),
            _ => {}
        }
    }

    /// The second to resume from: the earlier of the two outputs'.
    fn since(&self) -> Option<i64> {
        [self.stdout.since(), self.stderr.since()]
            .into_iter()
            .flatten()
            .min()
    }

    fn resume(&mut self) {
        self.stdout.resume();
        self.stderr.resume();
    }
}

/// One container whose log is read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    id: String,
    name: String,
    service: String,
}

/// Every container of `app`, or of its `service`, in service order.
fn targets(app: &App, service: Option<&str>) -> Vec<Target> {
    app.services
        .iter()
        .filter(|s| service.is_none_or(|name| s.name == name))
        .flat_map(|s| {
            s.containers.iter().map(|c| Target {
                id: c.id.clone(),
                name: c.name.clone(),
                service: s.name.clone(),
            })
        })
        .collect()
}

/// The last `tail` lines of `target`'s log, as many of the newest as fit
/// in [`MAX_TAIL_BYTES`], and where its outputs were left. Every line read
/// is noted in the place, kept or not, so following never sends one the
/// tail dropped. A container removed since the app list was read has
/// none.
async fn backlog(
    source: &SharedLogSource,
    endpoint: &str,
    target: &Target,
    tail: u32,
) -> Result<(Vec<Entry>, Place), LogError> {
    let request = LogRequest {
        follow: false,
        tail: Some(tail),
        since: None,
    };
    let mut logs = source.logs(endpoint, &target.id, request);
    let mut reader = Reader::new(target);
    let mut place = Place::default();
    let mut kept = Tail::new(usize::try_from(tail).unwrap_or(usize::MAX), MAX_TAIL_BYTES);
    let mut keep = |entries: Vec<Entry>| {
        for entry in entries {
            place.of(entry.line.stream).record(entry.at);
            let size = entry.size();
            kept.push(entry, size);
        }
    };
    while let Some(item) = logs.next().await {
        match item {
            Ok(chunk) => keep(reader.push(&chunk)),
            Err(error) if error.is_gone() => return Ok((Vec::new(), Place::default())),
            Err(error) => return Err(error),
        }
    }
    keep(reader.finish());
    place.seed();
    Ok((kept.into_vec(), place))
}

/// A line ready to send, with its timestamp for ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    at: Option<Timestamp>,
    line: LogLine,
}

impl Entry {
    /// The bytes of text it holds, as counted against [`MAX_TAIL_BYTES`].
    fn size(&self) -> usize {
        let line = &self.line;
        line.line.len()
            + line.time.as_ref().map_or(0, String::len)
            + line.container.len()
            + line.service.len()
    }
}

/// Turns one container's chunks into [`Entry`]s, keeping stdout and
/// stderr apart so a partial line on one is not joined to the other.
struct Reader<'a> {
    target: &'a Target,
    stdout: LineSplitter,
    stderr: LineSplitter,
}

impl<'a> Reader<'a> {
    fn new(target: &'a Target) -> Self {
        Self {
            target,
            stdout: LineSplitter::new(),
            stderr: LineSplitter::new(),
        }
    }

    fn push(&mut self, chunk: &LogChunk) -> Vec<Entry> {
        let lines = match chunk.stream {
            LogStream::Stderr => self.stderr.push(&chunk.bytes),
            _ => self.stdout.push(&chunk.bytes),
        };
        lines
            .into_iter()
            .map(|line| entry(self.target, chunk.stream, &line))
            .collect()
    }

    fn finish(&mut self) -> Vec<Entry> {
        let stdout = self.stdout.finish().map(|l| (LogStream::Stdout, l));
        let stderr = self.stderr.finish().map(|l| (LogStream::Stderr, l));
        stdout
            .into_iter()
            .chain(stderr)
            .map(|(stream, line)| entry(self.target, stream, &line))
            .collect()
    }
}

fn entry(target: &Target, stream: LogStream, raw: &str) -> Entry {
    let (time, line) = split_timestamp(raw);
    Entry {
        at: time.map(|(_, at)| at),
        line: LogLine {
            service: target.service.clone(),
            container: target.name.clone(),
            stream,
            time: time.map(|(text, _)| text.to_owned()),
            line: line.to_owned(),
        },
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
    use std::sync::Arc;

    use alchemy_api::apps::{AppContainer, AppService, StackName, StackStatus};
    use alchemy_core::engine::{EngineStatus, StopReason};

    use super::*;
    use crate::logs::FakeLogSource;

    const T1: &str = "2024-05-06T07:08:01.000000000Z";
    const T2: &str = "2024-05-06T07:08:02.000000000Z";
    const T3: &str = "2024-05-06T07:08:03.000000000Z";
    const T4: &str = "2024-05-06T07:08:04.000000000Z";

    fn container(id: &str, state: ContainerState) -> AppContainer {
        AppContainer {
            id: id.into(),
            name: format!("blog-{id}-1"),
            image: None,
            state,
            health: None,
            stop_reason: StopReason::Unknown,
        }
    }

    fn blog(services: &[(&str, &[(&str, ContainerState)])]) -> AppList {
        AppList {
            apps: vec![App {
                name: StackName::new("blog").unwrap(),
                status: StackStatus::Running,
                dir: "/srv/blog".into(),
                root: "/srv".into(),
                compose_file: "compose.yaml".into(),
                project: Some("blog".into()),
                images: None,
                services: services
                    .iter()
                    .map(|(name, containers)| AppService {
                        name: (*name).into(),
                        containers: containers
                            .iter()
                            .map(|(id, state)| container(id, *state))
                            .collect(),
                    })
                    .collect(),
                problems: vec![],
                stop_reason: StopReason::Unknown,
            }],
            ..AppList::default()
        }
    }

    const RUNNING: ContainerState = ContainerState::Running;

    fn ready_state(apps: AppList) -> State {
        let state = State::new();
        let mut engine = EngineStatus::new(EngineState::Ready);
        engine.endpoint = Some("unix:///fake.sock".into());
        state.set_engine(engine);
        state.set_apps(apps);
        state
    }

    fn query(service: Option<&str>, tail: u32, follow: bool) -> LogQuery {
        LogQuery {
            app: "blog".into(),
            service: service.map(str::to_owned),
            tail,
            follow,
        }
    }

    fn render(event: &LogEvent) -> String {
        match event {
            LogEvent::Log(line) => format!("{} {:?} {}", line.service, line.stream, line.line),
            LogEvent::Following(_) => "following".into(),
            LogEvent::Done(done) => format!("done {:?}", done.error),
        }
    }

    async fn collect(source: &FakeLogSource, state: &State, query: LogQuery) -> Vec<String> {
        let source: SharedLogSource = Arc::new(source.clone());
        stream(source, state.clone(), query)
            .unwrap()
            .map(|event| render(&event))
            .collect()
            .await
    }

    #[test]
    fn queries_are_checked_against_the_app() {
        let apps = blog(&[("web", &[]), ("db", &[])]);
        let ok = |service: Option<&str>, tail, follow| {
            Ok(LogQuery {
                app: "blog".into(),
                service: service.map(str::to_owned),
                tail,
                follow,
            })
        };
        let cases = [
            (("blog", None, None, None), ok(None, 200, false)),
            (
                ("blog", Some(""), Some("5"), Some("true")),
                ok(None, 5, true),
            ),
            (
                ("blog", Some("db"), Some("9000"), Some("0")),
                ok(Some("db"), 5000, false),
            ),
            (
                ("nope", None, None, None),
                Err(LogsRefusal::AppNotFound("nope".into())),
            ),
            (
                ("blog", Some("cache"), None, None),
                Err(LogsRefusal::ServiceNotFound {
                    app: "blog".into(),
                    service: "cache".into(),
                }),
            ),
            (
                ("blog", None, Some("x"), None),
                Err(LogsRefusal::Query(LogQueryError::Tail("x".into()))),
            ),
            (
                ("blog", None, None, Some("maybe")),
                Err(LogsRefusal::Query(LogQueryError::Follow("maybe".into()))),
            ),
        ];
        for ((app, service, tail, follow), want) in cases {
            assert_eq!(
                LogQuery::new(&apps, app, service, tail, follow),
                want,
                "{app} {service:?} {tail:?} {follow:?}"
            );
        }
    }

    #[test]
    fn targets_follow_service_order_and_the_filter() {
        let apps = blog(&[
            ("web", &[("w1", RUNNING), ("w2", RUNNING)]),
            ("db", &[("d1", RUNNING)]),
        ]);
        let app = &apps.apps[0];
        let ids =
            |service| -> Vec<String> { targets(app, service).into_iter().map(|t| t.id).collect() };
        assert_eq!(ids(None), ["w1", "w2", "d1"]);
        assert_eq!(ids(Some("db")), ["d1"]);
        assert_eq!(ids(Some("nope")), [] as [&str; 0]);
    }

    #[tokio::test]
    async fn the_tail_is_merged_in_time_order_and_cut() {
        let state = ready_state(blog(&[
            ("web", &[("w", RUNNING)]),
            ("db", &[("d", RUNNING)]),
        ]));
        let source = FakeLogSource::new();
        source.write("w", LogStream::Stdout, T1, "web one");
        source.write("d", LogStream::Stderr, T2, "db two");
        source.write("w", LogStream::Stdout, T3, "web three");
        source.write("d", LogStream::Stdout, T4, "db four");

        let cases = [
            (
                query(None, 200, false),
                vec![
                    "web Stdout web one",
                    "db Stderr db two",
                    "web Stdout web three",
                    "db Stdout db four",
                    "done None",
                ],
            ),
            (
                query(None, 2, false),
                vec!["web Stdout web three", "db Stdout db four", "done None"],
            ),
            (
                query(Some("db"), 200, false),
                vec!["db Stderr db two", "db Stdout db four", "done None"],
            ),
            (query(None, 0, false), vec!["done None"]),
        ];
        for (query, want) in cases {
            assert_eq!(
                collect(&source, &state, query.clone()).await,
                want,
                "{query:?}"
            );
        }
        assert!(
            source
                .requests()
                .iter()
                .all(|(_, r)| !r.follow && r.tail.is_some()),
            "{:?}",
            source.requests()
        );
    }

    #[tokio::test]
    async fn failures_end_with_an_error() {
        let source = FakeLogSource::new();
        source.write("w", LogStream::Stdout, T1, "hi");
        source.fail("d", 500);
        let apps = blog(&[("web", &[("w", RUNNING)]), ("db", &[("d", RUNNING)])]);
        let state = ready_state(apps.clone());
        let got = collect(&source, &state, query(None, 10, false)).await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            got[0].starts_with("done Some(\"couldn't read the logs of blog-d-1: "),
            "{got:?}"
        );

        let gone = ready_state(apps.clone());
        let fresh = FakeLogSource::new();
        fresh.write("w", LogStream::Stdout, T1, "hi");
        assert_eq!(
            collect(&fresh, &gone, query(None, 10, false)).await,
            ["web Stdout hi", "done None"],
            "a removed container has no lines"
        );

        let stopped = State::new();
        stopped.set_apps(apps);
        assert_eq!(
            collect(&source, &stopped, query(None, 10, true)).await,
            ["done Some(\"Docker isn't running\")"]
        );
    }

    #[tokio::test]
    async fn an_app_without_containers_has_no_lines() {
        let state = State::new();
        state.set_apps(blog(&[("web", &[])]));
        assert_eq!(
            collect(&FakeLogSource::new(), &state, query(None, 10, false)).await,
            ["done None"]
        );
    }

    async fn next(events: &mut (impl Stream<Item = LogEvent> + Unpin)) -> String {
        let event = tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .expect("timed out waiting for an event")
            .expect("stream ended");
        render(&event)
    }

    #[tokio::test]
    async fn following_sends_new_lines_and_survives_restarts() {
        let state = ready_state(blog(&[("web", &[("w", RUNNING)])]));
        let source = FakeLogSource::new();
        source.write("w", LogStream::Stdout, T1, "before");
        let shared: SharedLogSource = Arc::new(source.clone());
        let mut events = Box::pin(stream(shared, state.clone(), query(None, 10, true)).unwrap());

        assert_eq!(next(&mut events).await, "web Stdout before");
        assert_eq!(next(&mut events).await, "following");
        source.write("w", LogStream::Stderr, T2, "during");
        assert_eq!(next(&mut events).await, "web Stderr during");

        source.stop("w");
        state.set_apps(blog(&[("web", &[("w", ContainerState::Exited)])]));
        source.write("w", LogStream::Stdout, T3, "while stopped");
        source.start("w");
        state.set_apps(blog(&[("web", &[("w", RUNNING)])]));
        assert_eq!(next(&mut events).await, "web Stdout while stopped");
        source.write("w", LogStream::Stdout, T4, "after");
        assert_eq!(next(&mut events).await, "web Stdout after");

        state.set_apps(blog(&[("web", &[("w", RUNNING), ("n", RUNNING)])]));
        source.write("n", LogStream::Stdout, T4, "new container");
        assert_eq!(next(&mut events).await, "web Stdout new container");

        let resumed: Vec<_> = source
            .requests()
            .into_iter()
            .filter(|(id, r)| id == "w" && r.follow)
            .map(|(_, r)| r.since)
            .collect();
        assert_eq!(resumed.first(), Some(&Some(1_714_979_281)));
        assert!(resumed.len() >= 2, "{resumed:?}");

        drop(events);
        tokio::time::timeout(Duration::from_secs(5), async {
            while source.followers("w") + source.followers("n") > 0 {
                source.write("w", LogStream::Stdout, T4, "ping");
                source.write("n", LogStream::Stdout, T4, "ping");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("followers outlived the client");
    }

    #[tokio::test]
    async fn the_tail_of_zero_follows_from_now() {
        let state = ready_state(blog(&[("web", &[("w", RUNNING)])]));
        let source = FakeLogSource::new();
        source.write("w", LogStream::Stdout, T1, "old");
        let shared: SharedLogSource = Arc::new(source.clone());
        let mut events = Box::pin(stream(shared, state, query(None, 0, true)).unwrap());
        assert_eq!(next(&mut events).await, "following");
        source.write("w", LogStream::Stdout, T2, "new");
        assert_eq!(next(&mut events).await, "web Stdout new");
    }

    #[tokio::test]
    async fn outputs_out_of_order_with_each_other_are_all_sent_once() {
        let state = ready_state(blog(&[("web", &[("w", RUNNING)])]));
        let source = FakeLogSource::new();
        source.write("w", LogStream::Stdout, T2, "out two");
        let shared: SharedLogSource = Arc::new(source.clone());
        let mut events = Box::pin(stream(shared, state.clone(), query(None, 10, true)).unwrap());
        assert_eq!(next(&mut events).await, "web Stdout out two");
        assert_eq!(next(&mut events).await, "following");

        source.write("w", LogStream::Stdout, T4, "out four");
        source.write("w", LogStream::Stderr, T3, "err three");
        assert_eq!(next(&mut events).await, "web Stdout out four");
        assert_eq!(next(&mut events).await, "web Stderr err three");

        source.stop("w");
        state.set_apps(blog(&[("web", &[("w", ContainerState::Exited)])]));
        source.write("w", LogStream::Stderr, T4, "err four");
        source.start("w");
        state.set_apps(blog(&[("web", &[("w", RUNNING)])]));
        assert_eq!(next(&mut events).await, "web Stderr err four");
        source.write("w", LogStream::Stdout, T4, "out four again");
        assert_eq!(next(&mut events).await, "web Stdout out four again");
    }

    #[test]
    fn readers_keep_outputs_apart_and_parse_times() {
        let target = Target {
            id: "w".into(),
            name: "blog-web-1".into(),
            service: "web".into(),
        };
        let mut reader = Reader::new(&target);
        let chunk = |stream, bytes: &[u8]| LogChunk {
            stream,
            bytes: bytes.to_vec(),
        };
        let mut lines = reader.push(&chunk(LogStream::Stdout, format!("{T1} out ").as_bytes()));
        lines.extend(reader.push(&chunk(LogStream::Stderr, format!("{T2} err\n").as_bytes())));
        lines.extend(reader.push(&chunk(LogStream::Stdout, b"part\nbare \xff")));
        lines.extend(reader.finish());
        let got: Vec<_> = lines
            .iter()
            .map(|e| {
                (
                    e.line.stream,
                    e.line.time.as_deref(),
                    e.line.line.as_str(),
                    e.at.is_some(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (LogStream::Stderr, Some(T2), "err", true),
                (LogStream::Stdout, Some(T1), "out part", true),
                (LogStream::Stdout, None, "bare \u{fffd}", false),
            ]
        );
        assert!(
            lines
                .iter()
                .all(|e| e.line.container == "blog-web-1" && e.line.service == "web")
        );
    }

    /// Line `i` of a long log: a second apart, each at the line limit, its
    /// number first.
    fn long_line(i: usize) -> (String, String) {
        let secs = 8 * 60 + i;
        (
            format!("2024-05-06T07:{:02}:{:02}.000000000Z", secs / 60, secs % 60),
            format!("{i:05} {}", "x".repeat(alchemy_core::logs::MAX_LINE - 6)),
        )
    }

    #[tokio::test]
    async fn the_tail_keeps_only_the_newest_lines_that_fit_and_never_resends_the_rest() {
        let state = ready_state(blog(&[
            ("web", &[("w", RUNNING)]),
            ("db", &[("d", RUNNING)]),
        ]));
        let source = FakeLogSource::new();
        let lines = 400;
        for i in 0..lines {
            let (time, line) = long_line(i);
            let id = if i % 2 == 0 { "w" } else { "d" };
            source.write(id, LogStream::Stdout, &time, &line);
        }
        let shared: SharedLogSource = Arc::new(source.clone());
        let mut events = Box::pin(stream(shared, state, query(None, 5000, true)).unwrap());
        let mut sent = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.next())
                .await
                .unwrap()
                .unwrap()
            {
                LogEvent::Log(line) => sent.push(line),
                LogEvent::Following(_) => break,
                LogEvent::Done(done) => panic!("{done:?}"),
            }
        }
        let bytes: usize = sent
            .iter()
            .map(|l| {
                l.line.len()
                    + l.time.as_ref().map_or(0, String::len)
                    + l.container.len()
                    + l.service.len()
            })
            .sum();
        assert!(bytes <= MAX_TAIL_BYTES, "{bytes}");
        assert!(
            bytes > MAX_TAIL_BYTES - 2 * alchemy_core::logs::MAX_LINE,
            "{bytes}"
        );
        let numbers: Vec<usize> = sent.iter().map(|l| l.line[..5].parse().unwrap()).collect();
        let first = lines - numbers.len();
        assert_eq!(numbers, (first..lines).collect::<Vec<_>>());
        assert!(first > 0, "nothing was dropped");

        let (time, line) = long_line(lines);
        source.write("w", LogStream::Stdout, &time, &line);
        let next_line = next(&mut events).await;
        assert!(
            next_line.starts_with(&format!("web Stdout {lines:05} ")),
            "{}",
            &next_line[..20]
        );
    }

    #[tokio::test]
    async fn streams_beyond_the_limit_are_refused_until_one_closes() {
        let state = ready_state(blog(&[("web", &[("w", RUNNING)])]));
        let source = FakeLogSource::new();
        let shared: SharedLogSource = Arc::new(source.clone());
        let mut open: Vec<_> = (0..super::super::MAX_STREAMS)
            .map(|_| Box::pin(stream(shared.clone(), state.clone(), query(None, 0, true)).unwrap()))
            .collect();
        for events in &mut open {
            assert_eq!(next(events).await, "following");
        }
        assert!(matches!(
            stream(shared.clone(), state.clone(), query(None, 0, false)),
            Err(LogsRefusal::Busy)
        ));

        drop(open.pop());
        let mut events =
            Box::pin(stream(shared.clone(), state.clone(), query(None, 0, false)).unwrap());
        assert_eq!(next(&mut events).await, "done None");
        assert!(events.next().await.is_none());
        drop(events);
        assert!(stream(shared, state, query(None, 0, false)).is_ok());
    }
}
