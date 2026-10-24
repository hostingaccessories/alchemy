import { cleanup, fireEvent, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import type { App, AppFile, AppList } from "../lib/api.ts";
import { FakeEventSource, renderAt } from "../test/render.tsx";

const blog: App = {
  name: "blog",
  status: "stopped",
  dir: "/home/me/Alchemy/apps/blog",
  root: "/home/me/Alchemy/apps",
  compose_file: "compose.yaml",
  project: "blog",
  images: "registry",
  services: [],
  problems: [],
};

const apps: AppList = { apps: [blog], unmanaged: [], dirs: [] };

const COMPOSE = "services:\n  web:\n    image: nginx\n";

const file = (content: string, hash: string | null): AppFile => ({
  kind: "compose",
  name: "compose.yaml",
  content,
  hash,
});

let onDisk: AppFile = file(COMPOSE, "h1");
let putReply: () => Response = () => new Response("{}");
const puts: unknown[] = [];

beforeEach(() => {
  onDisk = file(COMPOSE, "h1");
  puts.length = 0;
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init?: RequestInit) => {
      if (url === "/api/apps") {
        return new Response(JSON.stringify(apps));
      }
      if (url === "/api/apps/blog/files/compose") {
        if (init?.method === "PUT") {
          puts.push(JSON.parse(init.body as string));
          return putReply();
        }
        return new Response(JSON.stringify(onDisk));
      }
      if (url === "/api/operations") {
        return new Response(JSON.stringify({ operations: [] }));
      }
      if (url === "/api/metrics") {
        return new Response(JSON.stringify({ projects: [] }));
      }
      return new Response(JSON.stringify({ version: "9.9.9", api: 1 }));
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

async function openCompose(): Promise<HTMLTextAreaElement> {
  await renderAt("/apps/blog");
  const edit = await screen.findByRole("region", { name: "Edit" });
  fireEvent.click(within(edit).getByRole("button", { name: "Compose file (compose.yaml)" }));
  const area = await within(edit).findByLabelText("compose.yaml");
  if (!(area instanceof HTMLTextAreaElement)) throw new Error("not a textarea");
  return area;
}

function error(status: number, code: string, message: string) {
  return () => new Response(JSON.stringify({ error: message, code }), { status });
}

describe("AppFileEditor", () => {
  it("saves an edit against the hash it was opened at", async () => {
    const edited = `${COMPOSE}    restart: always\n`;
    putReply = () => new Response(JSON.stringify(file(edited, "h2")));
    const area = await openCompose();
    expect(area.value).toBe(COMPOSE);
    const save = screen.getByRole("button", { name: "Save compose.yaml" });
    expect(save.hasAttribute("disabled")).toBe(true);

    fireEvent.change(area, { target: { value: edited } });
    fireEvent.click(save);
    expect(await screen.findByText("Saved compose.yaml.")).toBeTruthy();
    expect(puts).toEqual([{ content: edited, base_hash: "h1", dry_run: false }]);
  });

  it("shows Compose's message when it rejects the file", async () => {
    putReply = error(422, "invalid", "services.web Additional property imagee is not allowed");
    const area = await openCompose();
    fireEvent.change(area, { target: { value: "services:\n  web:\n    imagee: x\n" } });
    fireEvent.click(screen.getByRole("button", { name: "Save compose.yaml" }));
    const alert = await screen.findByRole("alert");
    expect(
      within(alert).getByText("Compose rejected compose.yaml, so nothing was saved"),
    ).toBeTruthy();
    expect(
      within(alert).getByText("services.web Additional property imagee is not allowed"),
    ).toBeTruthy();
    expect(area.value).toBe("services:\n  web:\n    imagee: x\n");
  });

  it("says when the file changed on disk and reloads it", async () => {
    putReply = error(409, "conflict", "compose.yaml changed on disk since you opened it.");
    const area = await openCompose();
    fireEvent.change(area, { target: { value: "services: {}\n" } });
    fireEvent.click(screen.getByRole("button", { name: "Save compose.yaml" }));
    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText("This file changed on disk since you opened it")).toBeTruthy();

    onDisk = file("services: {}\n# by hand\n", "h9");
    fireEvent.click(within(alert).getByRole("button", { name: "Reload it" }));
    await vi.waitFor(() => expect(area.value).toBe("services: {}\n# by hand\n"));
    expect(screen.queryByRole("alert")).toBeNull();

    putReply = () => new Response(JSON.stringify(file("x: 1\n", "h10")));
    fireEvent.change(area, { target: { value: "x: 1\n" } });
    fireEvent.click(screen.getByRole("button", { name: "Save compose.yaml" }));
    expect(await screen.findByText("Saved compose.yaml.")).toBeTruthy();
    expect(puts.at(-1)).toEqual({ content: "x: 1\n", base_hash: "h9", dry_run: false });
  });
});
