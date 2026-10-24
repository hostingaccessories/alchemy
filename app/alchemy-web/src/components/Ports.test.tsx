import { describe, expect, it } from "vite-plus/test";

import type { PortHolder, PublishedPort } from "../lib/api.ts";
import { addressLabel, holderLabel, portLabel, sourceLabel } from "./Ports.tsx";

const tcp: PublishedPort = { protocol: "tcp", host_ip: null, published: "8080", target: "80" };

describe("port labels", () => {
  it.each<[PublishedPort, string, string]>([
    [tcp, "8080/tcp", "All"],
    [
      { protocol: "udp", host_ip: "127.0.0.1", published: "8000-8010", target: null },
      "8000-8010/udp",
      "127.0.0.1",
    ],
    [{ ...tcp, host_ip: "::", published: null }, "any/tcp", "::"],
  ])("labels %o", (port, label, address) => {
    expect(portLabel(port)).toBe(label);
    expect(addressLabel(port)).toBe(address);
  });

  it("names each source", () => {
    expect(sourceLabel("live")).toBe("In use");
    expect(sourceLabel("reserved")).toBe("Reserved");
    expect(sourceLabel("declared")).toBe("Declared");
    expect(sourceLabel("other" as "live")).toBe("Unknown");
  });

  it.each<[PortHolder, string]>([
    [{ kind: "app", app: "blog", service: "web" }, "web of blog"],
    [{ kind: "container", name: "pihole" }, "Container pihole"],
    [{ kind: "reservation", owner: "proxy" }, "Reserved for proxy"],
    [{ kind: "program", process: "nc", pid: 42 }, "nc (pid 42)"],
    [{ kind: "program", process: "nc", pid: null }, "nc"],
    [{ kind: "program", process: null, pid: 42 }, "Another program (pid 42)"],
    [{ kind: "program", process: null, pid: null }, "Another program"],
    [
      { kind: "vm" } as unknown as PortHolder,
      "Something this version of Alchemy doesn't recognize",
    ],
  ])("names holder %o", (holder, label) => {
    expect(holderLabel(holder)).toBe(label);
  });
});
