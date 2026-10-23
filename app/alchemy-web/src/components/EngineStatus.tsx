import type { EngineStatus } from "../lib/api.ts";
import { Code } from "./Code.tsx";
import { Notice } from "./Notice.tsx";

/** Where Docker's own install guide lives; shown when Docker is missing. */
const GET_DOCKER_URL = "https://docs.docker.com/get-started/get-docker/";

/**
 * Docker's state as the daemon last reported it. `null` means no report
 * yet, or the event stream dropped and is reconnecting.
 */
export function EngineStatusView({ status }: { status: EngineStatus | null }) {
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
              className="text-accent underline"
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
