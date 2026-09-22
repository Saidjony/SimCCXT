import { useSyncExternalStore } from "react";
import type {
  AccountSnapshot,
  CandleRow,
  FillLine,
  OrderLine,
  ReplayState,
} from "../types/protocol";
import { applyCandleDelta } from "../types/protocol";

// Visual-only drawing objects. Never sent to Rust, never trade signals.
// - ray:      horizontal level starting at t0 (1 click), rendered as overlay
// - vline:    replay barrier (1 click): resets the replay clock to its bar;
//             bars right of it hide and reveal one by one on play
// - zone:     rectangle (t1,p1)-(t2,p2) (2 clicks), rendered as HTML overlay
// - position: long/short entry + SL + TP (3 clicks), rendered as HTML overlay
// Ruler is deliberately NOT stored: it is a transient local measurement.
export type Drawing =
  | { id: string; kind: "ray"; t0: number; price: number; color: string; title: string }
  | { id: string; kind: "vline"; t: number; color: string; title: string }
  | { id: string; kind: "zone"; t1: number; p1: number; t2: number; p2: number; color: string; title: string }
  | { id: string; kind: "position"; t1: number; t2: number; entry: number; sl: number; tp: number; color: string; title: string };

export type Tool = "none" | "ray" | "vline" | "zone" | "position" | "ruler";

/** A drawing without its id (id is assigned by the store). Distributive over the union. */
export type NewDrawing =
  | { kind: "ray"; t0: number; price: number; color: string; title: string }
  | { kind: "vline"; t: number; color: string; title: string }
  | { kind: "zone"; t1: number; p1: number; t2: number; p2: number; color: string; title: string }
  | { kind: "position"; t1: number; t2: number; entry: number; sl: number; tp: number; color: string; title: string };

export interface SimState {
  status: "connecting" | "open" | "closed";
  clientId: number | null;
  symbol: string;
  timeframe: string;  candles: CandleRow[];
  /** Bumped on every candle change; chart applies setData/update incrementally. */
  candlesVersion: number;
  /** True when the chart must replace the whole series (new snapshot). */
  fullReset: boolean;
  lastPrice: number;
  account: AccountSnapshot | null;
  fills: FillLine[];
  lastOrder: OrderLine | null;
  replay: ReplayState;
  error: string | null;
  drawings: Drawing[];
  tool: Tool;
  /** Selected drawing (highlight + Delete key). Null = none. */
  selectedId: string | null;
  /** Position-tool notional in USDT (entry size = usdt / entry price). */
  positionSizeUsdt: number;
  /** Risk terminal: starting capital (reference) and fixed risk per trade. */
  initialCapitalUsdt: number;
  fixedRiskUsdt: number;
  /** Wall-clock ms of the last message from Rust (any type). */
  lastMsgAt: number;
  /** How many reconnects this page has done (diagnostics). */
  reconnects: number;
}

const initialReplay: ReplayState = {
  type: "replay_state",
  paused: true,
  now_ms: 0,
  speed: 1,
  step_ms: 60000,
  done: false,
  fed: 0,
  total: 0,
};

let state: SimState = {
  status: "connecting",
  clientId: null,
  symbol: "BTC/USDT",
  timeframe: "1m",
  candles: [],
  candlesVersion: 0,
  fullReset: false,
  lastPrice: 0,
  account: null,
  fills: [],
  lastOrder: null,
  replay: initialReplay,
  error: null,
  drawings: [],
  tool: "none",
  selectedId: null,
  positionSizeUsdt: 100,
  initialCapitalUsdt: 1000,
  fixedRiskUsdt: 10,
  lastMsgAt: 0,
  reconnects: 0,
};

const listeners = new Set<() => void>();

function set(patch: Partial<SimState>): void {
  state = { ...state, ...patch };
  for (const l of listeners) l();
}

function subscribe(l: () => void): () => void {
  listeners.add(l);
  return () => {
    listeners.delete(l);
  };
}

function get(): SimState {
  return state;
}

