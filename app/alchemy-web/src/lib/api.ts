import { apiBase, authHeaders, forgetDaemonToken, isTauri } from "./platform.ts";
import { CLOSED, type EventStream, openEventStream } from "./stream.ts";

/** Mirrors `alchemy_api::HealthResponse`. */
export interface HealthResponse {
  status: "ok";
}

/** Mirrors `alchemy_api::VersionResponse`. */
export interface VersionResponse {
  version: string;
  /** Mirrors `alchemy_api::API_VERSION`, bumped only on a breaking change. */
  api: number;
}

/** Mirrors `alchemy_api::EngineState`. */
export type EngineState = "not_installed" | "not_running" | "starting" | "ready" | "unsupported";

/** Mirrors `alchemy_api::EngineFlavor`. */
export type EngineFlavor = "docker_desktop" | "docker_engine" | "other";

/**
 * Mirrors `alchemy_api::EngineStatus`, returned by `GET /api/engine`. Fields
 * other than `state` are `null` until the engine has answered enough to know
 * them.
 */
export interface EngineStatus {
  state: EngineState;
  flavor: EngineFlavor | null;
  engine_version: string | null;
  api_version: string | null;
  compose_version: string | null;
  desktop_version: string | null;
  endpoint: string | null;
}

/**
 * Mirrors `alchemy_api::StopReason`: why a container is or is not running.
 * `unknown` is also what a kind this build does not know means.
 */
export type StopReason =
  | { kind: "running" }
  | { kind: "paused" }
  | { kind: "not_started" }
  | { kind: "stopped_by_alchemy" }
  | { kind: "stopped_outside" }
  | { kind: "crashed"; exit_code: number | null; oom: boolean }
  | { kind: "crash_looping"; exit_code: number | null; oom: boolean }
  | { kind: "finished" }
  | { kind: "unknown" };

/** Mirrors `alchemy_api::ContainerStatus`. */
export interface ContainerStatus {
  id: string;
  name: string;
  image: string | null;
  project: string | null;
  service: string | null;
  /** The Compose project folder, from its `working_dir` label. */
  working_dir: string | null;
  state: ContainerState;
  health: Health | null;
  /** Absent from a daemon older than stop reasons; treat as `unknown`. */
  stop_reason?: StopReason;
  /** Host ports it publishes while running. Absent when there are none. */
  ports?: PublishedPort[];
  /**
   * Its IPv4 address on each network it has joined while running, by
   * network name, such as `{ alchemy: "10.200.0.3" }`. Absent when there
   * are none.
   */
  networks?: Record<string, string>;
}

/** Mirrors `alchemy_api::Protocol`. */
export type Protocol = "tcp" | "udp";

/**
 * Mirrors `alchemy_api::PublishedPort`. Ports are text: `8080` or a range
 * such as `8000-8010`. `host_ip` null means every address; `published`
 * null is an ephemeral port Docker picks at start.
 */
export interface PublishedPort {
  protocol: Protocol;
  host_ip: string | null;
  published: string | null;
  target: string | null;
}

/**
 * Mirrors `alchemy_api::ports::PortSource`: reserved by Alchemy, declared by
 * a stopped app's Compose file, or held right now.
 */
export type PortSource = "reserved" | "declared" | "live";

/**
 * Mirrors `alchemy_api::ports::PortHolder`. A `program`'s process and pid
 * are null when the OS doesn't reveal them.
 */
export type PortHolder =
  | { kind: "app"; app: string; service: string }
  | { kind: "container"; name: string }
  | { kind: "reservation"; owner: string }
  | { kind: "program"; process: string | null; pid: number | null };

/** Mirrors `alchemy_api::ports::PortClaim`. */
export interface PortClaim {
  port: PublishedPort;
  holder: PortHolder;
  source: PortSource;
}

/**
 * Mirrors `alchemy_api::ports::PortConflict`: a port an app wants that
 * something else holds, from {@link ActionPlan.conflicts}.
 */
export interface PortConflict {
  service: string;
  wanted: PublishedPort;
  claim: PortClaim;
  /** A plain sentence naming the port and its holder. */
  message: string;
}

/** Mirrors `alchemy_api::ports::PortList`, from `GET /api/ports`. */
export interface PortList {
  ports: PortClaim[];
  /** What alchemyd couldn't find out, as sentences. Empty when complete. */
  warnings: string[];
}

/**
 * Mirrors `alchemy_api::MachineUsage`. CPU is a percentage of the whole
 * machine, rates are bytes per second, and sizes are bytes. `null` means not
 * measured, never zero. On macOS this is the Mac, not Docker Desktop's VM.
 */
