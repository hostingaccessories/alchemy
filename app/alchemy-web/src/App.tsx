import {
  CircleAlert,
  CircleCheck,
  CircleX,
  FlaskConical,
  LoaderCircle,
  type LucideIcon,
} from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";

import { type EngineStatus, api } from "./lib/api.ts";

type DaemonState =
  | { kind: "loading" }
  | { kind: "online"; version: string }
  | { kind: "offline"; error: string };

/** Where Docker's own install guide lives; shown when Docker is missing. */
const GET_DOCKER_URL = "https://docs.docker.com/get-started/get-docker/";

export function App() {
  const [daemon, setDaemon] = useState<DaemonState>({ kind: "loading" });
  const [engine, setEngine] = useState<EngineStatus | null>(null);

  useEffect(() => {
    let probe: AbortController | undefined;
    const checkDaemon = () => {
      probe?.abort();
      const controller = new AbortController();
      probe = controller;
      api
        .version(controller.signal)
        .then(({ version }) => setDaemon({ kind: "online", version }))
        .catch((err: unknown) => {
          if (!controller.signal.aborted) {
            setDaemon({ kind: "offline", error: String(err) });
          }
        });
    };

    checkDaemon();
    const unsubscribe = api.subscribe(
      (event) => {
        if (event.event === "engine") {
          setEngine(event.data);
        }
      },
      {
        onOpen: checkDaemon,
        onError: () => {
          setEngine(null);
          checkDaemon();
        },
      },
    );
    return () => {
      probe?.abort();
      unsubscribe();
    };
  }, []);

  return (
    <main className="flex min-h-screen flex-col items-center justify-center gap-6 bg-stone-950 px-4 text-stone-100">
      <div className="flex items-center gap-3">
        <FlaskConical className="size-10 text-amber-500" aria-hidden />
        <h1 className="text-4xl font-semibold tracking-tight">Alchemy</h1>
      </div>
      <div role="status" aria-label="alchemyd">
        <DaemonStatus state={daemon} />
      </div>
      {daemon.kind === "online" && (
        <div role="status" aria-label="Docker" className="max-w-md text-center">
          <EngineStatusView status={engine} />
        </div>
      )}
    </main>
  );
}

function DaemonStatus({ state }: { state: DaemonState }) {
  switch (state.kind) {
    case "loading":
      return (
        <p className="flex items-center gap-2 text-stone-400">
          <LoaderCircle className="size-4 animate-spin" aria-hidden />
          Connecting to alchemyd…
        </p>
      );
    case "online":
      return (
        <p className="flex items-center gap-2 text-emerald-400">
          <CircleCheck className="size-4" aria-hidden />
          alchemyd {state.version}
        </p>
      );
    case "offline":
      return (
        <p className="flex items-center gap-2 text-rose-400" title={state.error}>
          <CircleX className="size-4" aria-hidden />
          alchemyd is not reachable
        </p>
      );
  }
}

type Tone = "ok" | "warn" | "error" | "busy";

const TONES: Record<Tone, { icon: LucideIcon; className: string }> = {
  ok: { icon: CircleCheck, className: "text-emerald-400" },
  warn: { icon: CircleAlert, className: "text-amber-400" },
  error: { icon: CircleX, className: "text-rose-400" },
  busy: { icon: LoaderCircle, className: "text-stone-400" },
};

/** One problem-first line, with optional detail underneath. */
function Notice({ tone, title, children }: { tone: Tone; title: string; children?: ReactNode }) {
  const { icon: Icon, className } = TONES[tone];
  return (
    <div className="flex flex-col items-center gap-1">
      <p className={`flex items-center gap-2 ${className}`}>
        {tone === "busy" ? (
          <Icon className="size-4 animate-spin" role="img" aria-label="In progress" />
        ) : (
          <Icon className="size-4" aria-hidden />
        )}
        {title}
      </p>
      {children && <div className="text-sm text-stone-400">{children}</div>}
    </div>
  );
}

function Code({ children }: { children: ReactNode }) {
  return <code className="rounded bg-stone-800 px-1 py-0.5 text-stone-200">{children}</code>;
}

/**
 * Docker's state as the daemon last reported it. `null` means no report
 * yet, or the event stream dropped and is reconnecting.
 */
function EngineStatusView({ status }: { status: EngineStatus | null }) {
  if (!status) {
    return <Notice tone="busy" title="Checking Docker…" />;
  }
  switch (status.state) {
    case "not_installed":
      return (
        <Notice tone="error" title="Docker isn't installed">
          <p>Alchemy runs your apps with Docker. Install Docker Desktop, then open it.</p>
          <p>
            <a
              className="text-amber-400 underline"
              href={GET_DOCKER_URL}
              target="_blank"
              rel="noreferrer"
            >
              docs.docker.com/get-started/get-docker
            </a>
          </p>
        </Notice>
      );
    case "not_running":
      return <NotRunning status={status} />;
    case "starting":
      return (
        <Notice
          tone="busy"
          title={
            status.flavor === "docker_desktop"
              ? "Docker Desktop is starting…"
              : "Docker is starting…"
          }
        />
      );
    case "ready":
      return <Ready status={status} />;
    case "unsupported":
      return (
        <Notice tone="error" title="Alchemy can't use this Docker connection">
          <p>
            Docker is set to use{" "}
            {status.endpoint ? <Code>{status.endpoint}</Code> : "a remote connection"}. Alchemy only
            works with Docker on this machine, through a local <Code>unix://</Code> socket.
          </p>
        </Notice>
      );
    default:
      return (
        <Notice
          tone="warn"
          title="Docker is in a state this version of Alchemy doesn't recognize. Update Alchemy."
        />
      );
  }
}

function NotRunning({ status }: { status: EngineStatus }) {
  const where = status.endpoint && <p>Looking for Docker at {status.endpoint}</p>;
  switch (status.flavor) {
    case "docker_desktop":
      return (
        <Notice tone="error" title="Docker Desktop isn't running. Open it to continue.">
          {where}
        </Notice>
      );
    case "docker_engine":
      return (
        <Notice tone="error" title="Docker Engine isn't running. Start it to continue.">
          <p>
            On most Linux systems: <Code>sudo systemctl start docker</Code>
          </p>
          {where}
        </Notice>
      );
    default:
      return (
        <Notice
          tone="error"
          title="Docker isn't running. Open Docker Desktop, or start the Docker service, to continue."
        >
          {where}
        </Notice>
      );
  }
}

function Ready({ status }: { status: EngineStatus }) {
  const compose = status.compose_version?.replace(/^v/, "");
  const versions = [
    status.desktop_version && `Docker Desktop ${status.desktop_version}`,
    status.engine_version && `Engine ${status.engine_version}`,
    compose && `Compose ${compose}`,
  ]
    .filter(Boolean)
    .join(" · ");
  if (!compose) {
    return (
      <Notice tone="warn" title="Docker Compose wasn't found">
        <p>
          Docker is running, but Alchemy needs Docker Compose to run apps. Docker Desktop includes
          it; on Linux, install the <Code>docker-compose-plugin</Code> package.
        </p>
        {versions && <p>{versions}</p>}
      </Notice>
    );
  }
  return (
    <Notice tone="ok" title="Docker is ready">
      <p>{versions}</p>
    </Notice>
  );
}
