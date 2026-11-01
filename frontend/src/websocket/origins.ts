// Where this page finds its server.
//
// Two deployment modes:
//   * single port — simccxt-server served this page, so the WebSocket, the
//     diary API and the market-data fallback proxy all live on this origin,
//     whatever port the server was started on (`--port 19010`) and even behind
//     a proxy (the origin comes from the request's own `Host` header);
//   * vite dev — the page is on :5173 and the Rust server on :9001, so the
//     WebSocket is :9001 and the HTTP API :9002 (see `apiBaseFrom`).
//
// This module deliberately imports nothing so both the socket layer and the
// market-data layer can use it without a cycle.

import { apiBaseFrom } from "../types/protocol.ts";

declare global {
  interface Window {
    /** Origin injected by simccxt-server into index.html (single-port mode). */
    __SIMCCXT_ORIGIN__?: string;
  }
}

/** Origin of the page when the server served it, else `undefined` (vite dev). */
export function injectedOrigin(): string | undefined {
  const origin = window.__SIMCCXT_ORIGIN__;
  return typeof origin === "string" && origin !== "" ? origin : undefined;
}

export function wsUrl(): string {
  const env = (import.meta as unknown as { env?: Record<string, string | undefined> }).env;
  if (env && env["VITE_WS_URL"]) return env["VITE_WS_URL"] as string;
  // The server that served this page also serves the WebSocket, so the page's
  // own origin is the answer — whatever port it was started on (`--port
  // 19010`), and behind a proxy too. This must be checked BEFORE the
  // localhost heuristic: in single-port mode the page is often 127.0.0.1.
  const injected = injectedOrigin();
  if (injected) {
    try {
      const url = new URL(injected);
      url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
      return url.origin;
    } catch {
      // fall through to the dev heuristics
    }
  }
  // Local dev/preview (vite serves the page, Rust serves WS :9001):
  // never point WS at the page's own port — handshake would fail forever.
  const host = window.location.hostname;
  if (host === "localhost" || host === "127.0.0.1" || host === "::1" || host === "") {
    return "ws://127.0.0.1:9001";
  }
  const proto = window.location.protocol === "https:" ? "wss" : "ws";
  return proto + "://" + window.location.host;
}

/**
 * Base URL of the server's HTTP API: the diary endpoints and the market-data
 * fallback proxy. Same origin in single-port mode; `ws_port + 1` in dev.
 */
export function apiUrl(): string {
  const env = (import.meta as unknown as { env?: Record<string, string | undefined> }).env;
  if (env && env["VITE_API_URL"]) return String(env["VITE_API_URL"]).replace(/\/+$/, "");
  const injected = injectedOrigin();
  if (injected) {
    try {
      return new URL(injected).origin;
    } catch {
      // fall through
    }
  }
  return apiBaseFrom(wsUrl());
}