/** useSyncExternalStore selector helper (one subscription, per-field select).
 *
 * getSnapshot must return a CACHED value: it is the stable `state` reference
 * itself (only replaced on set()). Selection happens during render, so
 * selectors may freely build fresh arrays (e.g. `s.account?.positions ?? []`)
 * without tripping React's infinite-loop guard.
 */
export function useSim<T>(sel: (s: SimState) => T): T {
  const snap = useSyncExternalStore(subscribe, get, get);
  return sel(snap);
}

let drawingSeq = 1;
function nextDrawingId(): string {
  drawingSeq += 1;
  return "d" + drawingSeq.toString(36) + Date.now().toString(36).slice(-4);
}

export const store = {
  setStatus(s: SimState["status"]): void {
    set({ status: s });
  },
  setClientId(id: number): void {
    set({ clientId: id });
  },
  /** Full historical dataset: afterwards only deltas arrive. */
  applySnapshot(
    symbol: string,
    timeframe: string,
    candles: CandleRow[],
    account: AccountSnapshot,
    replay: ReplayState,
  ): void {
    set({
      symbol,
      timeframe,
      candles,
      candlesVersion: state.candlesVersion + 1,
      fullReset: true,
      lastPrice: candles.length ? candles[candles.length - 1][4] : 0,
      account,
      replay,
      error: null,
    });
  },
  consumedReset(): void {
    if (state.fullReset) set({ fullReset: false });
  },
  /** Incremental update: new bar appended, current bar replaced in place. */
  applyCandle(candle: CandleRow): void {
    set({
      candles: applyCandleDelta(state.candles, { candle }),
      candlesVersion: state.candlesVersion + 1,
      lastPrice: candle[4],
    });
  },
  applyMarket(last: number): void {
    set({ lastPrice: last });
  },
  applyAccount(account: AccountSnapshot): void {
    set({ account });
  },
  applyReplay(replay: ReplayState): void {
    set({ replay });
  },
  applyOrder(order: OrderLine): void {
    set({ lastOrder: order, error: null });
  },
  applyFill(fill: FillLine): void {
    if (state.fills.some((f) => f.id === fill.id)) return;
    set({ fills: [fill, ...state.fills].slice(0, 500) });
  },
  setError(message: string): void {
    set({ error: message });
  },
  clearError(): void {
    if (state.error) set({ error: null });
  },
  setTool(tool: Tool): void {
    set({ tool });
    if (tool !== "none") set({ selectedId: null });
  },
  selectDrawing(id: string | null): void {
    set({ selectedId: id });
  },
  setPositionSizeUsdt(v: number): void {
    if (Number.isFinite(v) && v > 0) set({ positionSizeUsdt: v });
  },
  setInitialCapitalUsdt(v: number): void {
    if (Number.isFinite(v) && v > 0) set({ initialCapitalUsdt: v });
  },
  setFixedRiskUsdt(v: number): void {
    if (Number.isFinite(v) && v > 0) set({ fixedRiskUsdt: v });
  },
  /** Any inbound traffic resets the liveness clock (see the watchdog). */
  noteMessage(at: number): void {
    set({ lastMsgAt: at });
  },
  noteReconnect(): void {
    set({ reconnects: state.reconnects + 1, status: "connecting" });
  },
  addDrawing(d: NewDrawing): void {
    set({ drawings: [...state.drawings, { ...d, id: nextDrawingId() } as Drawing] });
  },
  removeDrawing(id: string): void {
    set({
      drawings: state.drawings.filter((d) => d.id !== id),
      selectedId: state.selectedId === id ? null : state.selectedId,
    });
  },
  /** Patch one drawing (drag-move). Pure fn keeps the kind intact. */
  updateDrawing(id: string, fn: (d: Drawing) => Drawing): void {
    set({ drawings: state.drawings.map((d) => (d.id === id ? fn(d) : d)) });
  },
  clearDrawings(): void {
    if (state.drawings.length) set({ drawings: [] });
  },
};

export function getState(): SimState {
  return get();
}
