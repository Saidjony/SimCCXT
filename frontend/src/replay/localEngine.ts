import type {
  AccountSnapshot,
  CandleRow,
  ExchangeId,
  FillLine,
  ReplayState,
  ServerMessage,
} from "../types/protocol";
import { getState, store } from "../state/store";
import type { ClientMessage } from "../types/protocol";
import { aggregateCandles } from "../market/aggregate";
import { peekBaseBars, loadBaseBars, loadOi } from "../market/provider";
import {
  BASE_TIMEFRAME,
  TIMEFRAME_MS,
  isTimeframe,
  timeframeOr,
  type Timeframe,
} from "../market/timeframes";

interface LocalSnapshot {
  type: "snapshot";
  symbol: string;
  exchange: ExchangeId;
  timeframe: string;
  full_reset: boolean;
  candles_delta?: CandleRow[];
  account: AccountSnapshot;
  replay: ReplayState;
  fills: FillLine[];
  oi?: Array<[number, number]> | null;
  oi_period?: string;
  range: { reveal_reset_ms: number; end_ms: number };
  entry_protections?: Record<string, Record<string, unknown>>;
  exit_reasons?: Record<string, string>;
}

interface WorkerResponse {
  type: "snapshot" | "error";
  snapshot?: LocalSnapshot;
  message?: string;
}

/**
 * Load the series for `symbol@timeframe` from THIS browser: the provider serves
 * 5m bars (memory → OPFS/IndexedDB → direct exchange download) and anything
 * coarser is aggregated locally. The returned window always starts at the start
 * of the replay range, so every timeframe sees the same history.
 */
async function loadSeries(
  symbol: string,
  exchange: ExchangeId,
  timeframe: Timeframe,
  signal?: AbortSignal,
): Promise<{ base: CandleRow[]; rows: CandleRow[]; revealResetMs: number; startMs: number }> {
  const range = getState().replayWindow;
  const stepMs = TIMEFRAME_MS[timeframe];
  const base = await loadBaseBars(exchange, symbol, range.start_ms, range.end_ms, {
    signal,
    onProgress: (done, total) => store.advancePreload(done, total, 0, `${symbol} · ${BASE_TIMEFRAME}`),
  });
  const rows = aggregateCandles(base, timeframe);
  if (rows.length === 0) {
    throw new Error(`Нет свечей ${symbol} на ${exchange} за выбранный диапазон дат`);
  }
  const revealMs = revealFor(rows, range.reveal_start_ms, stepMs);
  const revealResetMs = revealMs - stepMs;
  // Start the clock at the reveal boundary, not at the first loaded bar: the
  // window deliberately includes pre-revealed context before `reveal_start_ms`
  // (see the default two-month window). Starting at `rows[0]` made the first
  // load show only the first two bars, while reset/coin-switch showed the
  // pre-revealed history — two different replay starts for the same window.
  return {
    base,
    rows,
    revealResetMs,
    startMs: Math.max(rows[0][0], revealResetMs),
  };
}

/**
 * The reveal boundary actually used, snapped to the loaded series.
 *
 * The user picks a wall-clock moment ("2026-08-27 14:37"), but bars sit on
 * fixed boundaries and can be missing. The replay therefore starts at the first
 * bar at or after that moment; when the moment is past the loaded history, at
 * the start of its LAST bar — not at an arbitrary wall-clock time, which would
 * simply show nothing.
 */
function revealFor(rows: readonly CandleRow[], revealStartMs: number, stepMs: number): number {
  const first = rows.find((row) => row[0] >= revealStartMs);
  if (first) return first[0];
  return rows[rows.length - 1][0];
}

/**
 * The reveal boundary the running replay actually uses: the first bar at or
 * after the requested moment, or the last bar's time when the moment is past
 * the loaded history. The date controls show this, so they never pretend a
 * boundary the engine cannot honor.
 */
export function effectiveRevealMs(): number {
  const { replay, timeframe } = getState();
  return replay.now_ms + TIMEFRAME_MS[timeframe];
}

export function requestLocalSync(replaceFills = false): void {
  replaceFillsOnNextSync = replaceFillsOnNextSync || replaceFills;
  if (ready) worker?.postMessage({ type: "snapshot", include_fills: true });
}

