import { apiBase } from "./platform.ts";

/** Mirrors `alchemy_api::HealthResponse`. */
export interface HealthResponse {
  status: "ok";
}

/** Mirrors `alchemy_api::VersionResponse`. */
export interface VersionResponse {
  version: string;
}

async function getJson<T>(path: string, signal?: AbortSignal): Promise<T> {
  const res = await fetch(`${apiBase()}/api${path}`, { signal });
  if (!res.ok) {
    throw new Error(`GET /api${path} failed: ${res.status}`);
  }
  return (await res.json()) as T;
}

export const api = {
  health: (signal?: AbortSignal) => getJson<HealthResponse>("/health", signal),
  version: (signal?: AbortSignal) => getJson<VersionResponse>("/version", signal),
};
