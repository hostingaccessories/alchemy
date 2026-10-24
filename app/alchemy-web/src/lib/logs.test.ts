import { describe, expect, it } from "vite-plus/test";

import { type LogsQuery, logsPath } from "./logs.ts";

describe("logsPath", () => {
  const cases: [string, LogsQuery, string][] = [
    ["blog", { follow: false }, "/apps/blog/logs"],
    ["blog", { follow: true }, "/apps/blog/logs?follow=true"],
    [
      "blog",
      { service: "web", tail: 50, follow: true },
      "/apps/blog/logs?service=web&tail=50&follow=true",
    ],
    ["my app", { service: "a&b", follow: false }, "/apps/my%20app/logs?service=a%26b"],
    ["blog", { tail: 0, follow: false }, "/apps/blog/logs?tail=0"],
  ];
  it.each(cases)("%s %o", (app, query, want) => {
    expect(logsPath(app, query)).toBe(want);
  });
});
