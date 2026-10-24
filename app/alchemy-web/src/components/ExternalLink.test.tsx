import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import { AddressLink, ExternalLink } from "./ExternalLink.tsx";

const openUrl = vi.hoisted(() => vi.fn(async (_url: string) => {}));

vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl }));

let open: ReturnType<typeof vi.fn>;

beforeEach(() => {
  open = vi.fn();
  vi.stubGlobal("open", open);
});

afterEach(() => {
  cleanup();
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

function renderLink() {
  render(<ExternalLink href="https://docs.docker.com/get-started/">Get Docker</ExternalLink>);
  return screen.getByRole("link", { name: "Get Docker" });
}

describe("ExternalLink", () => {
  it("keeps a real href, without an opener or referrer", () => {
    const link = renderLink();
    expect(link.getAttribute("href")).toBe("https://docs.docker.com/get-started/");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toBe("noopener noreferrer");
  });

  it("opens a new tab in a browser", async () => {
    await userEvent.setup().click(renderLink());
    expect(open).toHaveBeenCalledWith(
      "https://docs.docker.com/get-started/",
      "_blank",
      "noopener,noreferrer",
    );
    expect(openUrl).not.toHaveBeenCalled();
  });

  it("opens the default browser from the desktop shell", async () => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    await userEvent.setup().click(renderLink());
    await vi.waitFor(() =>
      expect(openUrl).toHaveBeenCalledWith("https://docs.docker.com/get-started/"),
    );
    expect(open).not.toHaveBeenCalled();
  });
});

describe("AddressLink", () => {
  function renderAddress(href: string) {
    render(<AddressLink href={href}>Open blog</AddressLink>);
    return screen.getByRole("link", { name: "Open blog" });
  }

  it("opens an app's http: address in a new tab", async () => {
    const link = renderAddress("http://blog.localhost/");
    expect(link.getAttribute("rel")).toBe("noopener noreferrer");
    await userEvent.setup().click(link);
    expect(open).toHaveBeenCalledWith("http://blog.localhost/", "_blank", "noopener,noreferrer");
  });

  it("opens the default browser from the desktop shell", async () => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    await userEvent.setup().click(renderAddress("http://192.168.1.20:8100/"));
    await vi.waitFor(() => expect(openUrl).toHaveBeenCalledWith("http://192.168.1.20:8100/"));
    expect(open).not.toHaveBeenCalled();
  });

  it("opens nothing for another scheme", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    await userEvent.setup().click(renderAddress("file:///etc/passwd"));
    await vi.waitFor(() => expect(error).toHaveBeenCalled());
    expect(open).not.toHaveBeenCalled();
    expect(openUrl).not.toHaveBeenCalled();
    error.mockRestore();
  });
});
