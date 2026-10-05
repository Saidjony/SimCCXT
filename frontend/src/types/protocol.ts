// Protocol between simccxt-server (Rust) and this frontend.
// Must stay in sync with src/server/src/main.rs (see PROTOCOL comment there).
// No `any`: every message has an explicit type. Unknown shapes are rejected
// by isServerMessage() instead of crashing the UI.

// A candle row is always [ts_ms, open, high, low, close, volume].
export type CandleRow = [number, number, number, number, number, number];
export type ExchangeId = "binance" | "bybit" | "okx";

export interface BalanceLine {
  free: number;
  locked: number;
  total: number;
}

export interface PositionLine {
  symbol: string;
  side: string;
  qty: number;
  entryPrice: number;
  /** When the position was opened (epoch ms); 0/absent = unknown. */
  entryTimeMs?: number;
  markPrice: number;
  unrealizedPnl: number;
  realizedPnl: number;
  leverage: number;
  liquidationPrice: number;
}

export interface OrderLine {
  id: string;
  symbol: string;
  type: string;
  side: string;
  price: number | null;
  amount: number;
  filled: number;
  remaining: number;
  average: number;
  status: string;
  timestamp: number;
  fee: number;
  timeInForce: string;
  triggerPrice: number | null;
  reduceOnly: boolean;
  postOnly: boolean;
}

export interface FillLine {
  id: string;
  symbol: string;
  side: string;
  type: string;
  price: number;
  amount: number;
  cost: number;
  fee: number;
  orderId: string;
  timestamp: number;
  datetime: string;
  maker: boolean;
}

export interface AccountSnapshot {
  time: number;
  equity: number;
  balances: Record<string, BalanceLine>;
  positions: PositionLine[];
  open: OrderLine[];
  pnl: { realized: number; unrealized: number; fees: number };
}

export interface ReplayState {
  type: "replay_state";
  paused: boolean;
  now_ms: number;
  speed: number;
  step_ms: number;
  done: boolean;
  fed: number;
  total: number;
}

export interface ReplayWindow {
  start_ms: number;
  reveal_start_ms: number;
  end_ms: number;
}

export interface ChallengeEntry {
  nick: string;
  risk: number;
  capital: number;
  realized: number;
  unrealized: number;
  pnl: number;
  r: number;
  equity: number;
  trades: number;
  symbol: string;
}

// --- read-only diary HTTP API (GET /api/trader/:nick, /api/candles) ---

/** One closed round-trip trade reconstructed by the engine (FIFO). */
export interface DiaryTrade {
  symbol: string;
  side: "long" | "short";
  direction: string; // "buy→sell" | "sell→buy"
  qty: number;
  entry_price: number;
  exit_price: number;
  pnl: number;
  r: number;
  entry_time_ms: number;
  exit_time_ms: number;
  entry_fee: number;
  exit_fee: number;
  fee: number;
  entry_order_id: string;
  exit_order_id: string;
}

/** One still-open position (leftover lots of a single symbol). */
export interface DiaryPosition {
  symbol: string;
  side: "long" | "short";
  qty: number;
  entry_price: number;
  entry_time_ms: number;
  mark_price: number;
  unrealized: number;
}

export interface DiaryStats {
  total_trades: number;
  win_rate: number;
  wins: number;
  losses: number;
  realized_pnl: number;
  total_pnl: number;
  avg_r: number;
  avg_win: number;
  avg_loss: number;
  best: number;
  worst: number;
  fees: number;
}

/** Reply of `GET /api/trader/:nick`. `error` ⇒ trader is offline (HTTP 404). */
export interface DiaryData {
  type: "diary";
  nick: string;
  error?: string;
  trades?: DiaryTrade[];
  positions?: DiaryPosition[];
  stats?: DiaryStats;
  risk_usdt?: number;
  capital_usdt?: number;
  equity?: number;
  in_challenge?: boolean;
  symbol?: string;
  timeframe?: string;
  exchange?: string;
  range?: ReplayWindow;
  /** Visual context the trader published (see ClientMessage 'diary_meta'). */
  drawings?: Array<Record<string, unknown>>;
  criteria?: Record<string, string[]>;
}