export interface MachineUsage {
  cpu: number | null;
  /** The highest `cpu` within a rolled-up point. */
  cpu_max: number | null;
  cpus: number;
  /** The 1, 5, and 15 minute load averages, where the OS has them. */
  load: [number, number, number] | null;
  memory_used: number;
  memory_total: number;
  swap_used: number;
  swap_total: number;
  disk_used: number | null;
  disk_total: number | null;
  disk_read: number | null;
  disk_write: number | null;
  net_rx: number | null;
  net_tx: number | null;
}

/** Mirrors `alchemy_api::ContainerUsage`. CPU is a share of the whole engine host. */
export interface ContainerUsage {
  id: string;
  name: string;
  project: string | null;
  service: string | null;
  cpu: number | null;
  cpu_max: number | null;
  memory: number;
  memory_max: number | null;
  net_rx: number | null;
  net_tx: number | null;
  block_read: number | null;
  block_write: number | null;
}

/** Mirrors `alchemy_api::ProjectUsage`: one Compose project (an app) added up. */
export interface ProjectUsage {
  name: string;
  containers: number;
  cpu: number | null;
  memory: number;
  net_rx: number | null;
  net_tx: number | null;
}

/**
 * Mirrors `alchemy_api::MetricsResponse`, returned by `GET /api/metrics` and
 * sent as the `metrics` event after every sample.
 */
export interface MetricsResponse {
  /** Unix seconds of the sample, or `null` before the first. */
  at: number | null;
  /** Seconds between samples. */
  interval: number;
  /** Containers run in a VM (Docker Desktop, macOS): machine figures are the host, not the VM. */
  containers_in_vm: boolean;
  machine: MachineUsage | null;
  projects: ProjectUsage[];
  containers: ContainerUsage[];
}

/** Mirrors `alchemy_api::MetricsRange`. */
export type MetricsRange = "1h" | "12h" | "24h" | "7d" | "30d";

/** Mirrors `alchemy_api::MetricsPoint`. `t` is Unix seconds. */
export interface MetricsPoint {
  t: number;
  machine: MachineUsage | null;
  projects: ProjectUsage[];
  containers: ContainerUsage[];
}

/**
 * Mirrors `alchemy_api::MetricsHistory`, returned by
 * `GET /api/metrics/history`. Points more than one `step` apart have a gap
 * between them, which charts draw as a gap.
 */
export interface MetricsHistory {
  range: MetricsRange;
  step: number;
  /** The project asked for or the asked-for app's, `null` for the machine or an unread app. */
  project: string | null;
  points: MetricsPoint[];
}

/**
 * Narrows `/api/metrics` and `/api/metrics/history` to one app, by its
 * folder name, or to one Compose project. An unknown app is a 404
 * `app_not_found`; an app Compose hasn't read yet has no figures.
 */
export type MetricsFilter = { app: string } | { project: string };

/**
 * Mirrors `alchemy_api::apps::StackStatus`. A status a newer daemon sends
 * that this build doesn't know should be treated as `unknown`.
 */
export type StackStatus = "running" | "partial" | "stopped" | "unknown";

/** Mirrors `alchemy_api::apps::ImageSource`. */
export type ImageSource = "registry" | "build";

/** Mirrors `alchemy_core::engine::ContainerState`. */
export type ContainerState =
  | "created"
  | "running"
  | "paused"
  | "restarting"
  | "removing"
  | "exited"
  | "dead";

/** Mirrors `alchemy_core::engine::Health`. */
export type Health = "starting" | "healthy" | "unhealthy";

/** Mirrors `alchemy_api::apps::AppContainer`. */
export interface AppContainer {
  id: string;
  name: string;
  image: string | null;
  state: ContainerState;
  health: Health | null;
  /** Absent from a daemon older than stop reasons; treat as `unknown`. */
  stop_reason?: StopReason;
}

/** Mirrors `alchemy_api::apps::AppService`. */
export interface AppService {
  name: string;
  containers: AppContainer[];
}

/** Mirrors `alchemy_api::apps::AppProblemKind`; unknown kinds arrive as `other`. */
export type AppProblemKind =
  | "compose_error"
  | "compose_unavailable"
  | "shared_project"
  | "network_overlap"
  | "port_conflict"
  | "route"
  | "other";

/** Mirrors `alchemy_api::apps::AppProblem`. `message` is safe to show as is. */
export interface AppProblem {
  kind: AppProblemKind;
  message: string;
}

