import type { ClientMessage, ExchangeId, ServerMessage } from "../types/protocol";
import { apiBaseFrom, parseServerMessage } from "../types/protocol";
import { getState, store } from "../state/store";
import { handleLocalCommand } from "../replay/localEngine";

export function wsUrl(): string {
  const env = (import.meta as unknown as { env?: Record<string, string | undefined> }).env;
  if (env && env["VITE_WS_URL"]) return env["VITE_WS_URL"] as string;
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
 * Base URL of the read-only diary HTTP API (see `apiBaseFrom` for the
 * port+1 rule). Kept next to `wsUrl` so both URLs come from one place.
 */
export function apiUrl(): string {
  const env = (import.meta as unknown as { env?: Record<string, string | undefined> }).env;
  if (env && env["VITE_API_URL"]) return String(env["VITE_API_URL"]).replace(/\/+$/, "");
  return apiBaseFrom(wsUrl());
}

export type Route = (m: ServerMessage) => void;

export function subscribeSim(
  symbol: string,
  timeframe: string,
  exchange: ExchangeId,
): void {
  const range = getState().replayWindow;
  getConnection().expectSnapshot();
  sendSim({
    type: "subscribe",
    symbol,
    timeframe,
    exchange,
    ...range,
  });
}

/**
 * Ask Rust to warm the memory and disk caches for the whole watchlist x every timeframe
 * in the background. Unlike a burst of `subscribe`s this touches no session
 * state (no snapshot, no clock reset, no account change): Rust fills caches
 * on a background thread, so switching a coin or timeframe later is instant.
 * The replay date window rides along, so its range cache is warm too.
 */
export function preloadSim(
  symbols: string[],
  timeframes: string[],
  exchange: ExchangeId,
): void {
  if (symbols.length === 0 || timeframes.length === 0) return;
  const range = getState().replayWindow;
  sendSim({
    type: "preload",
    symbols,
    timeframes,
    exchange,
    start_ms: range.start_ms,
    end_ms: range.end_ms,
  });
  store.startPreload(symbols.length * timeframes.length);
}

/** Silence longer than this (with a live socket) means the link is dead.
 *  The server sends a connection-level keepalive every 5 s, independent of
 *  the engine thread, so a long CCXT load no longer looks like a dead link. */
const WATCHDOG_MS = 45000;

/**
 * Stable per-tab session token. A reload keeps it (sessionStorage survives a
 * reload, dies with the tab), so the server can park the session — account,
 * fills, PnL, replay clock — and hand it back instead of starting from zero.
 */
const TOKEN_KEY = "simccxt.session-token.v1";

export function sessionToken(): string {
  try {
    const saved = window.sessionStorage.getItem(TOKEN_KEY);
    if (saved) return saved;
  } catch {
    // private mode: fall through to an in-memory token
  }
  const random =
    typeof crypto !== "undefined" && "randomUUID" in crypto
      ? crypto.randomUUID()
      : Math.random().toString(36).slice(2) + Date.now().toString(36);
  try {
    window.sessionStorage.setItem(TOKEN_KEY, random);
  } catch {
    // ignore
  }
  return random;
}

let seq = 0;
export function nextRequestId(): string {
  seq += 1;
  return "r" + Date.now().toString(36) + "-" + seq;
}

/**
 * One WebSocket per page, shared across React mounts.
 *
 * React StrictMode (dev) mounts effects twice: a naive hook would open two
 * sockets per tab and the server log would fill with connect/disconnect
 * pairs. Here the socket belongs to the page, not to the mount: attach()
 * reuses a live socket and only swaps the message handler, detach() never
 * closes. TCP closes naturally with the tab.
 */
class Connection {
  private ws: WebSocket | null = null;
  private route: Route | null = null;
  private retryMs = 1000;
  private retryTimer = 0;
  private watchdogTimer = 0;
  private lastMsg = 0;
  private snapshotDeadline = 0;

  expectSnapshot(): void {
    this.snapshotDeadline = Date.now() + SNAPSHOT_TIMEOUT_MS;
  }

  attach(route: Route): void {
    this.route = route;
    if (!this.ws || this.ws.readyState === WebSocket.CLOSED || this.ws.readyState === WebSocket.CLOSING) {
      this.connect();
    } else if (this.ws.readyState === WebSocket.OPEN) {
      // Reused live socket (StrictMode remount): re-confirm the session. The
      // server answers with `session_state`; the router decides whether a
      // snapshot is needed. Never resets the replay clock.
      this.send({ type: "attach", token: sessionToken() });
    }
    this.startWatchdog();
  }

  detach(): void {
    this.route = null;
    window.clearInterval(this.watchdogTimer);
  }

  send(msg: ClientMessage): void {
    if (handleLocalCommand(msg)) return;
    this.sendToServer(msg);
  }

  sendToServer(msg: ClientMessage): void {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(msg));
    }
  }

  private connect(): void {
    let ws: WebSocket;
    try {
      ws = new WebSocket(wsUrl());
    } catch {
      this.schedule();
      return;
    }
    this.ws = ws;
    store.setStatus("connecting");
    ws.onopen = () => {
      if (this.ws !== ws) return;
      this.retryMs = 1000;
      // Fresh liveness window: a reconnect after a long offline stint must
      // not be judged by the age of the previous session's last message.
      this.lastMsg = Date.now();
      store.setStatus("open");
      store.clearError();
      // First frame on every (re)connect: bind this socket to the tab's
      // session token. A reload therefore resumes the same account instead
      // of starting a fresh one.
      this.send({ type: "attach", token: sessionToken() });
    };
    ws.onmessage = (ev: MessageEvent) => {
      if (this.ws !== ws) return;
      const m = parseServerMessage(String(ev.data));
      if (!m) return;
      this.lastMsg = Date.now();
      if (m.type === "snapshot" || m.type === "error") this.snapshotDeadline = 0;
      store.noteMessage(this.lastMsg);
      this.route?.(m);
    };
    ws.onclose = () => {
      if (this.ws !== ws) return; // stale socket from an older generation
      this.ws = null;
      store.setStatus("closed");
      this.schedule();
    };
    ws.onerror = () => {
      try {
        ws.close();
      } catch {
        /* onclose drives the reconnect */
      }
    };
  }

  private schedule(): void {
    window.clearTimeout(this.retryTimer);
    this.retryTimer = window.setTimeout(() => {
      if (!this.route) return; // unmounted: stay idle until the next attach
      store.noteReconnect();
      this.connect();
    }, this.retryMs);
    this.retryMs = Math.min(this.retryMs * 2, 10000);
  }

  private startWatchdog(): void {
    window.clearInterval(this.watchdogTimer);
    this.watchdogTimer = window.setInterval(() => {
      if (!this.route || this.lastMsg === 0) return;
      if (this.snapshotDeadline > Date.now()) return;
      if (this.ws && this.ws.readyState === WebSocket.OPEN && Date.now() - this.lastMsg > WATCHDOG_MS) {
        // Looks connected but silent well past the server heartbeat: recycle.
        try {
          this.ws.close();
        } catch {
          /* onclose drives the reconnect */
        }
      }
    }, 5000);
  }
}

let conn: Connection | null = null;

export function getConnection(): Connection {
  if (!conn) conn = new Connection();
  return conn;
}

export function sendSim(msg: ClientMessage): void {
  getConnection().send(msg);
}

export function sendServer(msg: ClientMessage): void {
  getConnection().sendToServer(msg);
}
