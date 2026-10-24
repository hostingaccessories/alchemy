//! Pins the exact JSON bytes of every API type. A failure here means the
//! wire contract changed: make it additive or bump `API_VERSION`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use alchemy_api::apps::{
    App, AppContainer, AppDir, AppDirState, AppList, AppProblem, AppProblemKind, AppService,
    AppSettings, AppSettingsUpdate, ContainerState, Health, ImageSource, SkipReason, SkippedApp,
    StackName, StackStatus, UnmanagedApp,
};
use alchemy_api::operations::{
    Action, ActionPlan, ActionRequest, Operation, OperationEvent, OperationList, OperationResponse,
    OperationState, OutputLine, PlannedStep, Step, StepCommand, StepState,
};
use alchemy_api::session::{LaunchResponse, SessionAuth, SessionResponse};
use alchemy_api::{
    API_VERSION, ApiEvent, ContainerStatus, ContainerUsage, EngineFlavor, EngineState,
    EngineStatus, ErrorCode, ErrorResponse, HealthResponse, HealthStatus, MachineUsage,
    MetricsHistory, MetricsPoint, MetricsRange, MetricsResponse, ProjectUsage, StopReason,
    VersionResponse,
};

fn assert_wire<T>(value: &T, json: &str)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(serde_json::to_string(value).unwrap(), json);
    assert_eq!(&serde_json::from_str::<T>(json).unwrap(), value);
}

#[test]
fn api_version() {
    assert_eq!(API_VERSION, 1);
}

#[test]
fn health() {
    assert_wire(
        &HealthResponse {
            status: HealthStatus::Ok,
        },
        r#"{"status":"ok"}"#,
    );
}

#[test]
fn version() {
    assert_wire(
        &VersionResponse {
            version: "0.1.0".into(),
            api: 1,
        },
        r#"{"version":"0.1.0","api":1}"#,
    );
}

#[test]
fn error_response() {
    assert_wire(
        &ErrorResponse::new(ErrorCode::NotFound, "not found"),
        r#"{"error":"not found","code":"not_found"}"#,
    );
}