/** Mirrors `alchemy_api::apps::App`, returned by `GET /api/apps/{name}`. */
export interface App {
  /** The app's name: its folder's name. */
  name: string;
  status: StackStatus;
  /** The app's folder. */
  dir: string;
  /** The app folder it was found in. */
  root: string;
  compose_file: string;
  /** The Compose project name; `null` until Compose has read the app, or when it can't. */
  project: string | null;
  images: ImageSource | null;
  services: AppService[];
  problems: AppProblem[];
  /** The most pressing of its containers' reasons. Absent from an older daemon. */
  stop_reason?: StopReason;
  /**
   * Where to open the app through Alchemy's proxy, best first. Absent when it
   * has no route; a `route` problem then says why.
   */
  addresses?: AppAddress[];
}

/** Mirrors `alchemy_api::apps::AddressKind`; unknown kinds arrive as `other`. */
export type AddressKind = "host" | "port" | "other";

/** Mirrors `alchemy_api::apps::AddressState`; unknown states arrive as `other`. */
export type AddressState = "ready" | "pending" | "not_running" | "unavailable" | "other";

/**
 * Mirrors `alchemy_api::apps::AppAddress`: one web address of an app, such as
 * `http://blog.local/` or `http://blog.localhost/` (`host`), or
 * `http://blog.local:8100/` or `http://192.168.1.20:8100/` (`port`).
 * `message` says why it doesn't answer and is safe to show as is.
 */
export interface AppAddress {
  kind: AddressKind;
  url: string;
  state: AddressState;
  /** The service and container port requests go to. */
  service: string;
  port: number;
  message: string | null;
}

/** Mirrors `alchemy_api::apps::UnmanagedApp`. */
export interface UnmanagedApp {
  project: string;
  dir: string | null;
  status: StackStatus;
  services: AppService[];
  /** The most pressing of its containers' reasons. Absent from an older daemon. */
  stop_reason?: StopReason;
}

/** Mirrors `alchemy_api::apps::AppDirState`; unknown states arrive as `other`. */
export type AppDirState = "ok" | "missing" | "not_a_directory" | "unreadable" | "other";

/** Mirrors `alchemy_api::apps::SkipReason`; unknown reasons arrive as `other`. */
export type SkipReason =
  | "invalid_name"
  | "duplicate"
  | "linked_compose_file"
  | "compose_file_not_regular"
  | "linked_folder"
  | "other";

/** Mirrors `alchemy_api::apps::SkippedApp`. */
export interface SkippedApp {
  name: string;
  path: string;
  reason: SkipReason;
  message: string;
}

/** Mirrors `alchemy_api::apps::AppDir`. */
export interface AppDir {
  path: string;
  /** Whether this is the home, where new apps go. */
  home: boolean;
  state: AppDirState;
  /** Why the folder can't be used, when `state` isn't `ok`. */
  message: string | null;
  skipped: SkippedApp[];
}

/** Mirrors `alchemy_api::apps::AppList`, returned by `GET /api/apps` and sent as `apps`. */
export interface AppList {
  apps: App[];
  unmanaged: UnmanagedApp[];
  dirs: AppDir[];
}

/** Mirrors `alchemy_api::apps::AppSettings`, returned by `GET /api/settings/apps`. */
export interface AppSettings {
  /** The home in effect: the chosen one, else `default_home`. */
  home: string | null;
  default_home: string | null;
  extra: string[];
}

/** Mirrors `alchemy_api::apps::AppSettingsUpdate`, the body of `PUT /api/settings/apps`. */
export interface AppSettingsUpdate {
  /** A full path, or `null` for the default home. */
  home: string | null;
  extra: string[];
}

/** Mirrors `alchemy_api::apps::files::MAX_FILE_LEN`: the most bytes a compose file or `.env` holds. */
export const MAX_FILE_LEN = 256 * 1024;

/** Mirrors `alchemy_api::apps::files::NewApp`, the body of `POST /api/apps`. */
export interface NewApp {
  name: string;
  /** A configured app folder as a full path, or `null` for the home. */
  folder: string | null;
  compose: string;
  /** Saved as `.env`, readable by its owner alone; `null` writes none. */
  env: string | null;
  dry_run: boolean;
  /** Start the app once it is created. Refused with `dry_run`. */
  start?: boolean;
}

/** Mirrors `alchemy_api::apps::files::AppFileKind`, also the route's path segment. */
export type AppFileKind = "compose" | "env";

