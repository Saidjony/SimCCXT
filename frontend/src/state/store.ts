import { useSyncExternalStore } from "react";
import type {
  AccountSnapshot,
  CandleRow,
  ChallengeEntry,
  ExchangeId,
  FillLine,
  OrderLine,
  ReplayState,
  ReplayWindow,
} from "../types/protocol";
import { applyCandleDelta } from "../types/protocol";
import { CRITERIA } from "../trade/criteria";

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

// ---------------------------------------------------------------------------
// Watchlist: the right-panel coin list. Persisted per browser (localStorage);
// trading state itself lives in the Rust session, this is pure UI preference.
// ---------------------------------------------------------------------------
export interface CoinEntry {
  symbol: string;
  flagged: boolean;
}

function loadExchange(): ExchangeId {
  try {
    const saved = window.localStorage.getItem(EXCHANGE_KEY);
    if (EXCHANGE_IDS.includes(saved as ExchangeId)) return saved as ExchangeId;
  } catch {
    // Use Binance when storage is unavailable.
  }
  return "binance";
}

function persistExchange(exchange: ExchangeId): void {
  try {
    window.localStorage.setItem(EXCHANGE_KEY, exchange);
  } catch {
    // Keep the selected exchange for this page load.
  }
}

const WATCHLIST_KEY = "simccxt.watchlist.v1";
const EXCHANGE_KEY = "simccxt.exchange.v1";
const EXCHANGE_IDS: ExchangeId[] = ["binance", "bybit", "okx"];
const REPLAY_WINDOW_KEY = "simccxt.replay-window.v1";
const DEFAULT_WATCHLIST: CoinEntry[] = [
  { symbol: "BTC/USDT", flagged: false },
  { symbol: "ETH/USDT", flagged: false },
  { symbol: "SOL/USDT", flagged: false },
  { symbol: "XRP/USDT", flagged: false },
];
const SYMBOL_RE = /^[A-Z0-9]{2,12}\/[A-Z0-9]{2,12}$/;

function defaultReplayWindow(): ReplayWindow {
  const end = Date.now();
  const day = 24 * 60 * 60_000;
  return { start_ms: end - 3 * day, reveal_start_ms: end - 2 * day, end_ms: end };
}

function loadReplayWindow(): ReplayWindow {
  const fallback = defaultReplayWindow();
  try {
    const raw = window.localStorage.getItem(REPLAY_WINDOW_KEY);
    if (raw) {
      const value: unknown = JSON.parse(raw);
      if (value && typeof value === "object") {
        const saved = value as Partial<ReplayWindow>;
        if (
          Number.isFinite(saved.start_ms) &&
          Number.isFinite(saved.reveal_start_ms) &&
          Number.isFinite(saved.end_ms) &&
          saved.start_ms! < saved.reveal_start_ms! &&
          saved.reveal_start_ms! <= saved.end_ms!
        ) {
          return {
            start_ms: saved.start_ms!,
            reveal_start_ms: saved.reveal_start_ms!,
            end_ms: saved.end_ms!,
          };
        }
      }
    }
  } catch {
    // Keep the default range when local storage is unavailable.
  }
  return fallback;
}

/** Normalize user input to BASE/QUOTE; null when the format is wrong. */
export function normalizeSymbol(raw: string): string | null {
  const s = raw.trim().toUpperCase().replace(/\s+/g, "");
  return SYMBOL_RE.test(s) ? s : null;
}

function loadWatchlist(): CoinEntry[] {
  try {
    const raw = window.localStorage.getItem(WATCHLIST_KEY);
    if (raw) {
      const parsed: unknown = JSON.parse(raw);
      if (Array.isArray(parsed)) {
        const out: CoinEntry[] = [];
        for (const e of parsed) {
          if (e && typeof e === "object") {
            const rec = e as { symbol?: unknown; flagged?: unknown };
            const sym = normalizeSymbol(String(rec.symbol ?? ""));
            if (sym && !out.some((c) => c.symbol === sym)) {
              out.push({ symbol: sym, flagged: Boolean(rec.flagged) });
            }
          }
        }
        if (out.length) return out;
      }
    }
  } catch {
    // private mode / corrupt data: fall through to defaults
  }
  return DEFAULT_WATCHLIST.map((c) => ({ ...c }));
}

function persistWatchlist(list: CoinEntry[]): void {
  try {
    window.localStorage.setItem(WATCHLIST_KEY, JSON.stringify(list));
  } catch {
    // storage unavailable: the list still works for this page load
  }
}