#[test]
fn error_codes() {
    let codes = [
        (ErrorCode::NotFound, r#""not_found""#),
        (ErrorCode::MethodNotAllowed, r#""method_not_allowed""#),
        (ErrorCode::BadRequest, r#""bad_request""#),
        (ErrorCode::Forbidden, r#""forbidden""#),
        (ErrorCode::Unauthorized, r#""unauthorized""#),
        (
            ErrorCode::UnsupportedMediaType,
            r#""unsupported_media_type""#,
        ),
        (ErrorCode::Conflict, r#""conflict""#),
        (ErrorCode::Invalid, r#""invalid""#),
        (ErrorCode::AppNotFound, r#""app_not_found""#),
        (ErrorCode::OperationNotFound, r#""operation_not_found""#),
        (ErrorCode::Busy, r#""busy""#),
        (ErrorCode::Internal, r#""internal""#),
        (ErrorCode::Unknown, r#""unknown""#),
    ];
    for (code, want) in codes {
        assert_wire(&code, want);
    }
}

#[test]
fn unknown_or_missing_error_code_decodes_as_unknown() {
    let cases = [
        r#"{"error":"x","code":"from_the_future"}"#,
        r#"{"error":"x"}"#,
    ];
    for json in cases {
        let got: ErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(got, ErrorResponse::new(ErrorCode::Unknown, "x"), "{json}");
    }
}

#[test]
fn engine_status() {
    let mut status = EngineStatus::new(EngineState::Ready);
    status.flavor = Some(EngineFlavor::DockerDesktop);
    status.engine_version = Some("29.8.1".into());
    status.api_version = Some("1.52".into());
    status.compose_version = Some("2.40.0".into());
    status.desktop_version = Some("4.93.0".into());
    status.endpoint = Some("unix:///var/run/docker.sock".into());
    assert_wire(
        &status,
        concat!(
            r#"{"state":"ready","flavor":"docker_desktop","engine_version":"29.8.1","#,
            r#""api_version":"1.52","compose_version":"2.40.0","desktop_version":"4.93.0","#,
            r#""endpoint":"unix:///var/run/docker.sock"}"#,
        ),
    );

    assert_wire(
        &EngineStatus::new(EngineState::NotRunning),
        concat!(
            r#"{"state":"not_running","flavor":null,"engine_version":null,"#,
            r#""api_version":null,"compose_version":null,"desktop_version":null,"#,
            r#""endpoint":null}"#,
        ),
    );
}

#[test]
fn container_status() {
    let mut status = ContainerStatus::new("abc", "blog-web-1", ContainerState::Exited);
    status.image = Some("nginx:1".into());
    status.project = Some("blog".into());
    status.service = Some("web".into());
    status.working_dir = Some("/srv/apps/blog".into());
    status.health = Some(Health::Unhealthy);
    status.stop_reason = StopReason::Crashed {
        exit_code: Some(137),
        oom: true,
    };
    assert_wire(
        &status,
        concat!(
            r#"{"id":"abc","name":"blog-web-1","image":"nginx:1","project":"blog","#,
            r#""service":"web","working_dir":"/srv/apps/blog","#,
            r#""state":"exited","health":"unhealthy","#,
            r#""stop_reason":{"kind":"crashed","exit_code":137,"oom":true}}"#,
        ),
    );
    let older: ContainerStatus =
        serde_json::from_str(r#"{"id":"abc","name":"web","state":"running"}"#).unwrap();
    assert_eq!(
        older.stop_reason,
        StopReason::Unknown,
        "a daemon without it"
    );
}

#[test]
fn stop_reasons() {
    let reasons = [
        (StopReason::Running, r#"{"kind":"running"}"#),
        (StopReason::Paused, r#"{"kind":"paused"}"#),
        (StopReason::NotStarted, r#"{"kind":"not_started"}"#),
        (
            StopReason::StoppedByAlchemy,
            r#"{"kind":"stopped_by_alchemy"}"#,
        ),
        (StopReason::StoppedOutside, r#"{"kind":"stopped_outside"}"#),
        (
            StopReason::Crashed {
                exit_code: None,
                oom: false,
            },
            r#"{"kind":"crashed","exit_code":null,"oom":false}"#,
        ),
        (
            StopReason::CrashLooping {
                exit_code: Some(1),
                oom: false,
            },
            r#"{"kind":"crash_looping","exit_code":1,"oom":false}"#,
        ),
        (StopReason::Finished, r#"{"kind":"finished"}"#),
        (StopReason::Unknown, r#"{"kind":"unknown"}"#),
    ];
    for (reason, want) in reasons {
        assert_wire(&reason, want);
    }
    let future: StopReason = serde_json::from_str(r#"{"kind":"from_the_future"}"#).unwrap();
    assert_eq!(future, StopReason::Unknown);
}

#[test]
fn container_states() {
    let states = [
        (ContainerState::Created, r#""created""#),
        (ContainerState::Running, r#""running""#),
        (ContainerState::Paused, r#""paused""#),
        (ContainerState::Restarting, r#""restarting""#),
        (ContainerState::Removing, r#""removing""#),
        (ContainerState::Exited, r#""exited""#),
        (ContainerState::Dead, r#""dead""#),
    ];
    for (state, want) in states {
        assert_wire(&state, want);
    }
}

#[test]
fn engine_states() {
    let states = [
        (EngineState::NotInstalled, r#""not_installed""#),
        (EngineState::NotRunning, r#""not_running""#),
        (EngineState::Starting, r#""starting""#),
        (EngineState::Ready, r#""ready""#),
        (EngineState::Unsupported, r#""unsupported""#),
    ];
    for (state, want) in states {
        assert_wire(&state, want);
    }
}

#[test]
fn engine_flavors() {
    let flavors = [
        (EngineFlavor::DockerDesktop, r#""docker_desktop""#),
        (EngineFlavor::DockerEngine, r#""docker_engine""#),
        (EngineFlavor::Other, r#""other""#),
    ];
    for (flavor, want) in flavors {
        assert_wire(&flavor, want);
    }
}

#[test]
fn api_event() {
    let event = ApiEvent::Engine(EngineStatus::new(EngineState::Starting));
    assert_eq!(event.name(), "engine");
    assert_wire(
        &event,
        concat!(
            r#"{"event":"engine","data":{"state":"starting","flavor":null,"#,
            r#""engine_version":null,"api_version":null,"compose_version":null,"#,
            r#""desktop_version":null,"endpoint":null}}"#,
        ),
    );
}

fn machine() -> MachineUsage {
    MachineUsage {
        cpu: Some(12.5),
        cpu_max: None,
        cpus: 8,
        load: Some([0.5, 0.25, 0.75]),
        memory_used: 4_000,
        memory_total: 16_000,
        swap_used: 0,
        swap_total: 2_000,
        disk_used: Some(100),
        disk_total: Some(1_000),
        disk_read: Some(1.5),
        disk_write: None,
        net_rx: Some(10.5),
        net_tx: Some(2.5),
    }
}

fn container() -> ContainerUsage {
    ContainerUsage {
        id: "abc".into(),
        name: "web-app-1".into(),
        project: Some("web".into()),
        service: Some("app".into()),
        cpu: Some(3.5),
        cpu_max: Some(9.5),
        memory: 500,
        memory_max: Some(600),
        net_rx: Some(1.5),
        net_tx: None,
        block_read: None,
        block_write: Some(0.5),
    }
}

fn project() -> ProjectUsage {
    ProjectUsage {
        name: "web".into(),
        containers: 1,
        cpu: Some(3.5),
        memory: 500,
        net_rx: Some(1.5),
        net_tx: None,
    }
}

const MACHINE_JSON: &str = concat!(
    r#"{"cpu":12.5,"cpu_max":null,"cpus":8,"load":[0.5,0.25,0.75],"#,
    r#""memory_used":4000,"memory_total":16000,"swap_used":0,"swap_total":2000,"#,
    r#""disk_used":100,"disk_total":1000,"disk_read":1.5,"disk_write":null,"#,
    r#""net_rx":10.5,"net_tx":2.5}"#,
);

const CONTAINER_JSON: &str = concat!(
    r#"{"id":"abc","name":"web-app-1","project":"web","service":"app","#,
    r#""cpu":3.5,"cpu_max":9.5,"memory":500,"memory_max":600,"#,
    r#""net_rx":1.5,"net_tx":null,"block_read":null,"block_write":0.5}"#,
);

const PROJECT_JSON: &str =
    r#"{"name":"web","containers":1,"cpu":3.5,"memory":500,"net_rx":1.5,"net_tx":null}"#;

#[test]
fn metrics_usage() {
    assert_wire(&machine(), MACHINE_JSON);
    assert_wire(&container(), CONTAINER_JSON);
    assert_wire(&project(), PROJECT_JSON);
}

#[test]
fn metrics_response() {
    assert_wire(
        &MetricsResponse {
            at: None,
            interval: 60,
            containers_in_vm: false,
            machine: None,
            projects: vec![],
            containers: vec![],
        },
        r#"{"at":null,"interval":60,"containers_in_vm":false,"machine":null,"projects":[],"containers":[]}"#,
    );
    assert_wire(
        &MetricsResponse {
            at: Some(1_700_000_000),
            interval: 60,
            containers_in_vm: true,
            machine: Some(machine()),
            projects: vec![project()],
            containers: vec![container()],
        },
        &format!(
            r#"{{"at":1700000000,"interval":60,"containers_in_vm":true,"machine":{MACHINE_JSON},"projects":[{PROJECT_JSON}],"containers":[{CONTAINER_JSON}]}}"#
        ),
    );
}

#[test]
fn metrics_history() {
    assert_wire(
        &MetricsHistory {
            range: MetricsRange::Day,
            step: 1200,
            project: Some("web".into()),
            points: vec![MetricsPoint {
                t: 1_200,
                machine: None,
                projects: vec![project()],
                containers: vec![container()],
            }],
        },
        &format!(
            r#"{{"range":"24h","step":1200,"project":"web","points":[{{"t":1200,"machine":null,"projects":[{PROJECT_JSON}],"containers":[{CONTAINER_JSON}]}}]}}"#
        ),
    );
}

#[test]
fn metrics_ranges() {
    let ranges = [
        (MetricsRange::Hour, r#""1h""#),
        (MetricsRange::HalfDay, r#""12h""#),
        (MetricsRange::Day, r#""24h""#),
        (MetricsRange::Week, r#""7d""#),
        (MetricsRange::Month, r#""30d""#),
    ];
    for (range, want) in ranges {
        assert_wire(&range, want);
    }
}

#[test]
fn metrics_event() {
    let event = ApiEvent::Metrics(MetricsResponse {
        at: Some(60),
        interval: 60,
        containers_in_vm: false,
        machine: None,
        projects: vec![],
        containers: vec![],
    });
    assert_eq!(event.name(), "metrics");
    assert_wire(
        &event,
        concat!(
            r#"{"event":"metrics","data":{"at":60,"interval":60,"containers_in_vm":false,"#,
            r#""machine":null,"projects":[],"containers":[]}}"#,
        ),
    );
    assert_eq!(
        serde_json::to_string(&event.data()).unwrap(),
        r#"{"at":60,"interval":60,"containers_in_vm":false,"machine":null,"projects":[],"containers":[]}"#
    );
}

fn blog() -> App {
    App {
        name: StackName::new("blog").unwrap(),
        status: StackStatus::Partial,
        dir: "/srv/apps/blog".into(),
        root: "/srv/apps".into(),
        compose_file: "compose.yaml".into(),
        project: Some("blog".into()),
        images: Some(ImageSource::Build),
        services: vec![
            AppService {
                name: "db".into(),
                containers: vec![AppContainer {
                    id: "abc".into(),
                    name: "blog-db-1".into(),
                    image: Some("postgres:16".into()),
                    state: ContainerState::Running,
                    health: Some(Health::Healthy),
                    stop_reason: StopReason::Running,
                }],
            },
            AppService {
                name: "web".into(),
                containers: vec![],
            },
        ],
        problems: vec![AppProblem {
            kind: AppProblemKind::SharedProject,
            message: "x".into(),
        }],
        stop_reason: StopReason::Crashed {
            exit_code: Some(1),
            oom: false,
        },
    }
}

const BLOG: &str = concat!(
    r#"{"name":"blog","status":"partial","dir":"/srv/apps/blog","root":"/srv/apps","#,
    r#""compose_file":"compose.yaml","project":"blog","images":"build","services":["#,
    r#"{"name":"db","containers":[{"id":"abc","name":"blog-db-1","image":"postgres:16","#,
    r#""state":"running","health":"healthy","stop_reason":{"kind":"running"}}]},"#,
    r#"{"name":"web","containers":[]}],"problems":[{"kind":"shared_project","message":"x"}],"#,
    r#""stop_reason":{"kind":"crashed","exit_code":1,"oom":false}}"#,
);

#[test]
fn app() {
    assert_wire(&blog(), BLOG);

    let unread = App {
        name: StackName::new("new").unwrap(),
        status: StackStatus::Unknown,
        dir: "/srv/apps/new".into(),
        root: "/srv/apps".into(),
        compose_file: "docker-compose.yml".into(),
        project: None,
        images: None,
        services: vec![],
        problems: vec![],
        stop_reason: StopReason::Unknown,
    };
    let json = concat!(
        r#"{"name":"new","status":"unknown","dir":"/srv/apps/new","root":"/srv/apps","#,
        r#""compose_file":"docker-compose.yml","project":null,"images":null,"#,
        r#""services":[],"problems":[]"#,
    );
    assert_wire(
        &unread,
        &format!(r#"{json},"stop_reason":{{"kind":"unknown"}}}}"#),
    );
    let older: App = serde_json::from_str(&format!("{json}}}")).unwrap();
    assert_eq!(older, unread, "a daemon without stop reasons");
}

#[test]
fn app_names_are_validated_on_decode() {
    let bad = BLOG.replace(r#""name":"blog""#, r#""name":"../etc""#);
    assert!(serde_json::from_str::<App>(&bad).is_err());
}

fn app_list() -> AppList {
    AppList {
        apps: vec![blog()],
        unmanaged: vec![UnmanagedApp {
            project: "legacy".into(),
            dir: Some("/home/me/legacy".into()),
            status: StackStatus::Running,
            services: vec![],
            stop_reason: StopReason::Running,
        }],
        dirs: vec![
            AppDir {
                path: "/srv/apps".into(),
                home: true,
                state: AppDirState::Ok,
                message: None,
                skipped: vec![SkippedApp {
                    name: "My App".into(),
                    path: "/srv/apps/My App".into(),
                    reason: SkipReason::InvalidName,
                    message: "y".into(),
                }],
            },
            AppDir {
                path: "/opt/stacks".into(),
                home: false,
                state: AppDirState::Missing,
                message: Some("z".into()),
                skipped: vec![],
            },
        ],
    }
}

const APP_LIST_TAIL: &str = concat!(
    r#""unmanaged":[{"project":"legacy","dir":"/home/me/legacy","status":"running","#,
    r#""services":[],"stop_reason":{"kind":"running"}}],"dirs":[{"path":"/srv/apps","home":true,"state":"ok","message":null,"#,
    r#""skipped":[{"name":"My App","path":"/srv/apps/My App","reason":"invalid_name","#,
    r#""message":"y"}]},{"path":"/opt/stacks","home":false,"state":"missing","#,
    r#""message":"z","skipped":[]}]}"#,
);

#[test]
fn app_list_and_event() {
    let json = format!(r#"{{"apps":[{BLOG}],{APP_LIST_TAIL}"#);
    assert_wire(&app_list(), &json);
    assert_wire(
        &AppList::default(),
        r#"{"apps":[],"unmanaged":[],"dirs":[]}"#,
    );

    let event = ApiEvent::Apps(app_list());
    assert_eq!(event.name(), "apps");
    assert_wire(&event, &format!(r#"{{"event":"apps","data":{json}}}"#));
    assert_eq!(serde_json::to_string(&event.data()).unwrap(), json);
}

#[test]
fn unmanaged_app() {
    let json = r#"{"project":"legacy","dir":null,"status":"stopped","services":[]"#;
    let crashed = UnmanagedApp {
        project: "legacy".into(),
        dir: None,
        status: StackStatus::Stopped,
        services: vec![],
        stop_reason: StopReason::Crashed {
            exit_code: Some(1),
            oom: false,
        },
    };
    assert_wire(
        &crashed,
        &format!(r#"{json},"stop_reason":{{"kind":"crashed","exit_code":1,"oom":false}}}}"#),
    );
    let older: UnmanagedApp = serde_json::from_str(&format!("{json}}}")).unwrap();
    assert_eq!(
        older,
        UnmanagedApp {
            stop_reason: StopReason::Unknown,
            ..crashed
        },
        "a daemon without stop reasons"
    );
}

#[test]
fn app_enums() {
    for (status, want) in [
        (StackStatus::Running, r#""running""#),
        (StackStatus::Partial, r#""partial""#),
        (StackStatus::Stopped, r#""stopped""#),
        (StackStatus::Unknown, r#""unknown""#),
    ] {
        assert_wire(&status, want);
    }
    for (images, want) in [
        (ImageSource::Registry, r#""registry""#),
        (ImageSource::Build, r#""build""#),
    ] {
        assert_wire(&images, want);
    }
    for (kind, want) in [
        (AppProblemKind::ComposeError, r#""compose_error""#),
        (
            AppProblemKind::ComposeUnavailable,
            r#""compose_unavailable""#,
        ),
        (AppProblemKind::SharedProject, r#""shared_project""#),
        (AppProblemKind::Other, r#""other""#),
    ] {
        assert_wire(&kind, want);
    }
    for (state, want) in [
        (AppDirState::Ok, r#""ok""#),
        (AppDirState::Missing, r#""missing""#),
        (AppDirState::NotADirectory, r#""not_a_directory""#),
        (AppDirState::Unreadable, r#""unreadable""#),
        (AppDirState::Other, r#""other""#),
    ] {
        assert_wire(&state, want);
    }
    for (reason, want) in [
        (SkipReason::InvalidName, r#""invalid_name""#),
        (SkipReason::Duplicate, r#""duplicate""#),
        (SkipReason::LinkedComposeFile, r#""linked_compose_file""#),
        (
            SkipReason::ComposeFileNotRegular,
            r#""compose_file_not_regular""#,
        ),
        (SkipReason::LinkedFolder, r#""linked_folder""#),
        (SkipReason::Other, r#""other""#),
    ] {
        assert_wire(&reason, want);
    }
}

#[test]
fn unknown_app_enum_values_decode_as_a_fallback() {
    let from_the_future = r#""from_the_future""#;
    assert_eq!(
        serde_json::from_str::<StackStatus>(from_the_future).unwrap(),
        StackStatus::Unknown
    );
    assert_eq!(
        serde_json::from_str::<AppProblemKind>(from_the_future).unwrap(),
        AppProblemKind::Other
    );
    assert_eq!(
        serde_json::from_str::<AppDirState>(from_the_future).unwrap(),
        AppDirState::Other
    );
    assert_eq!(
        serde_json::from_str::<SkipReason>(from_the_future).unwrap(),
        SkipReason::Other
    );
}

#[test]
fn app_settings() {
    assert_wire(
        &AppSettings {
            home: Some("/home/me/Alchemy/apps".into()),
            default_home: Some("/home/me/Alchemy/apps".into()),
            extra: vec!["/opt/stacks".into()],
        },
        r#"{"home":"/home/me/Alchemy/apps","default_home":"/home/me/Alchemy/apps","extra":["/opt/stacks"]}"#,
    );
    assert_wire(
        &AppSettings {
            home: None,
            default_home: None,
            extra: vec![],
        },
        r#"{"home":null,"default_home":null,"extra":[]}"#,
    );
    assert_wire(
        &AppSettingsUpdate {
            home: Some("/srv/apps".into()),
            extra: vec!["/opt/stacks".into()],
        },
        r#"{"home":"/srv/apps","extra":["/opt/stacks"]}"#,
    );
    assert_eq!(
        serde_json::from_str::<AppSettingsUpdate>("{}").unwrap(),
        AppSettingsUpdate {
            home: None,
            extra: vec![],
        }
    );
}

#[test]
fn log_events() {
    use alchemy_api::logs::{LogEvent, LogLine, LogStream, LogsDone, LogsFollowing};
    let line = LogLine {
        service: "web".into(),
        container: "blog-web-1".into(),
        stream: LogStream::Stderr,
        time: Some("2024-05-06T07:08:09.123456789Z".into()),
        line: "GET / 200".into(),
    };
    assert_wire(
        &line,
        r#"{"service":"web","container":"blog-web-1","stream":"stderr","time":"2024-05-06T07:08:09.123456789Z","line":"GET / 200"}"#,
    );
    assert_wire(
        &LogLine {
            stream: LogStream::Stdout,
            time: None,
            ..line.clone()
        },
        r#"{"service":"web","container":"blog-web-1","stream":"stdout","time":null,"line":"GET / 200"}"#,
    );
    assert_eq!(
        serde_json::from_str::<LogStream>(r#""console""#).unwrap(),
        LogStream::Other
    );
    assert_wire(&LogsFollowing {}, "{}");
    assert_wire(&LogsDone { error: None }, r#"{"error":null}"#);
    assert_wire(
        &LogsDone {
            error: Some("Docker isn't running".into()),
        },
        r#"{"error":"Docker isn't running"}"#,
    );

    let cases = [
        (LogEvent::Log(line.clone()), "log"),
        (LogEvent::Following(LogsFollowing {}), "following"),
        (LogEvent::Done(LogsDone::default()), "done"),
    ];
    for (event, name) in cases {
        assert_eq!(event.name(), name);
        let data = serde_json::to_value(event.data()).unwrap();
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::json!({ "event": name, "data": data })
        );
    }
}
fn update() -> Operation {
    Operation {
        id: "19a3b4c5d6e-1".into(),
        app: StackName::new("blog").unwrap(),
        action: Action::Update,
        state: OperationState::Running,
        steps: vec![
            Step {
                command: StepCommand::Pull,
                label: "Downloading updates".into(),
                state: StepState::Done,
                message: Some("2 of 2 images downloaded".into()),
            },
            Step {
                command: StepCommand::Up,
                label: "Starting the new version".into(),
                state: StepState::Active,
                message: None,
            },
        ],
        started_at: 1_700_000_000,
        finished_at: None,
        error: None,
    }
}

const UPDATE: &str = concat!(
    r#"{"id":"19a3b4c5d6e-1","app":"blog","action":"update","state":"running","steps":["#,
    r#"{"command":"pull","label":"Downloading updates","state":"done","message":"2 of 2 images downloaded"},"#,
    r#"{"command":"up","label":"Starting the new version","state":"active","message":null}],"#,
    r#""started_at":1700000000,"finished_at":null,"error":null}"#,
);

#[test]
fn operation() {
    assert_wire(&update(), UPDATE);
    let failed = Operation {
        state: OperationState::Failed,
        finished_at: Some(1_700_000_030),
        error: Some("pull access denied".into()),
        steps: vec![],
        ..update()
    };
    assert_wire(
        &failed,
        concat!(
            r#"{"id":"19a3b4c5d6e-1","app":"blog","action":"update","state":"failed","steps":[],"#,
            r#""started_at":1700000000,"finished_at":1700000030,"error":"pull access denied"}"#,
        ),
    );
    assert_wire(
        &OperationResponse {
            operation: update(),
        },
        &format!(r#"{{"operation":{UPDATE}}}"#),
    );
    assert_wire(
        &OperationList {
            operations: vec![update()],
        },
        &format!(r#"{{"operations":[{UPDATE}]}}"#),
    );
}

#[test]
fn operation_enums() {
    for (action, want) in [
        (Action::Up, r#""up""#),
        (Action::Down, r#""down""#),
        (Action::Start, r#""start""#),
        (Action::Stop, r#""stop""#),
        (Action::Restart, r#""restart""#),
        (Action::Pull, r#""pull""#),
        (Action::Build, r#""build""#),
        (Action::Update, r#""update""#),
    ] {
        assert_wire(&action, want);
    }
    for (command, want) in [
        (StepCommand::Up, r#""up""#),
        (StepCommand::Down, r#""down""#),
        (StepCommand::Start, r#""start""#),
        (StepCommand::Stop, r#""stop""#),
        (StepCommand::Restart, r#""restart""#),
        (StepCommand::Pull, r#""pull""#),
        (StepCommand::Build, r#""build""#),
    ] {
        assert_wire(&command, want);
    }
    for (state, want) in [
        (OperationState::Running, r#""running""#),
        (OperationState::Succeeded, r#""succeeded""#),
        (OperationState::Failed, r#""failed""#),
        (OperationState::Cancelled, r#""cancelled""#),
        (OperationState::Interrupted, r#""interrupted""#),
        (OperationState::Unknown, r#""unknown""#),
    ] {
        assert_wire(&state, want);
    }
    for (state, want) in [
        (StepState::Pending, r#""pending""#),
        (StepState::Active, r#""active""#),
        (StepState::Done, r#""done""#),
        (StepState::Failed, r#""failed""#),
        (StepState::Skipped, r#""skipped""#),
        (StepState::Unknown, r#""unknown""#),
    ] {
        assert_wire(&state, want);
    }
    let from_the_future = r#""from_the_future""#;
    assert_eq!(
        serde_json::from_str::<OperationState>(from_the_future).unwrap(),
        OperationState::Unknown
    );
    assert_eq!(
        serde_json::from_str::<StepState>(from_the_future).unwrap(),
        StepState::Unknown
    );
    assert!(serde_json::from_str::<Action>(from_the_future).is_err());
}

#[test]
fn action_request() {
    assert_wire(
        &ActionRequest {
            action: Action::Down,
            force: true,
        },
        r#"{"action":"down","force":true}"#,
    );
    assert_eq!(
        serde_json::from_str::<ActionRequest>(r#"{"action":"up"}"#).unwrap(),
        ActionRequest {
            action: Action::Up,
            force: false,
        }
    );
    assert!(serde_json::from_str::<ActionRequest>(r#"{"action":"kill"}"#).is_err());
}

#[test]
fn action_plan() {
    assert_wire(
        &ActionPlan {
            app: StackName::new("blog").unwrap(),
            action: Action::Down,
            steps: vec![PlannedStep {
                command: StepCommand::Down,
                label: "Removing the app's containers".into(),
                argv: vec![
                    "docker".into(),
                    "compose".into(),
                    "--project-directory".into(),
                    "/srv/apps/blog".into(),
                    "--progress".into(),
                    "json".into(),
                    "down".into(),
                ],
            }],
        },
        concat!(
            r#"{"app":"blog","action":"down","steps":[{"command":"down","#,
            r#""label":"Removing the app's containers","argv":["docker","compose","#,
            r#""--project-directory","/srv/apps/blog","--progress","json","down"]}]}"#,
        ),
    );
}

#[test]
fn operation_events() {
    let line = OutputLine {
        seq: 7,
        step: 1,
        text: "Container blog-web-1 Started".into(),
    };
    let line_json = r#"{"seq":7,"step":1,"text":"Container blog-web-1 Started"}"#;
    let cases = [
        (
            OperationEvent::Operation(update()),
            "operation",
            UPDATE.to_owned(),
        ),
        (OperationEvent::Output(line), "output", line_json.to_owned()),
        (OperationEvent::Done(update()), "done", UPDATE.to_owned()),
    ];
    for (event, name, data) in cases {
        assert_eq!(event.name(), name);
        assert_eq!(serde_json::to_string(&event.data()).unwrap(), data);
        assert_wire(&event, &format!(r#"{{"event":"{name}","data":{data}}}"#));
    }
}

#[test]
fn operations_event() {
    let event = ApiEvent::Operations(OperationList {
        operations: vec![update()],
    });
    assert_eq!(event.name(), "operations");
    assert_wire(
        &event,
        &format!(r#"{{"event":"operations","data":{{"operations":[{UPDATE}]}}}}"#),
    );
}

#[test]
fn new_app() {
    use alchemy_api::apps::files::NewApp;
    assert_wire(
        &NewApp {
            name: "blog".into(),
            folder: Some("/opt/stacks".into()),
            compose: "services: {}\n".into(),
            env: Some("A=1\n".into()),
            dry_run: true,
        },
        r#"{"name":"blog","folder":"/opt/stacks","compose":"services: {}\n","env":"A=1\n","dry_run":true}"#,
    );
    assert_eq!(
        serde_json::from_str::<NewApp>(r#"{"name":"blog","compose":"x"}"#).unwrap(),
        NewApp {
            name: "blog".into(),
            folder: None,
            compose: "x".into(),
            env: None,
            dry_run: false,
        }
    );
}

#[test]
fn app_files() {
    use alchemy_api::apps::files::{AppFile, AppFileKind, AppFileUpdate};
    assert_wire(&AppFileKind::Compose, r#""compose""#);
    assert_wire(&AppFileKind::Env, r#""env""#);
    assert_wire(
        &AppFile {
            kind: AppFileKind::Compose,
            name: "compose.yaml".into(),
            content: "services: {}\n".into(),
            hash: Some("ab12".into()),
        },
        r#"{"kind":"compose","name":"compose.yaml","content":"services: {}\n","hash":"ab12"}"#,
    );
    assert_wire(
        &AppFile {
            kind: AppFileKind::Env,
            name: ".env".into(),
            content: String::new(),
            hash: None,
        },
        r#"{"kind":"env","name":".env","content":"","hash":null}"#,
    );
    assert_wire(
        &AppFileUpdate {
            content: "A=2\n".into(),
            base_hash: Some("ab12".into()),
            dry_run: false,
        },
        r#"{"content":"A=2\n","base_hash":"ab12","dry_run":false}"#,
    );
    assert_eq!(
        serde_json::from_str::<AppFileUpdate>(r#"{"content":""}"#).unwrap(),
        AppFileUpdate {
            content: String::new(),
            base_hash: None,
            dry_run: false,
        }
    );
}

#[test]
fn launch_response() {
    assert_wire(
        &LaunchResponse {
            code: "c0de".into(),
            url: "http://127.0.0.1:7240/login?code=c0de".into(),
            expires_in: 60,
        },
        r#"{"code":"c0de","url":"http://127.0.0.1:7240/login?code=c0de","expires_in":60}"#,
    );
}

#[test]
fn session_response() {
    for (auth, json) in [
        (SessionAuth::Socket, r#"{"auth":"socket"}"#),
        (SessionAuth::Token, r#"{"auth":"token"}"#),
        (SessionAuth::Cookie, r#"{"auth":"cookie"}"#),
        (SessionAuth::Unknown, r#"{"auth":"unknown"}"#),
    ] {
        assert_wire(&SessionResponse { auth }, json);
    }
    let future: SessionResponse = serde_json::from_str(r#"{"auth":"passkey"}"#).unwrap();
    assert_eq!(future.auth, SessionAuth::Unknown);
}