/** Mirrors `alchemy_api::apps::files::AppFile`, from `GET /api/apps/{name}/files/{kind}`. */
export interface AppFile {
  kind: AppFileKind;
  /** Its name in the app's folder, such as `compose.yaml` or `.env`. */
  name: string;
  /** Empty when the file doesn't exist. */
  content: string;
  /** SHA-256 of the bytes in hex, sent back as `base_hash`; `null` when the file doesn't exist. */
  hash: string | null;
}

/**
 * Mirrors `alchemy_api::apps::files::AppFileUpdate`, the body of
 * `PUT /api/apps/{name}/files/{kind}`. A `base_hash` that no longer matches
 * the file on disk is refused with 409 `conflict`; Compose rejecting the
 * content is 422 `invalid` with Compose's message.
 */
export interface AppFileUpdate {
  content: string;
  base_hash: string | null;
  dry_run: boolean;
  /** Start the app once the file is saved. Refused with `dry_run`. */
  start?: boolean;
}

/**
 * Mirrors `alchemy_api::apps::files::Saved`: the created {@link App} or the
 * saved {@link AppFile}, and, when the request sent `start`, either the
 * operation that started the app or why it wasn't started. The save stands
 * either way.
 */
export type Saved<T> = T & {
  /** The `up` operation started on the app, running. */
  operation?: Operation;
  /** Why the app was saved but not started, as an action would be refused. */
  start_error?: ErrorResponse;
};

/** Mirrors `alchemy_api::logs::LogStream`; unknown outputs arrive as `other`. */
export type LogStream = "stdout" | "stderr" | "other";

/** Mirrors `alchemy_api::logs::LogLine`: one line of a container's log. */
export interface LogLine {
  service: string;
  /** The container's name. */
  container: string;
  stream: LogStream;
  /** When the engine received the line, as RFC 3339 in UTC, or `null`. */
  time: string | null;
  /** The line without its newline; a line over 8 KiB is cut and ends in `…`. */
  line: string;
}

/** Mirrors `alchemy_api::logs::LogsDone`. `error` is safe to show as is. */
export interface LogsDone {
  error: string | null;
}

/**
 * Mirrors `alchemy_api::logs::LogEvent`: one message on
 * `GET /api/apps/{name}/logs`. `log` events carry the tail, then, when
 * following, a `following` event and new lines; `done` ends the stream.
 */
export type LogEvent =
  | { event: "log"; data: LogLine }
  | { event: "following"; data: Record<string, never> }
  | { event: "done"; data: LogsDone };
/** Mirrors `alchemy_api::operations::Action`: a lifecycle action on an app. */
export type Action = "up" | "down" | "start" | "stop" | "restart" | "pull" | "build" | "update";

/** Mirrors `alchemy_api::operations::StepCommand`: the `docker compose` subcommand a step runs. */
export type StepCommand = "up" | "down" | "start" | "stop" | "restart" | "pull" | "build";

/** Mirrors `alchemy_api::operations::OperationState`; unknown states arrive as `unknown`. */
export type OperationState =
  | "running"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "interrupted"
  | "unknown";

/** Mirrors `alchemy_api::operations::StepState`; unknown states arrive as `unknown`. */
export type OperationStepState = "pending" | "active" | "done" | "failed" | "skipped" | "unknown";

/** Mirrors `alchemy_api::operations::Step`. */
export interface OperationStep {
  command: StepCommand;
  /** What the step does, in plain words: "Downloading the app". */
  label: string;
  state: OperationStepState;
  /** Progress while it runs, or the problem when it failed. */
  message: string | null;
}

/** Mirrors `alchemy_api::operations::Operation`: an action run on one app. */
export interface Operation {
  id: string;
  app: string;
  action: Action;
  state: OperationState;
  steps: OperationStep[];
  /** Unix seconds. */
  started_at: number;
  /** Unix seconds, or `null` while it runs. */
  finished_at: number | null;
  /** Why it did not succeed, safe to show. */
  error: string | null;
}

/** Mirrors `alchemy_api::operations::ActionRequest`, the body of `POST /api/apps/{name}/actions`. */
export interface ActionRequest {
  action: Action;
  /**
   * Go ahead even though another app shares this app's Compose project, or
   * a host port the app publishes is taken.
   */
  force?: boolean;
}

/** Mirrors `alchemy_api::operations::PlannedStep`. */
export interface PlannedStep {
  command: StepCommand;
  label: string;
  /** The whole command line, starting with `docker`. */
  argv: string[];
}