// ---------------------------------------------------------------------------
// Setup checklist: 30 criteria, toggled in the sidebar, shown as chips on
// the chart overlay. Pure UI preference, persisted per browser.
// ---------------------------------------------------------------------------

const CRITERIA_KEY = "simccxt.criteria.v1";
const CHART_CRITERIA_KEY = "simccxt.chart-criteria.v1";
const CRITERION_IDS = new Set(CRITERIA.map((c) => c.id));

function loadCriteria(): string[] {
  try {
    const raw = window.localStorage.getItem(CRITERIA_KEY);
    if (raw) {
      const parsed: unknown = JSON.parse(raw);
      if (Array.isArray(parsed)) {
        const out: string[] = [];
        for (const e of parsed) {
          if (typeof e === "string" && CRITERION_IDS.has(e) && !out.includes(e)) {
            out.push(e);
          }
        }
        return out;
      }
    }
  } catch {
    // private mode / corrupt data: start empty
  }
  return [];
}

function persistCriteria(list: string[]): void {
  try {
    window.localStorage.setItem(CRITERIA_KEY, JSON.stringify(list));
  } catch {
    // storage unavailable: selection still works for this page load
  }
}

export function chartCriteriaKey(symbol: string, timeframe: string): string {
  return `${symbol}::${timeframe}`;
}

function loadChartCriteria(): Record<string, string[]> {
  const validList = (value: unknown): string[] => {
    if (!Array.isArray(value)) return [];
    return value.filter(
      (id): id is string => typeof id === "string" && CRITERION_IDS.has(id),
    );
  };
  try {
    const raw = window.localStorage.getItem(CHART_CRITERIA_KEY);
    if (raw) {
      const parsed: unknown = JSON.parse(raw);
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        const values = parsed as Record<string, unknown>;
        const next: Record<string, string[]> = {};
        for (const [panel, timeframe] of [["top", "5m"], ["hour", "1h"], ["day", "1d"]]) {
          if (Array.isArray(values[panel]) && !Array.isArray(values[timeframe])) {
            next[chartCriteriaKey("BTC/USDT", timeframe)] = validList(values[panel]);
          }
        }
        for (const [key, value] of Object.entries(values)) {
          if (/^(1m|5m|15m|1h|4h|1d)$/.test(key)) {
            next[chartCriteriaKey("BTC/USDT", key)] = validList(value);
          } else if (/^[A-Z0-9]{2,12}\/[A-Z0-9]{2,12}::(1m|5m|15m|1h|4h|1d)$/.test(key)) {
            next[key] = validList(value);
          }
        }
        return next;
      }
    }
  } catch {
    // Private mode or corrupt data: use the legacy top-chart selection.
  }
  return { [chartCriteriaKey("BTC/USDT", "5m")]: loadCriteria() };
}

function persistChartCriteria(criteria: Record<string, string[]>): void {
  try {
    window.localStorage.setItem(CHART_CRITERIA_KEY, JSON.stringify(criteria));
  } catch {
    // Selection still works for this page load when storage is unavailable.
  }
}

// ---------------------------------------------------------------------------
// Challenge identity: display nick (+ whether we publish on the shared
// board). Passwordless by design; the server assigns a random nick per
// session and accepts an override. Persisted per browser, restored on connect.
// ---------------------------------------------------------------------------

const USER_KEY = "simccxt.user.v1";

function loadUser(): { name: string; inChallenge: boolean } {
  try {
    const raw = window.localStorage.getItem(USER_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as { name?: unknown; inChallenge?: unknown };
      return {
        name: typeof parsed.name === "string" ? parsed.name : "",
        // Every session participates on the shared board now (the "видно
        // всем" opt-in toggle is gone) — ignore the stored flag.
        inChallenge: true,
      };
    }
  } catch {
    // private mode / corrupt data: start anonymous
  }
  return { name: "", inChallenge: true };
}

function persistUser(name: string, inChallenge: boolean): void {
  try {
    window.localStorage.setItem(USER_KEY, JSON.stringify({ name, inChallenge }));
  } catch {
    // storage unavailable: identity still works for this page load
  }
}

// ---------------------------------------------------------------------------
// UI preferences: OI pane visibility + collapsed cards. Pure UI, per browser.
// ---------------------------------------------------------------------------

const UI_KEY = "simccxt.ui.v1";

