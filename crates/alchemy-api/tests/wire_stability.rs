//! Pins the exact JSON bytes of every API type. A failure here means the
//! wire contract changed: make it additive or bump `API_VERSION`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use alchemy_api::apps::{
    AddressKind, AddressState, App, AppAddress, AppContainer, AppDir, AppDirState, AppList,
    AppProblem, AppProblemKind, AppService, AppSettings, AppSettingsUpdate, ContainerState, Health,
    ImageSource, SkipReason, SkippedApp, StackName, StackStatus, UnmanagedApp,
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
fn container_status_ports() {
    use alchemy_api::{Protocol, PublishedPort};
    let mut status = ContainerStatus::new("abc", "blog-web-1", ContainerState::Running);
    status.stop_reason = StopReason::Running;
    status.ports = vec![
        PublishedPort::new(Protocol::Tcp, Some("8080".parse().unwrap()))
            .with_target(Some("80".parse().unwrap())),
        PublishedPort::new(Protocol::Udp, Some("5353".parse().unwrap()))
            .with_host_ip(Some("127.0.0.1".parse().unwrap()))
            .with_target(Some("53".parse().unwrap())),
    ];
    assert_wire(
        &status,
        concat!(
            r#"{"id":"abc","name":"blog-web-1","image":null,"project":null,"#,
            r#""service":null,"working_dir":null,"state":"running","health":null,"#,
            r#""stop_reason":{"kind":"running"},"ports":["#,
            r#"{"protocol":"tcp","host_ip":null,"published":"8080","target":"80"},"#,
            r#"{"protocol":"udp","host_ip":"127.0.0.1","published":"5353","target":"53"}]}"#,
        ),
    );
}

#[test]
fn container_status_networks() {
    let mut status = ContainerStatus::new("abc", "blog-web-1", ContainerState::Running);
    status.stop_reason = StopReason::Running;
    status
        .networks
        .insert("blog_default".into(), "172.18.0.2".parse().unwrap());
    status
        .networks
        .insert("alchemy".into(), "10.200.0.3".parse().unwrap());
    assert_wire(
        &status,
        concat!(
            r#"{"id":"abc","name":"blog-web-1","image":null,"project":null,"#,
            r#""service":null,"working_dir":null,"state":"running","health":null,"#,
            r#""stop_reason":{"kind":"running"},"#,
            r#""networks":{"alchemy":"10.200.0.3","blog_default":"172.18.0.2"}}"#,
        ),
    );
}