/** Mirrors `alchemy_api::operations::ActionPlan`, from `GET /api/apps/{name}/plan?action=ACTION`. */
export interface ActionPlan {
  app: string;
  action: Action;
  steps: PlannedStep[];
  /** Ports that refuse the action unless forced. Absent when there are none. */
  conflicts?: PortConflict[];
}

/** Mirrors `alchemy_api::operations::OperationResponse`. */
export interface OperationResponse {
  operation: Operation;
}

/** Mirrors `alchemy_api::OperationList`, returned by `GET /api/operations` and sent as `operations`. */
export interface OperationList {
  /** Newest first. */
  operations: Operation[];
}

/** Mirrors `alchemy_api::operations::OutputLine`: one line of an operation's raw output. */
export interface OutputLine {
  /** Counts up from 0; a gap means older lines were dropped. */
  seq: number;
  /** Index into {@link Operation.steps}. */
  step: number;
  text: string;
}

/**
 * Mirrors `alchemy_api::operations::OperationEvent`: one message on
 * `GET /api/operations/{id}/events`. `done` is always last.
 */
export type OperationEvent =
  | { event: "operation"; data: Operation }
  | { event: "output"; data: OutputLine }
  | { event: "done"; data: Operation };

/** Every event name on an operation's stream. */
const OPERATION_EVENT_NAMES: readonly OperationEvent["event"][] = ["operation", "output", "done"];

/**
 * Mirrors `alchemy_api::ApiEvent`: one message on `GET /api/events`. The SSE
 * `event:` field is `event`, and its `data:` field is `data` as JSON.
 */
export type ApiEvent =
  | { event: "engine"; data: EngineStatus }
  | { event: "metrics"; data: MetricsResponse }
  | { event: "apps"; data: AppList }
  | { event: "operations"; data: OperationList };

/** Every SSE event name the daemon sends. */
export type ApiEventName = ApiEvent["event"];

/** The payload of the event named `K`. */
export type ApiEventData<K extends ApiEventName> = Extract<ApiEvent, { event: K }>["data"];

/** One key per event name, so adding an {@link ApiEvent} variant fails to compile until listed. */
const EVENT_NAMES: { [K in ApiEventName]: K } = {
  engine: "engine",
  metrics: "metrics",
  apps: "apps",
  operations: "operations",
};

/** The event names {@link subscribe} listens for. Others are ignored. */
export const API_EVENT_NAMES: readonly ApiEventName[] = Object.values(EVENT_NAMES);

/** Optional callbacks for {@link subscribe}. */
export interface SubscribeOptions {
  /** The stream connected, or reconnected after an error. */
  onOpen?: () => void;
  /**
   * The stream dropped or failed to connect. The browser retries on its own;
   * the daemon resends the current state when it does.
   */
  onError?: () => void;
}

/**
 * Mirrors `alchemy_api::ErrorCode`. A newer daemon may send a code not listed
 * here; the Rust client reads those as `unknown`.
 */
export type ErrorCode =
  | "not_found"
  | "method_not_allowed"
  | "bad_request"
  | "forbidden"
  | "unauthorized"
  | "unsupported_media_type"
  | "conflict"
  | "invalid"
  | "app_not_found"
  | "operation_not_found"
  | "busy"
  | "internal"
  | "unknown";

/**
 * Mirrors `alchemy_api::session::SessionAuth`: how a request was let in. A
 * kind this build doesn't know arrives as `unknown`.
 */
export type SessionAuth = "socket" | "token" | "cookie" | "unknown";

/**
 * Mirrors `alchemy_api::session::SessionResponse`, returned by
 * `GET /api/session`. On the TCP port it answers 401 `unauthorized` until the
 * browser has signed in with `alchemy open` or the shell sends the token.
 */
export interface SessionResponse {
  auth: SessionAuth;
}

/** Mirrors `alchemy_api::ErrorResponse`, the body of every `/api` error. */
export interface ErrorResponse {
  /** Human-readable detail, safe to show to the user. */
  error: string;
  /** Machine-readable failure category. */
  code: ErrorCode;
}

/** An HTTP method {@link request} sends. */
/** Mirrors `alchemy_api::network::AddressPool`: `base` is CIDR text, `size` a prefix length. */
export interface AddressPool {
  base: string;
  size: number;
}

/** Mirrors `alchemy_api::network::PoolsSource`; unknown sources arrive as `unknown`. */
export type PoolsSource = "engine" | "daemon_json" | "defaults" | "unknown";

/** Mirrors `alchemy_api::network::DockerNetwork`. Subnets are CIDR text. */
export interface DockerNetwork {
  name: string;
  driver: string | null;
  subnets: string[];
  project: string | null;
  /** Whether Alchemy created it. */
  managed: boolean;
}