interface UiPrefs {
  oiVisible: boolean;
  cvdVisible: boolean;
  collapsedMarket: boolean;
  collapsedCriteria: boolean;
}

function loadUi(): UiPrefs {
  const dflt: UiPrefs = { oiVisible: true, cvdVisible: true, collapsedMarket: false, collapsedCriteria: false };
  try {
    const raw = window.localStorage.getItem(UI_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as Partial<UiPrefs>;
      return {
        oiVisible: parsed.oiVisible !== false,
        cvdVisible: parsed.cvdVisible !== false,
        collapsedMarket: parsed.collapsedMarket === true,
        collapsedCriteria: parsed.collapsedCriteria === true,
      };
    }
  } catch {
    // private mode / corrupt data: defaults
  }
  return dflt;
}

function persistUi(p: UiPrefs): void {
  try {
    window.localStorage.setItem(UI_KEY, JSON.stringify(p));
  } catch {
    // storage unavailable: still works for this page load
  }
}

function currentUi(): UiPrefs {
  return {
    oiVisible: state.oiVisible,
    cvdVisible: state.cvdVisible,
    collapsedMarket: state.collapsedMarket,
    collapsedCriteria: state.collapsedCriteria,
  };
}

export interface SimState {
  status: "connecting" | "open" | "closed";
  clientId: number | null;
  symbol: string;
  exchange: ExchangeId;
  timeframe: string;
  candles: CandleRow[];
  /** Bumped on every candle change; chart applies setData/update incrementally. */
  candlesVersion: number;
  /** True when the chart must replace the whole series (new snapshot). */
  fullReset: boolean;
  lastPrice: number;
  account: AccountSnapshot | null;
  fills: FillLine[];
  lastOrder: OrderLine | null;
  replay: ReplayState;
  replayWindow: ReplayWindow;
  error: string | null;
  /** Background market-data warm-up progress for watchlist x timeframes. */
  preload: PreloadProgress;
  bgLoaded: boolean;
  /** Drawings of the ACTIVE coin (what the charts render). */
  drawings: Drawing[];
  /**
   * Per-coin stash: every coin keeps its own drawings, `drawings` above is
   * just the active coin's set (swapped in `applySnapshot` on a coin switch).
   */
  drawingsBySymbol: Record<string, Drawing[]>;
  tool: Tool;
  /** Selected drawing (highlight + Delete key). Null = none. */
  selectedId: string | null;
  /** Position-tool notional in USDT (entry size = usdt / entry price). */
  positionSizeUsdt: number;
  /** Risk terminal: starting capital (reference) and fixed risk per trade. */
  initialCapitalUsdt: number;
  fixedRiskUsdt: number;
  /** Right-panel coin list: add / remove / flag (pinned to the top). */
  watchlist: CoinEntry[];
  /** Setup checklist: selected criterion ids → chips on the chart overlay. */
  criteria: string[];
  /** Independent checklist selection for each chart panel. */
  chartCriteria: Record<string, string[]>;
  /** Challenge display nick ("" = use the server-assigned random one). */
  nick: string;
  /** Effective nick from the last login_ok (random or override). */
  serverNick: string;
  /** Publish our nick+stats on the shared challenge board (opt-in). */
  inChallenge: boolean;
  /** Last challenge board broadcast by the server (no `to`: for everyone). */
  board: ChallengeEntry[];
  /** Open-interest history from the snapshot (static context, [[ts, usdt]]). */
  oi: Array<[number, number]>;
  /** OI grid actually loaded ("5m", "" when none). */
  oiPeriod: string;
  /** Show the OI pane under the chart (TradingView-style, toggleable). */
  oiVisible: boolean;
  /** Show the CVD line in the bottom pane (cumulative volume delta). */
  cvdVisible: boolean;
  /** Collapsed right-panel cards (persisted UI preference). */
  collapsedMarket: boolean;
  collapsedCriteria: boolean;
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

/**
 * Progress of the background market-data warm-up (Rust fills the disk caches
 * for the whole watchlist x every timeframe, so switching is instant). Pure UI
 * state: it never gates anything, it only tells the user what is loading.
 */
export interface PreloadProgress {
  /** A warm-up is queued or running. */
  active: boolean;
  /** Series finished so far (cache hit or downloaded). */
  done: number;
  /** Series in this warm-up batch. */
  total: number;
  /** Series that failed (offline, unknown pair, rate limit). */
  failed: number;
  /** Symbol/timeframe currently being warmed (empty between batches). */
  current: string;
}

const initialPreload: PreloadProgress = {
  active: false,
  done: 0,
  total: 0,
  failed: 0,
  current: "",
};

let state: SimState = {
  status: "connecting",
  clientId: null,
  symbol: "BTC/USDT",
  exchange: loadExchange(),
  timeframe: "5m",
  candles: [],
  candlesVersion: 0,
  fullReset: false,
  lastPrice: 0,
  account: null,
  fills: [],
  lastOrder: null,
  replay: initialReplay,
  replayWindow: loadReplayWindow(),
  error: null,
  preload: initialPreload,
  // Pre-load all watchlist symbols for all timeframes (background).
  bgLoaded: false,
  drawings: [],
  drawingsBySymbol: {},
  tool: "none",
  selectedId: null,
  positionSizeUsdt: 100,
  initialCapitalUsdt: 1000,
  fixedRiskUsdt: 10,
  watchlist: loadWatchlist(),
  criteria: loadCriteria(),
  chartCriteria: loadChartCriteria(),
  nick: loadUser().name,
  serverNick: "",
  inChallenge: loadUser().inChallenge,
  board: [],
  oi: [],
  oiPeriod: "",
  oiVisible: loadUi().oiVisible,
  cvdVisible: loadUi().cvdVisible,
  collapsedMarket: loadUi().collapsedMarket,
  collapsedCriteria: loadUi().collapsedCriteria,
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
  setExchange(exchange: ExchangeId): void {
    if (!EXCHANGE_IDS.includes(exchange)) return;
    set({ exchange });
    persistExchange(exchange);
  },
  setClientId(id: number): void {
    set({ clientId: id });
  },
  /** Full historical dataset: afterwards only deltas arrive. */
  applySnapshot(
    symbol: string,
    exchange: ExchangeId,
    timeframe: string,
    candles: CandleRow[],
    account: AccountSnapshot,
    replay: ReplayState,
    oi: Array<[number, number]>,
    oiPeriod: string,
    fills: FillLine[] = [],
  ): void {
    const selectedExchange = EXCHANGE_IDS.includes(exchange) ? exchange : state.exchange;
    // Coin switch: stash the active coin's drawings, pull in the incoming
    // coin's own set (empty if never drawn on). Same coin (replay reset,
    // reconnect, timeframe change): drawings stay untouched — they are
    // anchored to absolute time, so they still fit the new series.
    const switching = symbol !== state.symbol;
    const drawingsBySymbol = switching
      ? { ...state.drawingsBySymbol, [state.symbol]: state.drawings }
      : state.drawingsBySymbol;
    const drawings = switching ? (drawingsBySymbol[symbol] ?? []) : state.drawings;
    set({
      symbol,
      exchange: selectedExchange,
      timeframe,
      candles,
      candlesVersion: state.candlesVersion + 1,
      fullReset: true,
      lastPrice: candles.length ? candles[candles.length - 1][4] : 0,
      account,
      replay,
      oi,
      oiPeriod,
      fills: fills.slice(0, 500),
      error: null,
      drawings,
      drawingsBySymbol,
      // A selection points into the stashed set: drop it when coins change.
      selectedId: switching ? null : state.selectedId,
    });
    persistExchange(selectedExchange);
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
  applyOi(samples: Array<[number, number]>): void {
    if (!samples.length) return;
    const byTime = new Map(state.oi);
    for (const [time, value] of samples) {
      if (Number.isFinite(time) && Number.isFinite(value)) byTime.set(time, value);
    }
    set({ oi: [...byTime.entries()].sort((a, b) => a[0] - b[0]) });
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
  setReplayWindow(replayWindow: ReplayWindow): void {
    if (
      !Number.isFinite(replayWindow.start_ms) ||
      !Number.isFinite(replayWindow.reveal_start_ms) ||
      !Number.isFinite(replayWindow.end_ms) ||
      replayWindow.start_ms >= replayWindow.reveal_start_ms ||
      replayWindow.reveal_start_ms > replayWindow.end_ms
    ) return;
    set({ replayWindow });
    try {
      window.localStorage.setItem(REPLAY_WINDOW_KEY, JSON.stringify(replayWindow));
    } catch {
      // Keep the selected interval for this page load.
    }
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
  /** A warm-up batch was accepted by Rust (progress events follow). */
  startPreload(total: number): void {
    set({ preload: { active: true, done: 0, total, failed: 0, current: "" } });
  },
  /** One `symbol@timeframe` finished (cache hit, download or failure). */
  advancePreload(done: number, total: number, failed: number, current: string): void {
    set({ preload: { active: true, done, total, failed, current } });
  },
  /** The batch finished: keep the counters visible, stop the spinner. */
  finishPreload(done: number, total: number, failed: number): void {
    set({ preload: { active: false, done, total, failed, current: "" } });
  },
  /** Warm-up was refused (offline loader, no valid pairs): show nothing. */
  cancelPreload(): void {
    if (state.preload.active) set({ preload: initialPreload });
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
  /** Toggle a setup criterion (unknown ids ignored). */
  toggleCriterion(id: string): void {
    if (!CRITERION_IDS.has(id)) return;
    const criteria = state.criteria.includes(id)
      ? state.criteria.filter((c) => c !== id)
      : [...state.criteria, id];
    set({ criteria });
    persistCriteria(criteria);
  },
  clearCriteria(): void {
    if (state.criteria.length) {
      set({ criteria: [] });
      persistCriteria([]);
    }
  },
  toggleChartCriterion(symbol: string, timeframe: string, id: string): void {
    if (!CRITERION_IDS.has(id)) return;
    const key = chartCriteriaKey(symbol, timeframe);
    const current = state.chartCriteria[key] ?? [];
    const selected = current.includes(id)
      ? current.filter((criterion) => criterion !== id)
      : [...current, id];
    const chartCriteria = { ...state.chartCriteria, [key]: selected };
    set({ chartCriteria });
    persistChartCriteria(chartCriteria);
  },
  clearChartCriteria(symbol: string, timeframe: string): void {
    const key = chartCriteriaKey(symbol, timeframe);
    if (!state.chartCriteria[key]?.length) return;
    const chartCriteria = { ...state.chartCriteria, [key]: [] };
    set({ chartCriteria });
    persistChartCriteria(chartCriteria);
  },
  /** Display nick for the challenge ("" = server-assigned random nick). */
  setNick(name: string): void {
    if (state.nick === name) return;
    set({ nick: name });
    persistUser(name, state.inChallenge);
  },
  /** Server confirmed the effective nick (echo of login_ok). */
  applyLogin(nick: string): void {
    if (state.serverNick !== nick) set({ serverNick: nick });
  },
  setInChallenge(v: boolean): void {
    if (state.inChallenge === v) return;
    set({ inChallenge: v });
    persistUser(state.nick, v);
  },
  applyBoard(board: ChallengeEntry[]): void {
    set({ board });
  },
  setOiVisible(v: boolean): void {
    if (state.oiVisible === v) return;
    set({ oiVisible: v });
    persistUi({ ...currentUi(), oiVisible: v });
  },
  setCvdVisible(v: boolean): void {
    if (state.cvdVisible === v) return;
    set({ cvdVisible: v });
    persistUi({ ...currentUi(), cvdVisible: v });
  },
  setCollapsedMarket(v: boolean): void {
    if (state.collapsedMarket === v) return;
    set({ collapsedMarket: v });
    persistUi({ ...currentUi(), collapsedMarket: v });
  },
  setCollapsedCriteria(v: boolean): void {
    if (state.collapsedCriteria === v) return;
    set({ collapsedCriteria: v });
    persistUi({ ...currentUi(), collapsedCriteria: v });
  },
  /** Add a coin (already-normalized symbol). Returns null if dup/invalid. */
  addCoin(raw: string): string | null {
    const sym = normalizeSymbol(raw);
    if (!sym || state.watchlist.some((c) => c.symbol === sym)) return null;
    const watchlist = [...state.watchlist, { symbol: sym, flagged: false }];
    set({ watchlist });
    persistWatchlist(watchlist);
    return sym;
  },
  removeCoin(symbol: string): void {
    if (!state.watchlist.some((c) => c.symbol === symbol)) return;
    const watchlist = state.watchlist.filter((c) => c.symbol !== symbol);
    set({ watchlist });
    persistWatchlist(watchlist);
  },
  /** Flag/unflag: flagged coins sort to the top of the list. */
  toggleFlag(symbol: string): void {
    const watchlist = state.watchlist.map((c) =>
      c.symbol === symbol ? { ...c, flagged: !c.flagged } : c,
    );
    set({ watchlist });
    persistWatchlist(watchlist);
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
