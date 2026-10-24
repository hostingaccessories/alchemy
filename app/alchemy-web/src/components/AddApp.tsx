import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";

import {
  type App,
  type AppDir,
  type AppList,
  ApiError,
  type NewApp,
  type Saved,
  api,
} from "../lib/api.ts";
import { liveQueries, showOperation } from "../lib/queries.ts";
import { Advanced } from "./Advanced.tsx";
import { Button } from "./Button.tsx";
import { FILE_TEXTAREA } from "./AppFileEditor.tsx";
import { Notice } from "./Notice.tsx";

const INPUT =
  "min-w-0 rounded-md border border-border bg-surface px-3 py-1.5 text-sm focus-visible:ring-2 focus-visible:ring-accent focus-visible:outline-none";

/** What the compose file starts as: one web server, to change. */
export const STARTER_COMPOSE = `services:
  web:
    image: nginx:alpine
    ports:
      - "8080:80"
`;

/**
 * A new app from a name and a compose file. alchemyd checks the file with
 * Docker Compose first and creates the app's folder only if Compose accepts
 * it and no folder of that name exists; on success this opens the new app.
 * Create and start also starts it, and the app's page shows the progress.
 * If the app was created but couldn't be started, this says why instead,
 * with a way to open it.
 */
export function AddApp({ dirs, onCancel }: { dirs: AppDir[]; onCancel: () => void }) {
  const client = useQueryClient();
  const navigate = useNavigate();
  const home = dirs.find((dir) => dir.home)?.path ?? "";
  const [name, setName] = useState("");
  const [folder, setFolder] = useState(home);
  const [compose, setCompose] = useState(STARTER_COMPOSE);
  const [env, setEnv] = useState("");
  const [unstarted, setUnstarted] = useState<{ app: App; error: string } | null>(null);
  const create = useMutation({
    mutationFn: api.createApp,
    onSuccess: ({ operation, start_error, ...app }: Saved<App>) => {
      client.setQueryData<AppList>(liveQueries.apps.queryKey, (list) =>
        list && !list.apps.some((a) => a.name === app.name)
          ? { ...list, apps: [...list.apps, app] }
          : list,
      );
      if (operation) showOperation(client, operation);
      if (start_error) {
        setUnstarted({ app, error: start_error.error });
        return;
      }
      openApp(app);
    },
  });
  const openApp = (app: App) => void navigate({ to: "/apps/$name", params: { name: app.name } });

  const send = (start: boolean) => {
    const body: NewApp = {
      name: name.trim(),
      folder: folder === home ? null : folder,
      compose,
      env: env === "" ? null : env,
      dry_run: false,
    };
    if (start) body.start = true;
    create.mutate(body);
  };
  const submit = (event: FormEvent) => {
    event.preventDefault();
    send(false);
  };
  const failure = create.error instanceof ApiError ? create.error : null;
  const disabled = create.isPending || unstarted !== null || name.trim() === "";

  return (
    <form
      onSubmit={submit}
      aria-label="Add an app"
      className="flex flex-col gap-4 rounded-lg border border-border bg-surface-raised p-4"
    >
      <div className="flex flex-col gap-1">
        <label htmlFor="new-app-name" className="text-sm font-medium">
          Name
        </label>
        <p className="text-sm text-muted">
          Lowercase letters, digits, - and _. It becomes the app's folder name.
        </p>
        <input
          id="new-app-name"
          className={INPUT}
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="blog"
          spellCheck={false}
          autoCapitalize="off"
          required
        />
      </div>
      {dirs.length > 1 && (
        <div className="flex flex-col gap-1">
          <label htmlFor="new-app-folder" className="text-sm font-medium">
            App folder
          </label>
          <select
            id="new-app-folder"
            className={INPUT}
            value={folder}
            onChange={(e) => setFolder(e.target.value)}
          >
            {dirs.map((dir) => (
              <option key={dir.path} value={dir.path}>
                {dir.home ? `${dir.path} (home)` : dir.path}
              </option>
            ))}
          </select>
        </div>
      )}
      <div className="flex flex-col gap-1">
        <label htmlFor="new-app-compose" className="text-sm font-medium">
          Compose file
        </label>
        <textarea
          id="new-app-compose"
          className={FILE_TEXTAREA}
          value={compose}
          onChange={(e) => setCompose(e.target.value)}
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
        />
      </div>
      <Advanced label="Environment file (.env)">
        <label htmlFor="new-app-env" className="text-sm text-muted">
          Variables for the compose file, saved so only you can read them.
        </label>
        <textarea
          id="new-app-env"
          className={FILE_TEXTAREA}
          value={env}
          onChange={(e) => setEnv(e.target.value)}
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
          placeholder="NAME=value"
        />
      </Advanced>
      <div className="flex items-center gap-3">
        <Button type="submit" variant="primary" disabled={disabled}>
          Create app
        </Button>
        <Button disabled={disabled} onClick={() => send(true)}>
          Create and start
        </Button>
        <Button onClick={onCancel}>Cancel</Button>
      </div>
      {unstarted && (
        <div role="alert">
          <Notice
            tone="warn"
            title={`Created ${unstarted.app.name}, but couldn't start it`}
            action={
              <Button onClick={() => openApp(unstarted.app)}>Open {unstarted.app.name}</Button>
            }
          >
            <p>{unstarted.error}</p>
          </Notice>
        </div>
      )}
      {create.isError && (
        <div role="alert">
          <Notice
            tone="error"
            title={
              failure?.status === 422
                ? "Alchemy couldn't create this app"
                : failure?.status === 409
                  ? "That name is taken"
                  : "Couldn't create the app"
            }
          >
            <pre className="font-mono text-xs whitespace-pre-wrap text-text">
              {create.error.message}
            </pre>
          </Notice>
        </div>
      )}
    </form>
  );
}