/** Mirrors `alchemy_api::network::AlchemyNetworkState`; unknown states arrive as `other`. */
export type AlchemyNetworkState = "pending" | "ready" | "overlapping" | "unavailable" | "other";

/** Mirrors `alchemy_api::network::AlchemyNetwork`: Alchemy's own shared network. */
export interface AlchemyNetwork {
  name: string;
  state: AlchemyNetworkState;
  subnet: string | null;
}

/** Mirrors `alchemy_api::network::NetworkWarningKind`; unknown kinds arrive as `other`. */
export type NetworkWarningKind =
  | "bridge_overlap"
  | "pool_overlap"
  | "network_overlap"
  | "alchemy_network_overlap"
  | "no_free_subnet"
  | "create_failed"
  | "host_unreadable"
  | "engine_unreadable"
  | "other";

/**
 * Mirrors `alchemy_api::network::NetworkWarning`. `message` and `fix` are
 * safe to show as is; `fix` may span lines, such as a `daemon.json` fragment.
 */
export interface NetworkWarning {
  kind: NetworkWarningKind;
  message: string;
  fix: string | null;
  subnet: string | null;
  host: "lan" | "route" | null;
  host_subnet: string | null;
  network: string | null;
}

/**
 * Mirrors `alchemy_api::network::NetworkReport`, returned by `GET /api/network`.
 * Subnets are CIDR text. Docker's side is empty while `engine_ready` is false.
 */
export interface NetworkReport {
  engine_ready: boolean;
  lan: string[];
  routes: string[];
  bridge: string | null;
  pools: AddressPool[];
  pools_source: PoolsSource;
  networks: DockerNetwork[];
  alchemy: AlchemyNetwork;
  warnings: NetworkWarning[];
  /** The web address proxy. Absent from an older daemon. */
  proxy?: ProxyReport;
  /** Announcing apps' `.local` names. Absent from an older daemon. */
  mdns?: MdnsReport;
}

/** Mirrors `alchemy_api::network::MdnsState`; unknown states arrive as `other`. */
export type MdnsState = "off" | "starting" | "announcing" | "unavailable" | "other";

/** Mirrors `alchemy_api::network::MdnsBackend`; unknown backends arrive as `other`. */
export type MdnsBackend = "avahi" | "dns_sd" | "built_in" | "other";

/**
 * Mirrors `alchemy_api::network::MdnsReport`: whether apps' `.local` names are
 * announced on the LAN. `message` and `fix` are safe to show as is.
 */
export interface MdnsReport {
  /** `off` while the `network.mdns` setting is off. */
  state: MdnsState;
  /** `null` until an announcer was chosen. */
  backend: MdnsBackend | null;
  message: string | null;
  fix: string | null;
}

/** Mirrors `alchemy_api::network::NetworkSettings`, returned by `GET /api/settings/network`. */
export interface NetworkSettings {
  /** Whether apps' `.local` names are announced; when off, no app has a `.local` address. */
  mdns: boolean;
}

/**
 * Mirrors `alchemy_api::network::NetworkSettingsUpdate`, the body of
 * `PUT /api/settings/network`. A setting left out keeps its value.
 */
export interface NetworkSettingsUpdate {
  mdns?: boolean;
}

/**
 * Mirrors `alchemy_api::network::ListenerState`; unknown states arrive as
 * `other`. `unavailable` covers a port that couldn't be opened and one that
 * is open but another program answers, such as a firewall redirect.
 */
export type ListenerState = "off" | "listening" | "unavailable" | "other";

/** Mirrors `alchemy_api::network::Reachability`; unknown values arrive as `other`. */
export type Reachability = "container_ip" | "published_only" | "other";

/**
 * Mirrors `alchemy_api::network::ProxyListener`. `message` and `fix` are safe
 * to show as is.
 */
export interface ProxyListener {
  port: number;
  /** The app it serves, or `null` for the Host listener on port 80. */
  app: string | null;
  state: ListenerState;
  message: string | null;
  fix: string | null;
}

/** Mirrors `alchemy_api::network::ProxyReport`. */
export interface ProxyReport {
  /** `null` until the engine has been ready. */
  reachability: Reachability | null;
  /**
   * Port 80, which routes `<app>.localhost` and, while names are announced,
   * `<app>.local`; `off` while no app has a hostname.
   */
  host: ProxyListener;
  /** Each app's own proxy port, ascending. */
  ports: ProxyListener[];
}

