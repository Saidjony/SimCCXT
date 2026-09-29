import type { ClientMessage, ExchangeId, ServerMessage } from "../types/protocol";
import { parseServerMessage } from "../types/protocol";
import { getState, store } from "../state/store";

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

export type Route = (m: ServerMessage) => void;

export function subscribeSim(
  symbol: string,
  timeframe: string,
  exchange: ExchangeId,
): void {
  const range = getState().replayWindow;
  sendSim({
    type: "subscribe",
    symbol,
    timeframe,
    exchange,
    ...range,
  });
}

/** Silence longer than this (with a live socket) means the link is dead. */
const WATCHDOG_MS = 25000;

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

  attach(route: Route): void {
    this.route = route;
    if (!this.ws || this.ws.readyState === WebSocket.CLOSED || this.ws.readyState === WebSocket.CLOSING) {
      this.connect();
    } else if (this.ws.readyState === WebSocket.OPEN) {
      // Reused live socket (StrictMode remount): ask Rust for a fresh snapshot.
      this.send({
        type: "subscribe",
        symbol: getState().symbol,
        timeframe: getState().timeframe,
        exchange: getState().exchange,
        ...getState().replayWindow,
      });
    }
    this.startWatchdog();
  }

  detach(): void {
    this.route = null;
    window.clearInterval(this.watchdogTimer);
  }

  send(msg: ClientMessage): void {
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
      store.setStatus("open");
      store.clearError();
    };
    ws.onmessage = (ev: MessageEvent) => {
      if (this.ws !== ws) return;
      const m = parseServerMessage(String(ev.data));
      if (!m) return;
      this.lastMsg = Date.now();
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