let worker: Worker | null = null;
let workerKey = "";
/**
 * Random id of the engine instance running in this worker.
 *
 * Order ids restart at 1 every time the engine is built and the replay restarts
 * from the same bar after a reload, so `symbol|orderId|time` alone describes two
 * different trades. The id travels with the fills and with every protection /
 * exit-reason key, and the server reconstructs trades per engine instance.
 */
let engineId = "";
/** Base bars (5m) each series was initialised with, keyed like `workerKey`. */
const loadedRows = new Map<string, { base: CandleRow[] }>();
let generation = 0;
let initializing = false;
let ready = false;
let syncLocal: ((message: ClientMessage) => void) | undefined;
/**
 * Merge by default: this engine's fill list starts empty on every page load,
 * so replacing the server's mirror with it would erase the trader's history.
 * Only an explicit `requestLocalSync(true)` (reconnect / resync, where the
 * engine is the one holding the full history) asks for a replace.
 */
let replaceFillsOnNextSync = false;
let pendingSyncSnapshot: LocalSnapshot | undefined;
let pendingSyncFills: FillLine[] = [];
let localEntryProtections: Record<string, Record<string, unknown>> = {};
let localExitReasons: Record<string, string> = {};
let syncTimer: ReturnType<typeof setTimeout> | undefined;
let pendingRenderSnapshot: LocalSnapshot | undefined;
let renderTimer: ReturnType<typeof setTimeout> | undefined;
let lastRenderAt = 0;
const seenFills = new Set<string>();
const LOCAL_RENDER_INTERVAL_MS = 33;
const LARGE_SERIES_RENDER_INTERVAL_MS = 200;
const LARGE_SERIES_THRESHOLD = 10_000;
const SERVER_SYNC_INTERVAL_MS = 500;

function fillKey(fill: FillLine): string {
  return `${fill.id}|${fill.symbol}|${fill.timestamp}|${fill.price}|${fill.amount}`;
}