export type Method = "GET" | "POST" | "PUT" | "DELETE";

/** `ApiError.status` when no response arrived at all. */
export const NO_RESPONSE = 0;

/**
 * Why an `/api` request failed. `message` is always safe to show the user:
 * the daemon's {@link ErrorResponse} text when it sent one, or a generic line
 * otherwise. `status` is the HTTP status, or {@link NO_RESPONSE} when the
 * daemon could not be reached.
 */
export class ApiError extends Error {
  readonly status: number;
  /** The daemon's {@link ErrorCode}, or `null` when it sent none. */
  readonly code: ErrorCode | null;

  constructor(status: number, message: string, code: ErrorCode | null = null) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
  }
}

/**
 * Whether `error` is alchemyd refusing a request that isn't signed in: a
 * browser that hasn't been through `alchemy open`, or a shell without the
 * daemon's current token.
 */
export function isSignedOut(error: unknown): boolean {
  return error instanceof ApiError && error.status === 401 && error.code === "unauthorized";
}

/** Options for {@link request}. */
export interface RequestOptions {
  /** Sent as JSON. */
  body?: unknown;
  signal?: AbortSignal;
}

function isAbort(err: unknown): boolean {
  return err instanceof DOMException && err.name === "AbortError";
}

/** The error in `res`: the daemon's {@link ErrorResponse}, or a generic line for any other body. */
async function responseError(res: Response): Promise<ApiError> {
  let body: unknown;
  try {
    body = await res.json();
  } catch {
    body = null;
  }
  const { error, code } = (body ?? {}) as Partial<ErrorResponse>;
  const message =
    typeof error === "string" && error !== "" ? error : `alchemyd answered with HTTP ${res.status}`;
  return new ApiError(res.status, message, typeof code === "string" ? code : null);
}

/**
 * Send one request to `/api{path}` and return the JSON response, or
 * `undefined` for `204 No Content`. `body`, when given, is sent as JSON.
 * Every method but `GET` declares `content-type: application/json`, body or
 * not, because alchemyd refuses a mutating request without it (415). In the
 * desktop shell it also sends the daemon's token, and after a 401 reads the
 * token again and retries once, since alchemyd makes a new one each start.
 * Rejects with an {@link ApiError} for every failure except an abort, which
 * rejects with the `AbortError` unchanged.
 */
export async function request<T>(
  method: Method,
  path: string,
  { body, signal }: RequestOptions = {},
): Promise<T> {
  const send = async (): Promise<Response> => {
    const headers: Record<string, string> = await authHeaders();
    if (method !== "GET") {
      headers["content-type"] = "application/json";
    }
    const init: RequestInit = { method, signal, headers };
    if (body !== undefined) {
      init.body = JSON.stringify(body);
    }
    return fetch(`${apiBase()}/api${path}`, init);
  };

  let res: Response;
  try {
    res = await send();
    if (res.status === 401 && isTauri()) {
      forgetDaemonToken();
      res = await send();
    }
  } catch (err) {
    if (isAbort(err)) throw err;
    throw new ApiError(NO_RESPONSE, "alchemyd is not reachable");
  }

  if (!res.ok) {
    throw await responseError(res);
  }
  if (res.status === 204) {
    return undefined as T;
  }
  try {
    return (await res.json()) as T;
  } catch (err) {
    if (isAbort(err)) throw err;
    throw new ApiError(res.status, "alchemyd sent a response Alchemy can't read");
  }
}

/**
 * How long {@link subscribe} waits before reopening a stream the browser has
 * stopped retrying, which it does when a reconnect gets an error response
 * (a dev proxy with the daemon down) instead of a refused connection.
 */
export const RESUBSCRIBE_DELAY_MS = 2000;

/**
 * Open `GET /api/events` and call `onEvent` for every event. The first
 * `engine` event carries the current status, so no separate fetch is needed.
 * Frames whose data is not valid JSON are skipped. If the browser gives up
 * reconnecting, the stream is reopened after {@link RESUBSCRIBE_DELAY_MS}.
 *
 * Returns a function that closes the stream.
 */
