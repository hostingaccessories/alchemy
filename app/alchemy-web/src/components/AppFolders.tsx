import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, X } from "lucide-react";
import { type FormEvent, useState } from "react";

import { type AppSettings, api } from "../lib/api.ts";
import { useDaemonConnection } from "../lib/live.tsx";
import { appSettingsQuery } from "../lib/queries.ts";
import { Button } from "./Button.tsx";
import { ErrorState } from "./ErrorState.tsx";
import { Loading } from "./Loading.tsx";
import { Notice } from "./Notice.tsx";

const INPUT =
  "min-w-0 flex-1 rounded-md border border-border bg-surface px-3 py-1.5 text-sm focus-visible:ring-2 focus-visible:ring-accent focus-visible:outline-none";

/**
 * Where alchemyd looks for apps: the home, where new apps go, and any other
 * folders, kept in the daemon. Nothing in the folders is changed by saving.
 */
export function AppFolders() {
  const daemon = useDaemonConnection();
  const settings = useQuery({ ...appSettingsQuery, enabled: daemon.kind === "online" });

  if (daemon.kind !== "online" && settings.data === undefined) {
    return <p className="text-muted">Your app folders appear once alchemyd is reachable.</p>;
  }
  if (settings.isError) {
    return (
      <ErrorState
        error={settings.error}
        title="Couldn't load your app folders"
        onRetry={() => void settings.refetch()}
      />
    );
  }
  if (settings.data === undefined) {
    return <Loading label="Loading your app folders…" />;
  }
  return <AppFoldersForm saved={settings.data} />;
}

function AppFoldersForm({ saved }: { saved: AppSettings }) {
  const client = useQueryClient();
  const [home, setHome] = useState(saved.home ?? "");
  const [extra, setExtra] = useState<string[]>(saved.extra);
  const [done, setDone] = useState(false);
  const save = useMutation({
    mutationFn: api.setAppSettings,
    onSuccess: (next) => {
      client.setQueryData(appSettingsQuery.queryKey, next);
      setDone(true);
    },
  });

  const submit = (event: FormEvent) => {
    event.preventDefault();
    setDone(false);
    const trimmed = home.trim();
    save.mutate({
      home: trimmed === "" || trimmed === saved.default_home ? null : trimmed,
      extra: extra.map((dir) => dir.trim()).filter((dir) => dir !== ""),
    });
  };

  return (
    <form onSubmit={submit} className="flex flex-col gap-4">
      <div className="flex flex-col gap-1">
        <label htmlFor="app-home" className="text-sm font-medium">
          Home folder
        </label>
        <p className="text-sm text-muted">
          New apps go here, and Alchemy looks here first. Each app is a folder with a compose file
          in it.
        </p>
        <div className="flex gap-2">
          <input
            id="app-home"
            className={INPUT}
            value={home}
            onChange={(e) => setHome(e.target.value)}
            placeholder={saved.default_home ?? "/home/you/Alchemy/apps"}
            spellCheck={false}
          />
          {saved.default_home && home !== saved.default_home && (
            <Button onClick={() => setHome(saved.default_home ?? "")}>Use the default</Button>
          )}
        </div>
      </div>
      <fieldset className="flex flex-col gap-2">
        <legend className="text-sm font-medium">Other app folders</legend>
        <p className="text-sm text-muted">
          Folders that already hold apps, such as ones you run with docker compose by hand. Alchemy
          only reads them.
        </p>
        {extra.map((dir, index) => (
          <div key={index} className="flex gap-2">
            <input
              aria-label={`Other app folder ${index + 1}`}
              className={INPUT}
              value={dir}
              onChange={(e) => setExtra(extra.map((d, i) => (i === index ? e.target.value : d)))}
              placeholder="/opt/stacks"
              spellCheck={false}
            />
            <Button
              aria-label={`Remove ${dir || `folder ${index + 1}`}`}
              onClick={() => setExtra(extra.filter((_, i) => i !== index))}
            >
              <X className="size-4" aria-hidden />
            </Button>
          </div>
        ))}
        <div>
          <Button onClick={() => setExtra([...extra, ""])}>
            <Plus className="size-4" aria-hidden />
            Add a folder
          </Button>
        </div>
      </fieldset>
      <div className="flex items-center gap-3">
        <Button type="submit" variant="primary" disabled={save.isPending}>
          Save
        </Button>
      </div>
      <div role="status" aria-label="App folders saved">
        {done && !save.isPending && (
          <Notice tone="ok" title="Saved. Alchemy is looking in these folders now." />
        )}
      </div>
      {save.isError && (
        <div role="alert">
          <Notice tone="error" title="Couldn't save your app folders">
            <p>{save.error.message}</p>
          </Notice>
        </div>
      )}
    </form>
  );
}