function newEngineId(): string {
  const webCrypto = globalThis.crypto;
  if (webCrypto && typeof webCrypto.randomUUID === "function") return webCrypto.randomUUID();
  return `e${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

/**
 * Stop/TP of every entry this tab ever made, in the engine's snake_case shape.
 *
 * The store persists them (`simccxt.trade-protections.v2`) while this module's
 * copy dies with the page. Without re-seeding, the first `local_sync` after a
 * reload mirrors an almost empty map and the diary loses "было в ±R" (the
 * server can no longer tell which stop the trade was planned with).
 */
function persistedProtections(): Record<string, Record<string, unknown>> {
  const out: Record<string, Record<string, unknown>> = {};
  for (const [key, protection] of Object.entries(getState().entryProtections)) {
    const value: Record<string, unknown> = {};
    if (protection.stopPrice !== null) value.stop_price = protection.stopPrice;
    if (protection.takePrice !== null) value.take_price = protection.takePrice;
    if (Object.keys(value).length > 0) out[key] = value;
  }
  return out;
}

function applyLocalSnapshot(snapshot: LocalSnapshot): void {
  if (snapshot.full_reset) {
    // A worker restart (reload, or a fresh engine after a failure) has no
    // series of its own: this page's loaded base bars are the full history and
    // `fed` says how far the clock has revealed them.
    //
    // The FIRST snapshot of a page can arrive before the provider has finished
    // registering the series, so fall back to the bars this page just loaded
    // and handed to that very worker. Erroring out here used to leave the chart
    // blank ("Локальный реплей не получил свечи для полного обновления") even
    // though the data was already loading.
    const key = keyFor(snapshot.exchange, snapshot.symbol, snapshot.timeframe);
    const loaded = loadedRows.get(key);
    const base = peekBaseBars(snapshot.exchange, snapshot.symbol, 0, Number.MAX_SAFE_INTEGER)
      ?? (loaded ? loaded.base : null);
    if (!base || base.length === 0) {
      if (base === null) {
        // The series is still loading: its own snapshot will arrive shortly.
        return;
      }
      store.setError("Локальный реплей не получил свечи для полного обновления");
      return;
    }
    const timeframe = timeframeOr(snapshot.timeframe, BASE_TIMEFRAME);
    store.applySnapshot(
      snapshot.symbol,
      snapshot.exchange,
      timeframe,
      aggregateCandles(base, timeframe).slice(0, snapshot.replay.fed),
      snapshot.account,
      snapshot.replay,
      snapshot.oi ?? [],
      snapshot.oi_period ?? "",
      snapshot.fills,
    );
  } else {
    store.applyLocalUpdate(
      snapshot.candles_delta ?? [],
      snapshot.account,
      snapshot.replay,
      snapshot.fills,
    );
  }
  lastRenderAt = Date.now();
}

function flushLocalSync(): void {
  syncTimer = undefined;
  const snapshot = pendingSyncSnapshot;
  if (!snapshot || !syncLocal) return;
  const state = getState();
  // Never replace the server's mirror with nothing: a page load starts this
  // engine with an empty fill list, and `replace_fills` would then wipe the
  // trader's diary. An empty replace is a no-op anyway, so merge instead.
  const replaceFills = replaceFillsOnNextSync && snapshot.fills.length > 0;
  syncLocal({
    type: "local_sync",
    account: snapshot.account,
    fills: replaceFills ? snapshot.fills.slice().reverse() : pendingSyncFills,
    replace_fills: replaceFills,
    engine_id: engineId,
    entry_protections: localEntryProtections,
    exit_reasons: localExitReasons,
    symbol: snapshot.symbol,
    timeframe: snapshot.timeframe,
    exchange: snapshot.exchange,
    range: state.replayWindow,
  });
  replaceFillsOnNextSync = false;
  pendingSyncSnapshot = undefined;
  pendingSyncFills = [];
}

function queueLocalSync(snapshot: LocalSnapshot): void {
  if (!syncLocal) return;
  pendingSyncSnapshot = snapshot;
  const newFills = snapshot.fills
    .filter((fill) => !seenFills.has(fillKey(fill)))
    .reverse();
  for (const fill of newFills) {
    seenFills.add(fillKey(fill));
    pendingSyncFills.push(fill);
  }
  if (replaceFillsOnNextSync || newFills.length > 0) {
    if (syncTimer !== undefined) clearTimeout(syncTimer);
    flushLocalSync();
  } else if (syncTimer === undefined) {
    syncTimer = setTimeout(flushLocalSync, SERVER_SYNC_INTERVAL_MS);
  }
}

function setLocalSnapshot(snapshot: LocalSnapshot): void {
  localEntryProtections = {
    ...localEntryProtections,
    ...snapshot.entry_protections,
  };
  localExitReasons = {
    ...localExitReasons,
    ...snapshot.exit_reasons,
  };
  if (snapshot.entry_protections) {
    store.mergeEntryProtections(snapshot.entry_protections);
  }
  if (snapshot.full_reset) {
    if (renderTimer !== undefined) clearTimeout(renderTimer);
    renderTimer = undefined;
    pendingRenderSnapshot = undefined;
    applyLocalSnapshot(snapshot);
    queueLocalSync(snapshot);
    return;
  }
  if (pendingRenderSnapshot?.full_reset) {
    queueLocalSync(snapshot);
    return;
  }
  const pending = pendingRenderSnapshot;
  pendingRenderSnapshot = pending
    ? {
        ...snapshot,
        full_reset: false,
        candles_delta: [
          ...(pending.full_reset ? [] : pending.candles_delta ?? []),
          ...(snapshot.candles_delta ?? []),
        ],
        fills: [...pending.fills, ...snapshot.fills],
      }
    : snapshot;
  const renderInterval = snapshot.replay.total > LARGE_SERIES_THRESHOLD
    ? LARGE_SERIES_RENDER_INTERVAL_MS
    : LOCAL_RENDER_INTERVAL_MS;
  const elapsed = Date.now() - lastRenderAt;
  if (elapsed >= renderInterval) {
    if (renderTimer !== undefined) clearTimeout(renderTimer);
    renderTimer = undefined;
    const latest = pendingRenderSnapshot;
    pendingRenderSnapshot = undefined;
    if (latest) applyLocalSnapshot(latest);
  } else if (renderTimer === undefined) {
    renderTimer = setTimeout(() => {
      renderTimer = undefined;
      const latest = pendingRenderSnapshot;
      pendingRenderSnapshot = undefined;
      if (latest) applyLocalSnapshot(latest);
    }, renderInterval - elapsed);
  }
  queueLocalSync(snapshot);
}

export function isLocalEngineActive(): boolean {
  return worker !== null && ready;
}

export function isLocalEngineInitializing(): boolean {
  return initializing;
}

function keyFor(
  exchange: ExchangeId,
  symbol: string,
  timeframe: string,
): string {
  const range = getState().replayWindow;
  return `${exchange}|${symbol}|${timeframe}|${range.start_ms}|${range.end_ms}`;
}

function matchesWorkerSeries(snapshot: LocalSnapshot): boolean {
  const [exchange, symbol, timeframe] = workerKey.split("|");
  return snapshot.exchange === exchange
    && snapshot.symbol === symbol
    && snapshot.timeframe === timeframe;
}

/**
 * Load `symbol@timeframe` into this tab's Rust/WASM engine.
 *
 * Market data comes from the browser (see ../market/provider); the engine then
 * owns the clock, orders, positions and PnL exactly as before. `send` is only
 * used to mirror the local account to the server for the challenge/diary.
 */
export async function initializeLocalEngine(
  symbol: string,
  timeframe: string,
  exchange: ExchangeId,
  send: (message: ClientMessage) => void,
): Promise<void> {
  syncLocal = send;
  // Re-mirror the stop/TP this tab already planned before the reload (see
  // `persistedProtections`). Engine values still win: they are the live ones.
  localEntryProtections = { ...persistedProtections(), ...localEntryProtections };
  if (!isTimeframe(timeframe)) {
    store.setError(`Неизвестный таймфрейм ${timeframe}; доступны 5m, 1h и 1d`);
    return;
  }
  const state = getState();
  const key = keyFor(exchange, symbol, timeframe);
  if (worker && workerKey === key) return;
  if (initializing && workerKey === key) return;
  const previousKey = workerKey;
  const previous = previousKey ? previousKey.split("|") : [];
  const [previousExchange, previousSymbol] = previous;
  const preserveEngine = worker !== null && ready;
  // Switching coin or venue is not a series refresh: the old replay must stop
  // NOW, not after the new data finishes loading. Otherwise its timer keeps
  // ticking in the worker (bars keep appearing) while the new chart is fetched.
  const seriesChanged = preserveEngine
    && (previousExchange !== exchange || previousSymbol !== symbol);
  if (seriesChanged) {
    worker?.postMessage({ type: "command", command: { type: "pause" } });
  }
  const requestGeneration = ++generation;
  workerKey = key;
  initializing = true;
  store.setSeriesLoading(`${symbol} ${timeframe}: загрузка с биржи…`);
  const nextWorker = worker ?? new Worker(new URL("./engine.worker.ts", import.meta.url), {
    type: "module",
  });
  if (!worker) {
    engineId = newEngineId();
    // Fill keys are only unique inside one engine instance (order ids restart
    // at 1), so a rebuilt engine must not inherit the old seen-set: its first
    // fills would look like duplicates and never reach the server.
    seenFills.clear();
  }
  nextWorker.onmessage = (event: MessageEvent<WorkerResponse>) => {
    if (requestGeneration !== generation || nextWorker !== worker) return;
    if (event.data.type === "error") {
      initializing = false;
      if (!preserveEngine) {
        nextWorker.terminate();
        worker = null;
        ready = false;
      } else {
        workerKey = previousKey;
      }
      store.setSeriesLoading(null);
      store.setError(event.data.message ?? "Local replay engine failed");
      return;
    }
    if (event.data.snapshot) {
      if (!matchesWorkerSeries(event.data.snapshot)) return;
      initializing = false;
      ready = true;
      setLocalSnapshot(event.data.snapshot);
    }
  };
  nextWorker.onerror = (event: ErrorEvent) => {
    if (requestGeneration !== generation) return;
    initializing = false;
    if (!preserveEngine) {
      nextWorker.terminate();
      worker = null;
      ready = false;
    } else {
      workerKey = previousKey;
    }
    store.setSeriesLoading(null);
    store.setError(`Local replay engine failed: ${event.message}`);
  };
  worker = nextWorker;

  try {
    const { base, rows, revealResetMs, startMs } = await loadSeries(symbol, exchange, timeframe);
    if (requestGeneration !== generation) return;
    // Register before the worker answers: its first snapshot is a full reset
    // that rebuilds the chart from exactly these bars.
    loadedRows.set(key, { base });
    store.setSeriesLoading(null);
    store.finishPreload(1, 1, 0);
    const oi = await loadOiForSymbol(exchange, symbol, timeframe);
    if (requestGeneration !== generation) return;
    const stepMs = TIMEFRAME_MS[timeframe];
    if (preserveEngine) {
      const previousRange = previous.slice(3);
      const range = state.replayWindow;
      // A different coin starts FRESH: reset to its own reveal boundary and
      // pause. Inheriting the previous coin's clock made the new chart look
      // already replayed (and, if the old replay was playing, it silently
      // continued from the old coin's time). A timeframe switch of the SAME
      // coin still keeps the clock, which is what traders expect.
      const sameCoin = previousSymbol === symbol && previousExchange === exchange;
      const preserveClock = sameCoin
        && previousRange[0] === String(range.start_ms)
        && previousRange[1] === String(range.end_ms);
      nextWorker.postMessage({
        type: "command",
        command: {
          type: "switch_series",
          symbol,
          exchange,
          timeframe,
          step_ms: stepMs,
          speed: state.replay.speed,
          market_kind: "linear",
          engine_id: engineId,
          reveal_reset_ms: revealResetMs,
          range_end_ms: range.end_ms,
          oi: oi.samples,
          oi_period: oi.period,
          preserve_clock: preserveClock,
          candles: rows,
        },
      });
    } else {
      nextWorker.postMessage({
        type: "init",
        symbol,
        exchange,
        timeframe,
        start_ms: startMs,
        step_ms: stepMs,
        speed: state.replay.speed,
        market_kind: "linear",
        engine_id: engineId,
        reveal_reset_ms: revealResetMs,
        range_end_ms: state.replayWindow.end_ms,
        oi: oi.samples,
        oi_period: oi.period,
        candles: rows,
      });
    }
  } catch (error) {
    if (requestGeneration !== generation) return;
    initializing = false;
    if (!preserveEngine) {
      nextWorker.terminate();
      worker = null;
      ready = false;
    } else {
      workerKey = previousKey;
    }
    store.setSeriesLoading(null);
    store.setError(`Не удалось загрузить данные с биржи: ${String(error)}`);
  }
}

/** OI on the grid matching the chart's timeframe (empty if the venue has none). */
async function loadOiForSymbol(
  exchange: ExchangeId,
  symbol: string,
  timeframe: Timeframe,
): Promise<{ samples: Array<[number, number]>; period: string }> {
  try {
    return await loadOi(exchange, symbol, timeframe);
  } catch {
    // OI is chart context only: a venue without it must never block replay.
    return { samples: [], period: "" };
  }
}

export function handleLocalCommand(message: {
  type: string;
  action?: string;
  steps?: number;
  speed?: number;
  step_ms?: number;
  timestamp_ms?: number | null;
  symbol?: string;
  side?: string;
  order_type?: string;
  quantity?: number | null;
  price?: number | null;
  stop_price?: number | null;
  tp_price?: number | null;
  stop_loss_price?: number | null;
  risk_usdt?: number;
  time_in_force?: string;
  post_only?: boolean;
  reduce_only?: boolean;
  order_id?: string;
}): boolean {
  if (!isLocalEngineActive() || !worker) return false;
  if (message.type === "replay") {
    const command: Record<string, unknown> = { type: message.action };
    if (message.steps !== undefined) command["steps"] = message.steps;
    if (message.speed !== undefined) command["speed"] = message.speed;
    if (message.step_ms !== undefined) command["step_ms"] = message.step_ms;
    if (message.timestamp_ms != null) command["timestamp_ms"] = message.timestamp_ms;
    worker.postMessage({ type: "command", command });
    return true;
  }
  if (message.type === "place_order") {
    worker.postMessage({
      type: "command",
      command: {
        type: "place_order",
        symbol: message.symbol,
        side: message.side,
        order_type: message.order_type,
        quantity: message.quantity,
        price: message.price,
        stop_price: message.stop_price,
        tp_price: message.tp_price,
        stop_loss_price: message.stop_loss_price,
        risk_usdt: message.risk_usdt,
        time_in_force: message.time_in_force,
        post_only: message.post_only,
        reduce_only: message.reduce_only,
      },
    });
    return true;
  }
  if (message.type === "cancel_order") {
    worker.postMessage({
      type: "command",
      command: { type: "cancel_order", symbol: message.symbol, order_id: message.order_id },
    });
    return true;
  }
  if (message.type === "close_position") {
    worker.postMessage({
      type: "command",
      command: { type: "close_position", symbol: message.symbol, quantity: message.quantity },
    });
    return true;
  }
  return false;
}
