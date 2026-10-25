import { useQuery } from "@tanstack/react-query";

import { ApiError, type EngineStatus } from "../lib/api.ts";
import { type DockerStart, isDockerAway, useDockerStart } from "../lib/engineStart.ts";
import { useDaemonConnection } from "../lib/live.tsx";
import { liveQueries } from "../lib/queries.ts";
import { Button } from "./Button.tsx";
import { Code } from "./Code.tsx";
import { CopyButton } from "./CopyButton.tsx";
import { ExternalLink } from "./ExternalLink.tsx";
import { Notice } from "./Notice.tsx";
import { ProgressSteps, type Step } from "./ProgressSteps.tsx";

/** Where Docker's own install guide lives; shown when Docker is missing. */
const GET_DOCKER_URL = "https://docs.docker.com/get-started/get-docker/";

/** The command that starts Docker Engine on most Linux systems. */
const START_ENGINE = "sudo systemctl start docker";

/**
 * Docker's state as the daemon last reported it. `null` means no report
 * yet, or the event stream dropped and is reconnecting. Docker Desktop that
 * isn't running gets a Start Docker Desktop button, and its progress until
 * Docker is ready.
 */
export function EngineStatusView({ status }: { status: EngineStatus | null }) {
  const desktop = useDockerStart();
  if (!status) {
    return <Notice tone="busy" title="Checking Docker…" />;
  }
  switch (status.state) {
    case "not_installed":
      return (
        <Notice tone="error" title="Docker isn't installed">
          <p>Alchemy runs your apps with Docker. Install Docker Desktop, then open it.</p>
          <p>
            <ExternalLink href={GET_DOCKER_URL}>
              docs.docker.com/get-started/get-docker
            </ExternalLink>
          </p>
        </Notice>
      );
    case "not_running":
      return <NotRunning status={status} desktop={desktop} />;
    case "starting":
      if (desktop.starting) {
        return <DesktopStarting status={status} />;
      }
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

/**
 * Docker's state where apps are listed, shown only while Docker can't run
 * them, so the reason they're waiting is in sight and Docker Desktop can be
 * started from there.
 */
export function DockerAwayNotice() {
  const daemon = useDaemonConnection();
  const engine = useQuery({ ...liveQueries.engine, enabled: daemon.kind === "online" });
  if (daemon.kind !== "online" || !isDockerAway(engine.data)) {
    return null;
  }
  return (
    <div
      role="status"
      aria-label="Docker"
      className="rounded-lg border border-border bg-surface-raised p-4"
    >
      <EngineStatusView status={engine.data ?? null} />
    </div>
  );
}

function NotRunning({ status, desktop }: { status: EngineStatus; desktop: DockerStart }) {
  const where = status.endpoint && <p>Looking for Docker at {status.endpoint}</p>;
  switch (status.flavor) {
    case "docker_desktop":
      if (desktop.starting) {
        return <DesktopStarting status={status} />;
      }
      return (
        <Notice
          tone={desktop.timedOut ? "warn" : "error"}
          title={
            desktop.timedOut
              ? "Docker Desktop hasn't started yet"
              : "Docker Desktop isn't running. Start it to continue."
          }
          action={
            <Button variant="primary" disabled={desktop.sending} onClick={desktop.start}>
              Start Docker Desktop
            </Button>
          }
        >
          {desktop.timedOut && (
            <p>
              It's been more than two minutes. Check Docker Desktop for a message that needs you,
              then try again.
            </p>
          )}
          {where}
          <StartAnswer desktop={desktop} />
        </Notice>
      );
    case "docker_engine":
      return (
        <Notice tone="error" title="Docker Engine isn't running. Start it to continue.">
          <p>
            On most Linux systems: <Code>{START_ENGINE}</Code>
          </p>
          <div>
            <CopyButton text={START_ENGINE} label="Copy command" />
          </div>
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

/** Docker Desktop on its way up, step by step, after Start Docker Desktop. */
function DesktopStarting({ status }: { status: EngineStatus }) {
  const answering = status.state === "starting";
  const steps: Step[] = [
    { id: "open", label: "Open Docker Desktop", state: "done" },
    {
      id: "answer",
      label: answering ? "Docker answered" : "Wait for Docker to answer…",
      state: answering ? "done" : "active",
    },
    {
      id: "ready",
      label: answering ? "Wait for Docker to be ready…" : "Wait for Docker to be ready",
      state: answering ? "active" : "pending",
    },
  ];
  return (
    <Notice tone="busy" title="Starting Docker Desktop…">
      <p>This can take a minute. Your apps come back once Docker is ready.</p>
      <ProgressSteps label="Starting Docker Desktop" steps={steps} />
    </Notice>
  );
}

/** What alchemyd said to Start Docker Desktop, when it didn't start it. */
function StartAnswer({ desktop }: { desktop: DockerStart }) {
  const { answer, error } = desktop;
  if (error !== null) {
    const missing = error instanceof ApiError && error.code === "unsupported";
    return (
      <div role="alert">
        <Notice tone="error" title="Alchemy couldn't start Docker Desktop">
          <p>{sentence(error instanceof ApiError ? error.message : "Something went wrong")}</p>
          {missing && (
            <p>
              <ExternalLink href={GET_DOCKER_URL}>
                docs.docker.com/get-started/get-docker
              </ExternalLink>
            </p>
          )}
        </Notice>
      </div>
    );
  }
  if (answer?.result !== "unsupported") {
    return null;
  }
  return (
    <>
      <p>{sentence(answer.message)}</p>
      {answer.command !== null && (
        <>
          <pre className="overflow-x-auto rounded bg-surface-muted p-2 text-xs text-text">
            {answer.command}
          </pre>
          <div>
            <CopyButton text={answer.command} label="Copy command" />
          </div>
        </>
      )}
    </>
  );
}

/** `text` with a capital and a full stop, as alchemyd's messages have neither. */
function sentence(text: string): string {
  const trimmed = text.trim();
  if (trimmed === "") {
    return trimmed;
  }
  const capital = trimmed.charAt(0).toUpperCase() + trimmed.slice(1);
  return /[.!?…]$/.test(capital) ? capital : `${capital}.`;
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