/** One row of `GET /api/traders`. */
export interface TraderRow {
  nick: string;
  in_challenge: boolean;
  risk_usdt: number;
  capital_usdt: number;
  symbol: string;
  timeframe: string;
  equity: number;
  realized: number;
  unrealized: number;
  pnl: number;
  r: number | null;
  trades: number;
}

export interface TradersData {
  type: "traders";
  traders: TraderRow[];
}

/** Reply of `GET /api/candles` (diary chart data). */
export interface CandlesData {
  type: "candles";
  symbol: string;
  timeframe?: string;
  candles: CandleRow[];
  error?: string;
}

/**
 * Base URL of the read-only diary HTTP API, derived from the WS URL: Rust
 * binds the HTTP listener on `ws_port + 1`, so `ws://127.0.0.1:19003` ⇒
 * `http://127.0.0.1:19004`. A URL without an explicit port (reverse proxy)
 * keeps its origin, assuming the proxy routes `/api` too. Pure function of
 * the input so it is unit-testable (no `window`, no `import.meta`).
 */
export function apiBaseFrom(ws: string): string {
  try {
    const url = new URL(ws);
    url.protocol = url.protocol === "wss:" ? "https:" : "http:";
    if (url.port) url.port = String(Number(url.port) + 1);
    url.pathname = "";
    url.search = "";
    url.hash = "";
    return url.origin;
  } catch {
    return "http://127.0.0.1:9002"; // default ws port 9001 + 1
  }
}

// --- frontend -> Rust ---

export type ClientMessage =
  | {
      type: "subscribe";
      symbol: string;
      timeframe: string;
      exchange?: ExchangeId;
      start_ms?: number;
      reveal_start_ms?: number;
      end_ms?: number;
      request_id?: string;
    }
  | {
      type: "place_order";
      symbol: string;
      side: string;
      order_type: string;
      quantity: number;
      price?: number | null;
      stop_price?: number | null;
      stop_loss_price?: number | null;
      tp_price?: number | null;
      time_in_force?: string;
      post_only?: boolean;
      reduce_only?: boolean;
      request_id?: string;
    }
  | { type: "cancel_order"; order_id: string; symbol: string; request_id?: string }
  | { type: "close_position"; symbol: string; quantity?: number | null; request_id?: string }
  // Visual context of this session (annotations + enabled criteria) published
  // to the server so the read-only diary API can surface them.
  | {
      type: "diary_meta";
      drawings?: Array<Record<string, unknown>>;
      criteria?: Record<string, string[]>;
      request_id?: string;
    }
  | {
      type: "replay";
      action: "play" | "pause" | "step" | "reset" | "set_speed" | "set_step";
      steps?: number;
      speed?: number;
      step_ms?: number;
      timestamp_ms?: number | null;
      request_id?: string;
    }
  | { type: "login"; name: string; request_id?: string }
  | { type: "challenge"; join: boolean; risk_usdt?: number; capital_usdt?: number; request_id?: string }
  /**
   * Background warm-up of the Rust disk caches for watchlist x timeframes, so
   * switching a coin or a timeframe later resolves from disk instead of
   * waiting for a download. Touches no session state; the answer is
   * `preload_progress` events plus one `preload_done`.
   */
  | {
      type: "preload";
      symbols: string[];
      timeframes: string[];
      exchange?: ExchangeId;
      start_ms?: number;
      end_ms?: number;
      request_id?: string;
    }
  | { type: "ping" };

// --- Rust -> frontend ---
//
// Session events carry `to` = this tab's client_id; the connection drops
// messages addressed to other clients (one isolated Rust session per tab).