export function subscribe(
  onEvent: (event: ApiEvent) => void,
  options: SubscribeOptions = {},
): () => void {
  let closed = false;
  let retry: ReturnType<typeof setTimeout> | undefined;
  let source: EventStream;

  const open = () => {
    retry = undefined;
    const current = openEventStream(`${apiBase()}/api/events`);
    source = current;
    for (const name of API_EVENT_NAMES) {
      current.addEventListener(name, (message) => {
        let data: unknown;
        try {
          data = JSON.parse((message as MessageEvent<string>).data);
        } catch {
          return;
        }
        onEvent({ event: name, data } as ApiEvent);
      });
    }
    current.addEventListener("open", () => options.onOpen?.());
    current.addEventListener("error", () => {
      options.onError?.();
      if (!closed && retry === undefined && current.readyState === CLOSED) {
        retry = setTimeout(open, RESUBSCRIBE_DELAY_MS);
      }
    });
  };

  open();
  return () => {
    closed = true;
    clearTimeout(retry);
    source.close();
  };
}

/**
 * Follow `GET /api/operations/{id}/events`, calling `onEvent` for each event
 * until `done`, then closing the stream. The daemon replays the output it
 * kept first, so following late misses nothing it still has.
 *
 * Returns a function that closes the stream.
 */
export function followOperation(id: string, onEvent: (event: OperationEvent) => void): () => void {
  const source = openEventStream(`${apiBase()}/api/operations/${encodeURIComponent(id)}/events`);
  for (const name of OPERATION_EVENT_NAMES) {
    source.addEventListener(name, (message) => {
      let data: unknown;
      try {
        data = JSON.parse((message as MessageEvent<string>).data);
      } catch {
        return;
      }
      onEvent({ event: name, data } as OperationEvent);
      if (name === "done") {
        source.close();
      }
    });
  }
  return () => source.close();
}

export const api = {
  health: (signal?: AbortSignal) => request<HealthResponse>("GET", "/health", { signal }),
  version: (signal?: AbortSignal) => request<VersionResponse>("GET", "/version", { signal }),
  session: (signal?: AbortSignal) => request<SessionResponse>("GET", "/session", { signal }),
  engine: (signal?: AbortSignal) => request<EngineStatus>("GET", "/engine", { signal }),
  metrics: (signal?: AbortSignal, filter?: MetricsFilter) => {
    const query = new URLSearchParams(filter).toString();
    return request<MetricsResponse>("GET", query === "" ? "/metrics" : `/metrics?${query}`, {
      signal,
    });
  },
  metricsHistory: (range: MetricsRange, filter?: MetricsFilter, signal?: AbortSignal) => {
    const query = new URLSearchParams({ range, ...filter });
    return request<MetricsHistory>("GET", `/metrics/history?${query.toString()}`, { signal });
  },
  ports: (signal?: AbortSignal) => request<PortList>("GET", "/ports", { signal }),
  apps: (signal?: AbortSignal) => request<AppList>("GET", "/apps", { signal }),
  app: (name: string, signal?: AbortSignal) =>
    request<App>("GET", `/apps/${encodeURIComponent(name)}`, { signal }),
  appSettings: (signal?: AbortSignal) => request<AppSettings>("GET", "/settings/apps", { signal }),
  setAppSettings: (update: AppSettingsUpdate) =>
    request<AppSettings>("PUT", "/settings/apps", { body: update }),
  startAction: (name: string, body: ActionRequest) =>
    request<OperationResponse>("POST", `/apps/${encodeURIComponent(name)}/actions`, { body }),
  actionPlan: (name: string, action: Action, signal?: AbortSignal) =>
    request<ActionPlan>(
      "GET",
      `/apps/${encodeURIComponent(name)}/plan?action=${encodeURIComponent(action)}`,
      { signal },
    ),
  operations: (signal?: AbortSignal) => request<OperationList>("GET", "/operations", { signal }),
  network: (signal?: AbortSignal) => request<NetworkReport>("GET", "/network", { signal }),
  networkSettings: (signal?: AbortSignal) =>
    request<NetworkSettings>("GET", "/settings/network", { signal }),
  setNetworkSettings: (update: NetworkSettingsUpdate) =>
    request<NetworkSettings>("PUT", "/settings/network", { body: update }),
  cancelOperation: (id: string) =>
    request<OperationResponse>("POST", `/operations/${encodeURIComponent(id)}/cancel`),
  followOperation,
  createApp: (app: NewApp) => request<Saved<App>>("POST", "/apps", { body: app }),
  appFile: (name: string, kind: AppFileKind, signal?: AbortSignal) =>
    request<AppFile>("GET", `/apps/${encodeURIComponent(name)}/files/${kind}`, { signal }),
  saveAppFile: (name: string, kind: AppFileKind, update: AppFileUpdate) =>
    request<Saved<AppFile>>("PUT", `/apps/${encodeURIComponent(name)}/files/${kind}`, {
      body: update,
    }),
  subscribe,
};
