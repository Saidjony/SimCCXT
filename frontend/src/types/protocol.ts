// Protocol between simccxt-server (Rust) and this frontend.
// Must stay in sync with src/server/src/main.rs (see PROTOCOL comment there).
// No `any`: every message has an explicit type. Unknown shapes are rejected
// by isServerMessage() instead of crashing the UI.

// A candle row is always [ts_ms, open, high, low, close, volume].
export type CandleRow = [number, number, number, number, number, number];

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
  markPrice: number;
  unrealizedPnl: number;
  realizedPnl: number;
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

export interface ChallengeEntry {
  nick: string;
  risk: number;
  pnl: number;
  r: number;
  equity: number;
}

// --- frontend -> Rust ---

export type ClientMessage =
  | { type: "subscribe"; symbol: string; timeframe: string; request_id?: string }
  | {
      type: "place_order";
      symbol: string;
      side: string;
      order_type: string;
      quantity: number;
      price?: number | null;
      stop_price?: number | null;
      time_in_force?: string;
      post_only?: boolean;
      reduce_only?: boolean;
      request_id?: string;
    }
  | { type: "cancel_order"; order_id: string; symbol: string; request_id?: string }
  | { type: "close_position"; symbol: string; quantity?: number | null; request_id?: string }
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
  | { type: "challenge"; join: boolean; risk_usdt?: number; request_id?: string }
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
      candles: CandleRow[];
      account: AccountSnapshot;
      replay: ReplayState;
      /** Open-interest context: [[ts_ms, usdt], ...], static (never replayed). */
      oi: Array<[number, number]>;
      oi_period: string;
      request_id?: string;
    }
  | { type: "candle"; to?: number; symbol: string; timeframe: string; candle: CandleRow; is_new: boolean }
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