export type ServerMessage =
  | { type: "welcome"; client_id: number }
  | {
      type: "snapshot";
      to?: number;
      symbol: string;
      timeframe: string;
      exchange: ExchangeId;
      candles: CandleRow[];
      account: AccountSnapshot;
      replay: ReplayState;
      /** Open-interest context: [[ts_ms, usdt], ...], static (never replayed). */
      oi: Array<[number, number]>;
      oi_period: string;
      fills?: FillLine[];
      request_id?: string;
    }
  | { type: "candle"; to?: number; symbol: string; timeframe: string; exchange?: ExchangeId; candle: CandleRow; is_new: boolean }
  | { type: "oi_update"; to?: number; samples: Array<[number, number]> }
  | { type: "market_update"; to?: number; symbol: string; last: number; timestamp: number }
  | { type: "account_update"; to?: number; account: AccountSnapshot }
  | { type: "order_update"; to?: number; order: OrderLine; request_id?: string }
  | { type: "trade"; to?: number; fill: FillLine }
  | (ReplayState & { to?: number; request_id?: string })
  | { type: "heartbeat"; to?: number; now_ms: number; paused: boolean }
  | { type: "resync"; to?: number }
  | { type: "login_ok"; to?: number; nick: string; request_id?: string }
  | { type: "challenge_ok"; to?: number; joined: boolean; nick?: string; request_id?: string }
  // Challenge board has NO `to`: the server broadcasts it to every socket.
  | { type: "challenge_board"; board: ChallengeEntry[] }
  // Background market-data warm-up (see the `preload` client command).
  | {
      type: "preload_progress";
      to?: number;
      done: number;
      total: number;
      failed: number;
      symbol?: string;
      timeframe?: string;
      status: "queued" | "cached" | "error";
      note: string;
    }
  | { type: "preload_done"; to?: number; done: number; total: number; failed: number }
  | { type: "error"; to?: number; message: string; request_id?: string }
  | { type: "pong"; to?: number };

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null;
}

/** Narrow an unknown JSON value to a ServerMessage; null when malformed. */
export function parseServerMessage(raw: string): ServerMessage | null {
  let v: unknown;
  try {
    v = JSON.parse(raw) as unknown;
  } catch {
    return null;
  }
  if (!isRecord(v) || typeof v["type"] !== "string") return null;
  switch (v["type"]) {
    case "welcome":
    case "snapshot":
    case "candle":
    case "oi_update":
    case "market_update":
    case "account_update":
    case "order_update":
    case "trade":
    case "replay_state":
    case "heartbeat":
    case "resync":
    case "login_ok":
    case "challenge_ok":
    case "challenge_board":
    case "preload_progress":
    case "preload_done":
    case "error":
    case "pong":
      return v as ServerMessage;
    default:
      return null;
  }
}

/** Apply one incremental candle to a local series (pure, unit-tested). */
export function applyCandleDelta(series: CandleRow[], msg: { candle: CandleRow }): CandleRow[] {
  const last = series[series.length - 1];
  if (last && last[0] === msg.candle[0]) {
    const next = series.slice();
    next[next.length - 1] = msg.candle;
    return next;
  }
  if (!last || msg.candle[0] > last[0]) return [...series, msg.candle];
  return series; // stale / out-of-order: ignore, Rust stays authoritative
}

export function formatTs(ms: number): string {
  const d = new Date(ms);
  const p = (n: number): string => (n < 10 ? "0" + n : "" + n);
  return (
    d.getUTCFullYear() + "-" + p(d.getUTCMonth() + 1) + "-" + p(d.getUTCDate()) +
    " " + p(d.getUTCHours()) + ":" + p(d.getUTCMinutes()) + ":" + p(d.getUTCSeconds())
  );
}

export function formatMoney(n: number): string {
  if (!Number.isFinite(n)) return "—";
  return n.toLocaleString("en-US", { maximumFractionDigits: 2, minimumFractionDigits: 2 });
}
