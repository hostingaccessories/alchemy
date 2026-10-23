import { Boxes, Plus, Trash2 } from "lucide-react";
import { type ReactNode, useState } from "react";

import { Advanced } from "../components/Advanced.tsx";
import { Button } from "../components/Button.tsx";
import { Code } from "../components/Code.tsx";
import { ConfirmDialog } from "../components/ConfirmDialog.tsx";
import { CopyButton } from "../components/CopyButton.tsx";
import { EmptyState } from "../components/EmptyState.tsx";
import { ErrorState } from "../components/ErrorState.tsx";
import { ExternalLink } from "../components/ExternalLink.tsx";
import { Loading } from "../components/Loading.tsx";
import { Notice } from "../components/Notice.tsx";
import { PageHeader } from "../components/PageHeader.tsx";
import { ProgressSteps, type Step } from "../components/ProgressSteps.tsx";
import { ApiError, NO_RESPONSE } from "../lib/api.ts";

const PULL_OUTPUT = `[+] Pulling 3/3
 ✔ db Pulled                 4.2s
 ✔ redis Pulled              2.1s
 ✔ app Pulled               11.8s`;

const STEPS: Step[] = [
  { id: "check", label: "Check Docker", state: "done" },
  { id: "pull", label: "Download Nextcloud", state: "done", output: PULL_OUTPUT },
  {
    id: "start",
    label: "Start Nextcloud",
    state: "failed",
    message: "Port 8080 is already in use by another app.",
    output: "Error response from daemon: driver failed programming external connectivity",
  },
  { id: "address", label: "Give it a web address", state: "skipped" },
];

const RUNNING: Step[] = [
  { id: "check", label: "Check Docker", state: "done" },
  { id: "pull", label: "Download Nextcloud", state: "active", message: "212 MB of 540 MB" },
  { id: "start", label: "Start Nextcloud", state: "pending" },
];

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section aria-label={title} className="flex flex-col gap-3">
      <h2 className="border-b border-border pb-1 text-base font-semibold">{title}</h2>
      <div className="flex flex-col gap-4">{children}</div>
    </section>
  );
}

function ThrowOnRender(): never {
  throw new Error("The gallery threw on purpose");
}

/**
 * Every shared component in each of its states, for building and reviewing
 * them side by side. Registered only in dev builds, at `/dev/components`.
 */
export function ComponentGallery() {
  const [broken, setBroken] = useState(false);
  const wait = () => new Promise<void>((resolve) => setTimeout(resolve, 1000));

  return (
    <div className="flex max-w-2xl flex-col gap-8">
      <PageHeader title="Components" />
      {broken && <ThrowOnRender />}

      <Section title="Notice">
        <Notice tone="ok" title="Docker is ready">
          <p>Docker Desktop 4.93.0 · Engine 29.8.1 · Compose 2.40.0</p>
        </Notice>
        <Notice tone="warn" title="Docker Compose wasn't found">
          <p>
            Install the <Code>docker-compose-plugin</Code> package.
          </p>
        </Notice>
        <Notice
          tone="error"
          title="Nextcloud stopped"
          action={<Button>Start it</Button>}
          detail={<Code>exited with code 137</Code>}
        >
          <p>It ran out of memory. Start it again, or give it more memory under Advanced.</p>
        </Notice>
        <Notice tone="busy" title="Docker Desktop is starting…" />
      </Section>

      <Section title="Buttons">
        <div className="flex flex-wrap gap-2">
          <Button variant="primary">
            <Plus className="size-4" aria-hidden />
            Add app
          </Button>
          <Button>Cancel</Button>
          <Button variant="danger">Remove app</Button>
          <Button disabled>Disabled</Button>
          <CopyButton text="alchemyd 0.1.0 on macOS" label="Copy diagnostics" />
        </div>
      </Section>

      <Section title="ProgressSteps">
        <ProgressSteps label="Installing Nextcloud" steps={RUNNING} />
        <ProgressSteps label="Installing Nextcloud (failed)" steps={STEPS} />
      </Section>

      <Section title="Advanced">
        <Advanced>
          <p className="text-sm text-muted">
            Compose file: <Code>~/Alchemy/apps/nextcloud/compose.yaml</Code>
          </p>
        </Advanced>
      </Section>

      <Section title="ExternalLink">
        <p className="text-sm text-muted">
          Opens in the default browser:{" "}
          <ExternalLink href="https://docs.docker.com/get-started/get-docker/">
            docs.docker.com/get-started/get-docker
          </ExternalLink>
        </p>
      </Section>

      <Section title="ConfirmDialog">
        <div className="flex flex-wrap gap-2">
          <ConfirmDialog
            trigger={<Button>Stop Nextcloud</Button>}
            title="Stop Nextcloud?"
            description="People using it lose access until you start it again."
            confirmLabel="Stop app"
            onConfirm={wait}
          />
          <ConfirmDialog
            trigger={
              <Button variant="danger">
                <Trash2 className="size-4" aria-hidden />
                Delete Nextcloud
              </Button>
            }
            title="Delete Nextcloud and its data?"
            description={
              <>
                <p>Its files, database, and settings are deleted from this machine.</p>
                <p>This can't be undone.</p>
              </>
            }
            confirmLabel="Delete app and data"
            typeToConfirm="nextcloud"
            onConfirm={async () => {
              await wait();
              throw new ApiError(409, "Nextcloud is still starting. Try again in a moment.");
            }}
          />
        </div>
      </Section>

      <Section title="EmptyState">
        <EmptyState
          icon={Boxes}
          title="No apps yet"
          action={
            <Button variant="primary">
              <Plus className="size-4" aria-hidden />
              Add app
            </Button>
          }
        >
          Apps you add to Alchemy will show up here.
        </EmptyState>
      </Section>

      <Section title="Loading">
        <Loading label="Loading apps…" />
      </Section>

      <Section title="ErrorState">
        <ErrorState
          error={new ApiError(NO_RESPONSE, "alchemyd is not reachable")}
          onRetry={() => {}}
        />
        <ErrorState
          error={new ApiError(500, "Docker returned an error: no space left on device")}
          title="Couldn't load your apps"
          onRetry={() => {}}
        />
        <ErrorState error={new TypeError("Cannot read properties of undefined")} />
      </Section>

      <Section title="Route error boundary">
        <div>
          <Button variant="danger" onClick={() => setBroken(true)}>
            Break this page
          </Button>
        </div>
      </Section>
    </div>
  );
}