#[test]
fn port_list() {
    use alchemy_api::ports::{PortClaim, PortHolder, PortList, PortSource};
    use alchemy_api::{Protocol, PublishedPort};
    let tcp = |port: &str| PublishedPort::new(Protocol::Tcp, Some(port.parse().unwrap()));
    let list = PortList {
        ports: vec![
            PortClaim {
                port: tcp("80").with_target(Some("8080".parse().unwrap())),
                holder: PortHolder::App {
                    app: "blog".into(),
                    service: "web".into(),
                },
                source: PortSource::Live,
            },
            PortClaim {
                port: tcp("3000"),
                holder: PortHolder::App {
                    app: "wiki".into(),
                    service: "app".into(),
                },
                source: PortSource::Declared,
            },
            PortClaim {
                port: tcp("5000"),
                holder: PortHolder::Container {
                    name: "pihole".into(),
                },
                source: PortSource::Live,
            },
            PortClaim {
                port: tcp("8443").with_host_ip(Some("127.0.0.1".parse().unwrap())),
                holder: PortHolder::Reservation {
                    owner: "proxy".into(),
                },
                source: PortSource::Reserved,
            },
            PortClaim {
                port: tcp("9000"),
                holder: PortHolder::Program {
                    process: Some("nc".into()),
                    pid: Some(42),
                },
                source: PortSource::Live,
            },
            PortClaim {
                port: PublishedPort::new(Protocol::Udp, Some("53".parse().unwrap()))
                    .with_host_ip(Some("::".parse().unwrap())),
                holder: PortHolder::Program {
                    process: None,
                    pid: None,
                },
                source: PortSource::Live,
            },
        ],
        warnings: vec!["alchemyd can't tell which programs are listening".into()],
    };
    assert_wire(
        &list,
        concat!(
            r#"{"ports":["#,
            r#"{"port":{"protocol":"tcp","host_ip":null,"published":"80","target":"8080"},"#,
            r#""holder":{"kind":"app","app":"blog","service":"web"},"source":"live"},"#,
            r#"{"port":{"protocol":"tcp","host_ip":null,"published":"3000","target":null},"#,
            r#""holder":{"kind":"app","app":"wiki","service":"app"},"source":"declared"},"#,
            r#"{"port":{"protocol":"tcp","host_ip":null,"published":"5000","target":null},"#,
            r#""holder":{"kind":"container","name":"pihole"},"source":"live"},"#,
            r#"{"port":{"protocol":"tcp","host_ip":"127.0.0.1","published":"8443","target":null},"#,
            r#""holder":{"kind":"reservation","owner":"proxy"},"source":"reserved"},"#,
            r#"{"port":{"protocol":"tcp","host_ip":null,"published":"9000","target":null},"#,
            r#""holder":{"kind":"program","process":"nc","pid":42},"source":"live"},"#,
            r#"{"port":{"protocol":"udp","host_ip":"::","published":"53","target":null},"#,
            r#""holder":{"kind":"program","process":null,"pid":null},"source":"live"}],"#,
            r#""warnings":["alchemyd can't tell which programs are listening"]}"#,
        ),
    );
    let bare: PortList = serde_json::from_str(r#"{"ports":[]}"#).unwrap();
    assert_eq!(bare, PortList::default(), "warnings default to none");
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
        addresses: vec![],
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
        addresses: vec![],
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
fn app_addresses() {
    let mut app = blog();
    app.addresses = vec![
        AppAddress {
            kind: AddressKind::Host,
            url: "http://blog.localhost/".into(),
            state: AddressState::Unavailable,
            service: "web".into(),
            port: 80,
            message: Some("m".into()),
        },
        AppAddress {
            kind: AddressKind::Port,
            url: "http://192.168.1.20:8100/".into(),
            state: AddressState::Ready,
            service: "web".into(),
            port: 80,
            message: None,
        },
    ];
    let json = format!(
        "{},{}}}",
        BLOG.strip_suffix('}').unwrap(),
        concat!(
            r#""addresses":[{"kind":"host","url":"http://blog.localhost/","state":"unavailable","#,
            r#""service":"web","port":80,"message":"m"},{"kind":"port","#,
            r#""url":"http://192.168.1.20:8100/","state":"ready","service":"web","port":80,"#,
            r#""message":null}]"#,
        )
    );
    assert_wire(&app, &json);
}

#[test]
fn address_enums() {
    for (kind, want) in [
        (AddressKind::Host, r#""host""#),
        (AddressKind::Port, r#""port""#),
        (AddressKind::Other, r#""other""#),
    ] {
        assert_wire(&kind, want);
    }
    for (state, want) in [
        (AddressState::Ready, r#""ready""#),
        (AddressState::Pending, r#""pending""#),
        (AddressState::NotRunning, r#""not_running""#),
        (AddressState::Unavailable, r#""unavailable""#),
        (AddressState::Other, r#""other""#),
    ] {
        assert_wire(&state, want);
    }
    let future: AddressKind = serde_json::from_str(r#""mdns""#).unwrap();
    assert_eq!(future, AddressKind::Other);
    let future: AddressState = serde_json::from_str(r#""sleeping""#).unwrap();
    assert_eq!(future, AddressState::Other);
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
            conflicts: Vec::new(),
        },
        concat!(
            r#"{"app":"blog","action":"down","steps":[{"command":"down","#,
            r#""label":"Removing the app's containers","argv":["docker","compose","#,
            r#""--project-directory","/srv/apps/blog","--progress","json","down"]}]}"#,
        ),
    );
}

#[test]
fn action_plan_with_port_conflicts() {
    use alchemy_api::ports::{PortClaim, PortConflict, PortHolder, PortSource};
    use alchemy_api::{Protocol, PublishedPort};
    let tcp = PublishedPort::new(Protocol::Tcp, Some("8080".parse().unwrap()));
    assert_wire(
        &ActionPlan {
            app: StackName::new("wiki").unwrap(),
            action: Action::Up,
            steps: vec![PlannedStep {
                command: StepCommand::Up,
                label: "Starting the app".into(),
                argv: vec!["docker".into(), "compose".into(), "up".into()],
            }],
            conflicts: vec![PortConflict {
                service: "app".into(),
                wanted: tcp.with_target(Some("80".parse().unwrap())),
                claim: PortClaim {
                    port: tcp,
                    holder: PortHolder::App {
                        app: "blog".into(),
                        service: "web".into(),
                    },
                    source: PortSource::Live,
                },
                message: "Port 8080/tcp for service app is in use by service web of app blog."
                    .into(),
            }],
        },
        concat!(
            r#"{"app":"wiki","action":"up","steps":[{"command":"up","#,
            r#""label":"Starting the app","argv":["docker","compose","up"]}],"#,
            r#""conflicts":[{"service":"app","#,
            r#""wanted":{"protocol":"tcp","host_ip":null,"published":"8080","target":"80"},"#,
            r#""claim":{"port":{"protocol":"tcp","host_ip":null,"published":"8080","target":null},"#,
            r#""holder":{"kind":"app","app":"blog","service":"web"},"source":"live"},"#,
            r#""message":"Port 8080/tcp for service app is in use by service web of app blog."}]}"#,
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
            start: false,
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
            start: false,
        }
    );
    assert_wire(
        &NewApp {
            name: "blog".into(),
            folder: None,
            compose: "x".into(),
            env: None,
            dry_run: false,
            start: true,
        },
        r#"{"name":"blog","folder":null,"compose":"x","env":null,"dry_run":false,"start":true}"#,
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
            start: false,
        },
        r#"{"content":"A=2\n","base_hash":"ab12","dry_run":false}"#,
    );
    assert_eq!(
        serde_json::from_str::<AppFileUpdate>(r#"{"content":""}"#).unwrap(),
        AppFileUpdate {
            content: String::new(),
            base_hash: None,
            dry_run: false,
            start: false,
        }
    );
    assert_wire(
        &AppFileUpdate {
            content: "A=2\n".into(),
            base_hash: None,
            dry_run: false,
            start: true,
        },
        r#"{"content":"A=2\n","base_hash":null,"dry_run":false,"start":true}"#,
    );
}

#[test]
fn saved() {
    use alchemy_api::apps::files::{AppFile, AppFileKind, Saved};
    let file = AppFile {
        kind: AppFileKind::Env,
        name: ".env".into(),
        content: "A=1\n".into(),
        hash: Some("ab12".into()),
    };
    assert_wire(
        &Saved::new(file.clone()),
        r#"{"kind":"env","name":".env","content":"A=1\n","hash":"ab12"}"#,
    );
    assert_wire(&Saved::new(blog()), BLOG);
    assert_wire(
        &Saved {
            saved: file,
            operation: Some(update()),
            start_error: None,
        },
        &[
            r#"{"kind":"env","name":".env","content":"A=1\n","hash":"ab12","operation":"#,
            UPDATE,
            "}",
        ]
        .concat(),
    );
    assert_wire(
        &Saved {
            saved: blog(),
            operation: None,
            start_error: Some(ErrorResponse::new(ErrorCode::Conflict, "blog is busy")),
        },
        &[
            BLOG.strip_suffix('}').unwrap(),
            r#","start_error":{"error":"blog is busy","code":"conflict"}}"#,
        ]
        .concat(),
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

#[test]
fn network_problem_kinds() {
    for (kind, want) in [
        (AppProblemKind::NetworkOverlap, r#""network_overlap""#),
        (AppProblemKind::PortConflict, r#""port_conflict""#),
        (AppProblemKind::Route, r#""route""#),
    ] {
        assert_wire(&kind, want);
    }
}

fn net(cidr: &str) -> alchemy_api::network::Ipv4Net {
    cidr.parse().unwrap()
}

#[test]
fn network_report() {
    use alchemy_api::network::{
        AddressPool, AlchemyNetwork, AlchemyNetworkState, DockerNetwork, HostSide, ListenerState,
        MdnsBackend, MdnsReport, MdnsState, NetworkReport, NetworkWarning, NetworkWarningKind,
        PoolsSource, ProxyListener, ProxyReport, Reachability,
    };
    let report = NetworkReport {
        engine_ready: true,
        lan: vec![net("192.168.0.0/16")],
        routes: vec![net("100.64.0.0/10")],
        bridge: Some(net("172.17.0.0/16")),
        pools: vec![AddressPool::new(net("192.168.0.0/16"), 20).unwrap()],
        pools_source: PoolsSource::Defaults,
        networks: vec![DockerNetwork {
            name: "alchemy".into(),
            driver: Some("bridge".into()),
            subnets: vec![net("172.18.0.0/24")],
            project: None,
            managed: true,
        }],
        alchemy: AlchemyNetwork {
            name: "alchemy".into(),
            state: AlchemyNetworkState::Ready,
            subnet: Some(net("172.18.0.0/24")),
        },
        warnings: vec![NetworkWarning {
            kind: NetworkWarningKind::PoolOverlap,
            message: "m".into(),
            fix: Some("f".into()),
            subnet: Some(net("192.168.0.0/16")),
            host: Some(HostSide::Lan),
            host_subnet: Some(net("192.168.0.0/16")),
            network: None,
        }],
        proxy: ProxyReport {
            reachability: Some(Reachability::ContainerIp),
            host: ProxyListener {
                port: 80,
                app: None,
                state: ListenerState::Unavailable,
                message: Some("m".into()),
                fix: Some("f".into()),
            },
            ports: vec![ProxyListener {
                port: 8100,
                app: Some("blog".into()),
                state: ListenerState::Listening,
                message: None,
                fix: None,
            }],
        },
        mdns: MdnsReport {
            state: MdnsState::Unavailable,
            backend: Some(MdnsBackend::DnsSd),
            message: Some("m".into()),
            fix: Some("f".into()),
        },
    };
    assert_wire(
        &report,
        concat!(
            r#"{"engine_ready":true,"lan":["192.168.0.0/16"],"routes":["100.64.0.0/10"],"#,
            r#""bridge":"172.17.0.0/16","pools":[{"base":"192.168.0.0/16","size":20}],"#,
            r#""pools_source":"defaults","networks":[{"name":"alchemy","driver":"bridge","#,
            r#""subnets":["172.18.0.0/24"],"project":null,"managed":true}],"#,
            r#""alchemy":{"name":"alchemy","state":"ready","subnet":"172.18.0.0/24"},"#,
            r#""warnings":[{"kind":"pool_overlap","message":"m","fix":"f","#,
            r#""subnet":"192.168.0.0/16","host":"lan","host_subnet":"192.168.0.0/16","#,
            r#""network":null}],"proxy":{"reachability":"container_ip","#,
            r#""host":{"port":80,"app":null,"state":"unavailable","message":"m","fix":"f"},"#,
            r#""ports":[{"port":8100,"app":"blog","state":"listening","message":null,"#,
            r#""fix":null}]},"mdns":{"state":"unavailable","backend":"dns_sd","#,
            r#""message":"m","fix":"f"}}"#,
        ),
    );
    let older = concat!(
        r#"{"engine_ready":false,"lan":[],"routes":[],"bridge":null,"pools":[],"#,
        r#""pools_source":"unknown","networks":[],"#,
        r#""alchemy":{"name":"alchemy","state":"pending","subnet":null},"warnings":[]"#,
    );
    let proxy = r#""proxy":{"reachability":null,"host":{"port":80,"app":null,"state":"off","message":null,"fix":null},"ports":[]}"#;
    assert_wire(
        &NetworkReport::default(),
        &format!(
            r#"{older},{proxy},"mdns":{{"state":"off","backend":null,"message":null,"fix":null}}}}"#
        ),
    );
    let without_proxy: NetworkReport = serde_json::from_str(&format!("{older}}}")).unwrap();
    assert_eq!(
        without_proxy,
        NetworkReport::default(),
        "a daemon without a proxy"
    );
    let without_mdns: NetworkReport = serde_json::from_str(&format!("{older},{proxy}}}")).unwrap();
    assert_eq!(
        without_mdns,
        NetworkReport::default(),
        "a daemon without mDNS"
    );
}

#[test]
fn mdns_enums() {
    use alchemy_api::network::{MdnsBackend, MdnsState};
    for (state, want) in [
        (MdnsState::Off, r#""off""#),
        (MdnsState::Starting, r#""starting""#),
        (MdnsState::Announcing, r#""announcing""#),
        (MdnsState::Unavailable, r#""unavailable""#),
        (MdnsState::Other, r#""other""#),
    ] {
        assert_wire(&state, want);
    }
    for (backend, want) in [
        (MdnsBackend::Avahi, r#""avahi""#),
        (MdnsBackend::DnsSd, r#""dns_sd""#),
        (MdnsBackend::BuiltIn, r#""built_in""#),
        (MdnsBackend::Other, r#""other""#),
    ] {
        assert_wire(&backend, want);
    }
    let future: MdnsState = serde_json::from_str(r#""paused""#).unwrap();
    assert_eq!(future, MdnsState::Other);
    let future: MdnsBackend = serde_json::from_str(r#""systemd_resolved""#).unwrap();
    assert_eq!(future, MdnsBackend::Other);
}

#[test]
fn network_settings() {
    use alchemy_api::network::{NetworkSettings, NetworkSettingsUpdate};
    assert_wire(&NetworkSettings { mdns: true }, r#"{"mdns":true}"#);
    assert_wire(&NetworkSettings { mdns: false }, r#"{"mdns":false}"#);
    assert_eq!(NetworkSettings::default(), NetworkSettings { mdns: true });
    assert_wire(
        &NetworkSettingsUpdate { mdns: Some(false) },
        r#"{"mdns":false}"#,
    );
    assert_wire(&NetworkSettingsUpdate { mdns: None }, "{}");
    let cases = [
        (true, None, true),
        (true, Some(false), false),
        (false, Some(true), true),
        (false, None, false),
    ];
    for (mdns, update, want) in cases {
        assert_eq!(
            NetworkSettings { mdns }.updated(NetworkSettingsUpdate { mdns: update }),
            NetworkSettings { mdns: want },
            "{mdns} {update:?}"
        );
    }
}

#[test]
fn proxy_enums() {
    use alchemy_api::network::{ListenerState, Reachability};
    for (state, want) in [
        (ListenerState::Off, r#""off""#),
        (ListenerState::Listening, r#""listening""#),
        (ListenerState::Unavailable, r#""unavailable""#),
        (ListenerState::Other, r#""other""#),
    ] {
        assert_wire(&state, want);
    }
    for (reach, want) in [
        (Reachability::ContainerIp, r#""container_ip""#),
        (Reachability::PublishedOnly, r#""published_only""#),
        (Reachability::Other, r#""other""#),
    ] {
        assert_wire(&reach, want);
    }
    let future: ListenerState = serde_json::from_str(r#""draining""#).unwrap();
    assert_eq!(future, ListenerState::Other);
    let future: Reachability = serde_json::from_str(r#""tunnel""#).unwrap();
    assert_eq!(future, Reachability::Other);
}

#[test]
fn network_enums() {
    use alchemy_api::network::{AlchemyNetworkState, HostSide, NetworkWarningKind, PoolsSource};
    for (source, want) in [
        (PoolsSource::Engine, r#""engine""#),
        (PoolsSource::DaemonJson, r#""daemon_json""#),
        (PoolsSource::Defaults, r#""defaults""#),
        (PoolsSource::Unknown, r#""unknown""#),
    ] {
        assert_wire(&source, want);
    }
    for (state, want) in [
        (AlchemyNetworkState::Pending, r#""pending""#),
        (AlchemyNetworkState::Ready, r#""ready""#),
        (AlchemyNetworkState::Overlapping, r#""overlapping""#),
        (AlchemyNetworkState::Unavailable, r#""unavailable""#),
        (AlchemyNetworkState::Other, r#""other""#),
    ] {
        assert_wire(&state, want);
    }
    for (kind, want) in [
        (NetworkWarningKind::BridgeOverlap, r#""bridge_overlap""#),
        (NetworkWarningKind::PoolOverlap, r#""pool_overlap""#),
        (NetworkWarningKind::NetworkOverlap, r#""network_overlap""#),
        (
            NetworkWarningKind::AlchemyNetworkOverlap,
            r#""alchemy_network_overlap""#,
        ),
        (NetworkWarningKind::NoFreeSubnet, r#""no_free_subnet""#),
        (NetworkWarningKind::CreateFailed, r#""create_failed""#),
        (NetworkWarningKind::HostUnreadable, r#""host_unreadable""#),
        (
            NetworkWarningKind::EngineUnreadable,
            r#""engine_unreadable""#,
        ),
        (NetworkWarningKind::Other, r#""other""#),
    ] {
        assert_wire(&kind, want);
    }
    for (host, want) in [(HostSide::Lan, r#""lan""#), (HostSide::Route, r#""route""#)] {
        assert_wire(&host, want);
    }
    let future: NetworkWarningKind = serde_json::from_str(r#""ipv6_overlap""#).unwrap();
    assert_eq!(future, NetworkWarningKind::Other);
    let future: PoolsSource = serde_json::from_str(r#""etcd""#).unwrap();
    assert_eq!(future, PoolsSource::Unknown);
}
