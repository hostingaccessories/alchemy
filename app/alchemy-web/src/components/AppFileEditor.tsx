import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";

import { type App, type AppFile, type AppFileKind, ApiError, api } from "../lib/api.ts";
import { Advanced } from "./Advanced.tsx";
import { Button } from "./Button.tsx";
import { ErrorState } from "./ErrorState.tsx";
import { Loading } from "./Loading.tsx";
import { Notice } from "./Notice.tsx";

/** The textarea style shared by the file editors. */
export const FILE_TEXTAREA =
  "min-h-64 w-full rounded-md border border-border bg-surface px-3 py-2 font-mono text-sm focus-visible:ring-2 focus-visible:ring-accent focus-visible:outline-none";

/** The query that reads one of an app's files. */
export function appFileQuery(name: string, kind: AppFileKind) {
  return {
    queryKey: ["apps", name, "files", kind] as const,
    queryFn: ({ signal }: { signal: AbortSignal }) => api.appFile(name, kind, signal),
  };
}

/**
 * Edit an app's compose file and `.env` as plain text. alchemyd checks each
 * save with Compose and refuses one that Compose rejects, or one made over
 * a change on disk since the file was opened, so the file is never left
 * broken or a hand edit lost.
 */
export function AppFileEditor({ app }: { app: App }) {
  return (
    <section aria-labelledby="edit" className="flex flex-col gap-3">
      <h2 id="edit" className="text-base font-semibold">
        Edit
      </h2>
      <p className="text-sm text-muted">
        Alchemy checks your change with Docker Compose before saving it, and saves nothing Compose
        rejects.
      </p>
      <Advanced label={`Compose file (${app.compose_file})`}>
        <FileEditor name={app.name} kind="compose" />
      </Advanced>
      <Advanced label="Environment file (.env)">
        <FileEditor name={app.name} kind="env" />
      </Advanced>
    </section>
  );
}

function FileEditor({ name, kind }: { name: string; kind: AppFileKind }) {
  const file = useQuery(appFileQuery(name, kind));
  if (file.isError) {
    return (
      <ErrorState
        error={file.error}
        title="Couldn't open this file"
        onRetry={() => void file.refetch()}
      />
    );
  }
  if (file.data === undefined) {
    return <Loading label="Opening the file…" />;
  }
  return (
    <FileForm name={name} saved={file.data} onReload={async () => (await file.refetch()).data} />
  );
}

function FileForm({
  name,
  saved,
  onReload,
}: {
  name: string;
  saved: AppFile;
  onReload: () => Promise<AppFile | undefined>;
}) {
  const client = useQueryClient();
  const [draft, setDraft] = useState(saved.content);
  const [done, setDone] = useState(false);
  const save = useMutation({
    mutationFn: (content: string) =>
      api.saveAppFile(name, saved.kind, {
        content,
        base_hash: saved.hash,
        dry_run: false,
      }),
    onSuccess: (next) => {
      client.setQueryData(appFileQuery(name, saved.kind).queryKey, next);
      setDone(true);
    },
  });

  const submit = (event: FormEvent) => {
    event.preventDefault();
    setDone(false);
    save.mutate(draft);
  };
  const reload = () => {
    save.reset();
    setDone(false);
    void onReload().then((next) => {
      if (next) setDraft(next.content);
    });
  };
  const failure = save.error instanceof ApiError ? save.error : null;

  return (
    <form onSubmit={submit} className="flex flex-col gap-3">
      <label htmlFor={`file-${saved.kind}`} className="sr-only">
        {saved.name}
      </label>
      <textarea
        id={`file-${saved.kind}`}
        className={FILE_TEXTAREA}
        value={draft}
        onChange={(e) => {
          setDraft(e.target.value);
          setDone(false);
        }}
        spellCheck={false}
        autoCapitalize="off"
        autoCorrect="off"
        placeholder={saved.kind === "env" ? "NAME=value" : undefined}
      />
      <div className="flex items-center gap-3">
        <Button
          type="submit"
          variant="primary"
          disabled={save.isPending || draft === saved.content}
        >
          Save {saved.name}
        </Button>
        {draft !== saved.content && !save.isPending && (
          <Button onClick={() => setDraft(saved.content)}>Undo changes</Button>
        )}
      </div>
      <div role="status" aria-label={`${saved.name} saved`}>
        {done && !save.isPending && <Notice tone="ok" title={`Saved ${saved.name}.`} />}
      </div>
      {save.isError && (
        <div role="alert">
          {failure?.status === 409 ? (
            <Notice
              tone="warn"
              title="This file changed on disk since you opened it"
              action={<Button onClick={reload}>Reload it</Button>}
            >
              <p>
                Nothing was saved. Reload to see the file as it is now, then make your change again.
              </p>
            </Notice>
          ) : failure?.status === 422 ? (
            <Notice tone="error" title={`Compose rejected ${saved.name}, so nothing was saved`}>
              <pre className="font-mono text-xs whitespace-pre-wrap text-text">
                {failure.message}
              </pre>
            </Notice>
          ) : (
            <Notice tone="error" title={`Couldn't save ${saved.name}`}>
              <p>{save.error.message}</p>
            </Notice>
          )}
        </div>
      )}
    </form>
  );
}
