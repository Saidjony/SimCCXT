import type { ClientMessage, ExchangeId, ReplayWindow, ServerMessage } from "../types/protocol";
import { apiBaseFrom, parseServerMessage } from "../types/protocol";
import { getState, store } from "../state/store";
import { handleLocalCommand, initializeLocalEngine } from "../replay/localEngine";
import { prefetchSeries } from "../market/provider";
import { downloadTicker } from "../market/exchanges";
import { BASE_TIMEFRAME, TIMEFRAMES, type Timeframe } from "../market/timeframes";

declare global {
  interface Window {
    /** Origin injected by simccxt-server into index.html (single-port mode). */
    __SIMCCXT_ORIGIN__?: string;
  }
}

/**
 * Origin of the page when the server served it (single-port mode), else
 * `undefined` (vite dev, where the page lives on :5173 and Rust on :9001).
 */
function injectedOrigin(): string | undefined {
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
 * Base URL of the read-only diary HTTP API. In single-port mode the API is on
 * the same origin as the page; in the vite dev flow it is `ws_port + 1` (see
 * `apiBaseFrom`). Market data does NOT come from here any more — it is
 * downloaded straight from the exchange by this browser (see
 * ../market/provider).
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

export type Route = (m: ServerMessage) => void;

/**
 * Switch this tab's series. The bars are downloaded by THIS browser and
 * aggregated locally; the Rust/WASM engine in the Web Worker owns the clock,
 * orders and PnL from that point on. Nothing about the selection is sent to
 * our server: it only receives the account/fill mirror for the challenge board
 * and the diary.
 */
export function subscribeSim(
  symbol: string,
  timeframe: string,
  exchange: ExchangeId,
): void {
  void initializeLocalEngine(symbol, timeframe, exchange, (message) => {
    getConnection().sendToServer(message);
  });
}

/**
 * Background download of the whole watchlist into THIS browser's cache.
 *
 * The point is that a coin is ready BEFORE the user clicks it: coins whose
 * data is already cached (and not due for a refresh) cost one disk read, and
 * the rest are downloaded one after another so the venue's per-IP rate limit
 * is never spent in a burst.
 *
 * Re-runs are safe by design: the provider serves anything already cached for
 * the same date window from disk, so re-entering the page the same day does not
 * re-download a coin that was fetched once.
 */
let warming = false;
let warmAgain = false;
export function warmMarketData(symbols: string[]): void {
  if (symbols.length === 0) return;
  if (warming) {
    warmAgain = true; // a coin was added while the loop ran
    return;
  }
  const exchange = getState().exchange;
  // Pin the date window: the default one is derived from `Date.now()` and is
  // re-created on every page load, so an unpinned warm-up could ask for a
  // window a few bars wider than the one on disk — and "a few bars wider" is a
  // different window, i.e. a full re-download. Persisting it also makes the
  // "Даты истории" controls show exactly what was downloaded.
  const stored = store.replayWindowFromStorage();
  if (stored) {
    store.setReplayWindow(stored);
  } else {
    const range: ReplayWindow = { ...getState().replayWindow };
    store.setReplayWindow(range);
  }
  const range = getState().replayWindow;
  warming = true;
  store.startPreload(symbols.length);
  void (async () => {
    let done = 0;
    let failed = 0;
    try {
      await prefetchSeries(symbols, exchange, range.start_ms, range.end_ms, {
        onStart: (symbol) => store.advancePreload(done, symbols.length, failed, `${symbol} · ${BASE_TIMEFRAME}`),
        onDone: (_symbol, didFail) => {
          done += 1;
          if (didFail) failed += 1;
        },
        shouldStop: () => {
          const current = getState();
          // A different venue or a new date window makes the rest moot; the
          // effect that re-ran the warm-up starts the right batch.
          return current.exchange !== exchange
            || current.replayWindow.start_ms !== range.start_ms
            || current.replayWindow.end_ms !== range.end_ms;
        },
      });
    } finally {
      store.finishPreload(done, symbols.length, failed);
      warming = false;
    }
    if (warmAgain) {
      warmAgain = false;
      const state = getState();
      warmMarketData(state.watchlist.map((coin) => coin.symbol));
    }
  })();
}

/** Intervals that exist in this terminal (5m downloaded, 1h/1d aggregated). */
export function availableTimeframes(): readonly Timeframe[] {
  return TIMEFRAMES;
}

/** Request current prices for the watchlist directly from the venue. */
export function lookupMarketsSim(symbols: string[], exchange: ExchangeId): void {
  if (symbols.length === 0) return;
  void (async () => {
    const quotes: Array<{ symbol: string; exchange: ExchangeId; last: number }> = [];
    const failures: Array<{ symbol: string; error: string }> = [];
    await Promise.all(symbols.map(async (symbol) => {
      try {
        const last = await downloadTicker(exchange, symbol);
        if (last !== null && last > 0) quotes.push({ symbol, exchange, last });
        else failures.push({ symbol, error: "нет цены" });
      } catch (error) {
        failures.push({ symbol, error: error instanceof Error ? error.message : String(error) });
      }
    }));
    store.applyMarketLookup(quotes, failures);
  })();
}

/** Silence longer than this (with a live socket) means the link is dead.
 *  The server sends a connection-level keepalive every 5 s, independent of
 *  the engine thread, so a long CCXT load no longer looks like a dead link. */
const WATCHDOG_MS = 45000;
/** A subscribe can park on the background series loader for a long time:
 *  keep the watchdog quiet that long so a cold cache looks "loading", not dead. */
const SNAPSHOT_TIMEOUT_MS = 180000;

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

/**
 * Send a message that is NOT a replay/trading command (local_sync, diary_meta,
 * login...).
 *
 * It deliberately bypasses `send()`: that one routes replay/trading commands to
 * the local engine, but a `local_sync` sent THROUGH it was silently swallowed by
 * `handleLocalCommand` — the server then never learned the selected series and
 * (in the window where its own subscribe reply was not yet applied) echoed back
 * `session_state` for the DEFAULT series. That echo reset the replay window and
 * re-triggered a download of the wrong date range, which is what made the
 * "Даты истории" window look broken and made every coin switch re-download.
 */
export function sendServer(msg: ClientMessage): void {
  getConnection().sendToServer(msg);
}
