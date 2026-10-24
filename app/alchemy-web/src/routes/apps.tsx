import { useQuery } from "@tanstack/react-query";
import { Link, createRoute } from "@tanstack/react-router";
import { Boxes, ChevronRight } from "lucide-react";

import { AppStatus } from "../components/AppStatus.tsx";
import { EmptyState } from "../components/EmptyState.tsx";
import { ErrorState } from "../components/ErrorState.tsx";
import { Loading } from "../components/Loading.tsx";
import { Notice } from "../components/Notice.tsx";
import { PageHeader } from "../components/PageHeader.tsx";
import type { App, AppDir, AppList, UnmanagedApp } from "../lib/api.ts";
import { useDaemonConnection } from "../lib/live.tsx";
import { liveQueries } from "../lib/queries.ts";
import { rootRoute } from "./root.tsx";

/** The apps in the app folders, live, and running projects Alchemy doesn't manage. */
export const appsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/apps",
  component: AppsPage,
});

function AppsPage() {
  const daemon = useDaemonConnection();
  const apps = useQuery({ ...liveQueries.apps, enabled: daemon.kind === "online" });

  return (
    <div className="flex max-w-3xl flex-col gap-8">
      <PageHeader title="Apps" />
      {daemon.kind !== "online" && apps.data === undefined ? (
        <p className="text-muted">Your apps appear once alchemyd is reachable.</p>
      ) : apps.isError ? (
        <ErrorState
          error={apps.error}
          title="Couldn't load your apps"
          onRetry={() => void apps.refetch()}
        />
      ) : apps.data === undefined ? (
        <Loading label="Loading your apps…" />
      ) : (
        <AppsView list={apps.data} />
      )}
    </div>
  );
}

function AppsView({ list }: { list: AppList }) {
  const home = list.dirs.find((dir) => dir.home);
  return (
    <>
      {list.apps.length === 0 ? (
        <EmptyState
          icon={Boxes}
          title="No apps yet"
          action={
            <Link to="/settings" className="text-accent underline">
              Choose where Alchemy looks for apps
            </Link>
          }
        >
          {home
            ? `Alchemy looks for apps in ${home.path}. Each app is a folder there with a compose file in it.`
            : "Tell Alchemy which folder holds your apps in Settings."}
        </EmptyState>
      ) : (
        <ul aria-label="Your apps" className="flex flex-col gap-2">
          {list.apps.map((app) => (
            <AppRow key={app.dir} app={app} />
          ))}
        </ul>
      )}
      <FolderProblems dirs={list.dirs} />
      {list.unmanaged.length > 0 && <Unmanaged projects={list.unmanaged} />}
    </>
  );
}

function AppRow({ app }: { app: App }) {
  return (
    <li>
      <Link
        to="/apps/$name"
        params={{ name: app.name }}
        className="flex items-center gap-4 rounded-lg border border-border bg-surface-raised px-4 py-3 hover:bg-surface-muted"
      >
        <div className="flex min-w-0 flex-1 flex-col">
          <span className="font-medium">{app.name}</span>
          <span className="truncate text-sm text-muted">{app.dir}</span>
          {app.problems.length > 0 && (
            <span className="text-sm text-warn">{app.problems[0]?.message}</span>
          )}
        </div>
        <AppStatus status={app.status} />
        <ChevronRight className="size-4 text-muted" aria-hidden />
      </Link>
    </li>
  );
}

/**
 * Folders that can't be read, and folders inside them that look like apps
 * but were skipped. A missing home is normal before the first app.
 */
function FolderProblems({ dirs }: { dirs: AppDir[] }) {
  const broken = dirs.filter((dir) => dir.state !== "ok" && !(dir.home && dir.state === "missing"));
  const skipped = dirs.flatMap((dir) => dir.skipped);
  if (broken.length === 0 && skipped.length === 0) {
    return null;
  }
  return (
    <section aria-label="Folders that need a look" className="flex flex-col gap-3">
      {broken.map((dir) => (
        <Notice key={dir.path} tone="warn" title={`Alchemy can't use ${dir.path}`}>
          <p>{dir.message}</p>
          <p>Change your app folders in Settings.</p>
        </Notice>
      ))}
      {skipped.length > 0 && (
        <Notice
          tone="warn"
          title={
            skipped.length === 1
              ? "1 folder wasn't added as an app"
              : `${skipped.length} folders weren't added as apps`
          }
        >
          <ul className="flex flex-col gap-1">
            {skipped.map((entry) => (
              <li key={entry.path}>
                <span className="font-medium text-text">{entry.path}</span>: {entry.message}
              </li>
            ))}
          </ul>
        </Notice>
      )}
    </section>
  );
}

function Unmanaged({ projects }: { projects: UnmanagedApp[] }) {
  return (
    <section aria-labelledby="unmanaged" className="flex flex-col gap-3">
      <div>
        <h2 id="unmanaged" className="text-base font-semibold">
          Running outside your app folders
        </h2>
        <p className="text-sm text-muted">
          Alchemy shows these but doesn't manage them. To manage one, add the folder it runs from in
          Settings.
        </p>
      </div>
      <ul className="flex flex-col gap-2">
        {projects.map((project) => (
          <li
            key={project.project}
            className="flex items-center gap-4 rounded-lg border border-border px-4 py-3"
          >
            <div className="flex min-w-0 flex-1 flex-col">
              <span className="font-medium">{project.project}</span>
              <span className="truncate text-sm text-muted">{project.dir ?? "Folder unknown"}</span>
            </div>
            <AppStatus status={project.status} />
          </li>
        ))}
      </ul>
    </section>
  );
}
