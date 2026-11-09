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
import { peekBaseBars, loadBaseBars } from "../market/provider";
import {
  BASE_TIMEFRAME,
  TIMEFRAME_MS,
  isTimeframe,
  timeframeOr,
  type Timeframe,
} from "../market/timeframes";
import {
  fillIdentity,
  groupHistory,
  tagEngineFills,
  type HistoryGroup,
} from "./historyRestore";

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
  last_price?: number;
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
 * The engine keeps a large USDT buffer so a risk-sized order is never blocked
 * by margin (a wide stop needs more notional than the challenge capital).
 * That buffer must never be shown as the trader's equity: a board reading
 * "капитал 1000 / equity 1,000,000" looks broken. Equity in the trader's
 * terms is the challenge capital plus PnL.
 */
function displayAccount(account: AccountSnapshot): AccountSnapshot {
  const capital = getState().initialCapitalUsdt;
  if (!(capital > 0) || !Number.isFinite(capital)) return account;
  const pnl = account.pnl.realized + account.pnl.unrealized;
  if (!Number.isFinite(pnl)) return account;
  return { ...account, equity: capital + pnl };
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

/**
 * Re-mirror the trade history this browser persisted into a fresh server
 * session.
 *
 * Closing the tab drops the `sessionStorage` token, so the server starts with
 * an empty `local_fills` while the Trades table is rebuilt from localStorage.
 * The WASM engine replays the market but never re-creates old executions, so
 * without this the diary of a reopened site stays empty. Fills are restored
 * under the engine instance that produced them; legacy rows without an id go
 * under one stable id.
 */
export function restoreLocalHistory(): void {
  restoreHistoryOnNextSync = true;
  // The engine may already be running (server restart, not a page reload) and
  // hold fills that `pendingSyncFills` considers already seen. Send its whole
  // snapshot once so a fresh session gets them too.
  mirrorAllFillsOnNextSync = true;
  if (ready) worker?.postMessage({ type: "snapshot", include_fills: true });
}

/** Symbol field of a fill/protection key (`id|symbol|...`). */
function keySymbol(key: string): string {
  return key.split("|")[1] ?? "";
}

/**
 * Delete every trace of `symbol` from the running engine and from the server's
 * mirrored ledger — the source the diary is reconstructed from.
 *
 * `store.resetSymbolData()` clears the browser's own copy. Without this the
 * WASM engine still held the fills and the server still held them in
 * `local_fills`, so a diary refresh (or the next full sync) brought every
 * "deleted" trade back.
 */
export function clearLocalSymbol(symbol: string = getState().symbol): void {
  if (!symbol) return;
  const belongsToSymbol = (key: string): boolean => keySymbol(key) === symbol;
  // These maps are merged into the server's session, so a cleared trade's stop
  // or exit verdict has to leave both copies.
  localEntryProtections = Object.fromEntries(
    Object.entries(localEntryProtections).filter(([key]) => !belongsToSymbol(key)),
  );
  localExitReasons = Object.fromEntries(
    Object.entries(localExitReasons).filter(([key]) => !belongsToSymbol(key)),
  );
  persistExitReasons(localExitReasons);
  for (const key of Array.from(seenFills)) {
    if (belongsToSymbol(key)) seenFills.delete(key);
  }
  pendingSyncFills = pendingSyncFills.filter((fill) => fill.symbol !== symbol);
  // Never let a sync queued before the click replace the server's ledger with
  // the pre-clear fills.
  pendingSyncSnapshot = undefined;
  replaceFillsOnNextSync = false;
  if (syncTimer !== undefined) {
    clearTimeout(syncTimer);
    syncTimer = undefined;
  }
  pendingClearSymbol = symbol;
  worker?.postMessage({ type: "command", command: { type: "clear_symbol", symbol } });
  // Send the explicit clear straight away when the current account is known;
  // otherwise the engine's answer carries it on the next mirror sync.
  const state = getState();
  if (syncLocal && state.account) {
    syncLocal({
      type: "local_sync",
      account: displayAccount(state.account),
      fills: [],
      replace_fills: false,
      clear_symbol: symbol,
      engine_id: engineId,
      entry_protections: localEntryProtections,
      exit_reasons: localExitReasons,
      symbol: state.symbol,
      timeframe: state.timeframe,
      exchange: state.exchange,
      range: state.replayWindow,
    });
  }
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
/**
 * Coin deleted with the chart's trash. The next mirror sync carries it so the
 * server drops the rows from its ledger even when the sync merges (the normal
 * case) instead of replacing.
 */
let pendingClearSymbol: string | null = null;
let pendingSyncSnapshot: LocalSnapshot | undefined;
let pendingSyncFills: FillLine[] = [];
/**
 * A fresh server session has no fills of its own, while this browser still has
 * the history persisted in localStorage. Mirror it once, under the engine ids
 * that produced it, or reopening the site leaves the diary empty even though
 * the Trades table is rebuilt from that same localStorage.
 */
let restoreHistoryOnNextSync = false;
/**
 * Same situation, but for an engine that is already running (server restart):
 * its fills are not "new", so `pendingSyncFills` would not carry them. Send
 * the whole snapshot once instead of just the delta.
 */
let mirrorAllFillsOnNextSync = false;
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

// Exit verdicts ("stop" / "take") live in this module, but the diary is
// rebuilt after a reload from what the server mirrored. Persisting them next
// to the fills keeps the "Итог" column correct after reopening the site, not
// just while this tab is alive.
const EXIT_REASONS_KEY = "simccxt.trade-exit-reasons.v1";
const EXIT_REASONS_LIMIT = 500;
/**
 * Stable id for fills persisted before the browser started tagging them with
 * an engine id. It must survive reloads: re-sending the same legacy rows under
 * a new id on every page load would let the server count them twice.
 */
const LEGACY_HISTORY_ENGINE_KEY = "simccxt.trade-history-engine.v1";

function loadStoredExitReasons(): Record<string, string> {
  try {
    const raw = window.localStorage.getItem(EXIT_REASONS_KEY);
    if (!raw) return {};
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
    const out: Record<string, string> = {};
    for (const [key, value] of Object.entries(parsed)) {
      if (typeof value === "string" && value.length > 0) out[key] = value;
    }
    return out;
  } catch {
    return {};
  }
}

function persistExitReasons(reasons: Record<string, string>): void {
  try {
    const ids = Object.keys(reasons);
    if (ids.length > EXIT_REASONS_LIMIT) {
      for (const id of ids.slice(0, ids.length - EXIT_REASONS_LIMIT)) delete reasons[id];
    }
    window.localStorage.setItem(EXIT_REASONS_KEY, JSON.stringify(reasons));
  } catch {
    // Losing an exit label must never break trading.
  }
}

function legacyHistoryEngineId(): string {
  try {
    const saved = window.localStorage.getItem(LEGACY_HISTORY_ENGINE_KEY);
    if (saved) return saved;
  } catch {
    // private mode: fall through to an in-memory id
  }
  const id = `history-${newEngineId()}`;
  try {
    window.localStorage.setItem(LEGACY_HISTORY_ENGINE_KEY, id);
  } catch {
    // ignore
  }
  return id;
}

/**
 * The persisted history grouped for re-mirroring into a fresh server session.
 * The grouping/merging itself lives in `historyRestore.ts` so it stays pure
 * and unit-tested; this wrapper only supplies the live state and storage.
 */
function historyGroups(clearSymbol: string | null): HistoryGroup[] {
  return groupHistory(
    getState().fills,
    seenFills,
    engineId,
    legacyHistoryEngineId(),
    clearSymbol,
    persistedProtections(),
    loadStoredExitReasons(),
  );
}

function applyLocalSnapshot(snapshot: LocalSnapshot): void {
  const account = displayAccount(snapshot.account);
  // The engine aggregates the chart's timeframe from the same base bars that
  // triggered the orders, so its rows already reflect how far the clock has
  // actually gone inside the newest bar. Prefer them: recomputing from the
  // page's full base used to show the whole day's (future) close as "now".
  const engineRows = snapshot.candles_delta ?? [];
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
    const timeframe = timeframeOr(snapshot.timeframe, BASE_TIMEFRAME);
    let rows = engineRows;
    if (rows.length === 0) {
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
      rows = aggregateCandles(base, timeframe).slice(0, snapshot.replay.fed);
    }
    store.applySnapshot(
      snapshot.symbol,
      snapshot.exchange,
      timeframe,
      rows,
      account,
      snapshot.replay,
      tagEngineFills(snapshot.fills, engineId),
    );
  } else {
    store.applyLocalUpdate(
      engineRows,
      account,
      snapshot.replay,
      tagEngineFills(snapshot.fills, engineId),
    );
  }
  if (snapshot.last_price !== undefined && Number.isFinite(snapshot.last_price) && snapshot.last_price > 0) {
    store.applyMarket(snapshot.last_price);
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
  const clearSymbol = pendingClearSymbol;
  const mirrorAll = mirrorAllFillsOnNextSync;
  const restoreHistory = restoreHistoryOnNextSync;
  const send = syncLocal;
  const common = {
    type: "local_sync" as const,
    account: displayAccount(snapshot.account),
    clear_symbol: clearSymbol ?? undefined,
    symbol: snapshot.symbol,
    timeframe: snapshot.timeframe,
    exchange: snapshot.exchange,
    range: state.replayWindow,
  };
  // Historical groups go first, the live engine last: the server reads the
  // last engine id as "current" when it decides which open positions to show.
  if (restoreHistory) {
    for (const group of historyGroups(clearSymbol)) {
      send({
        ...common,
        fills: group.fills,
        replace_fills: false,
        engine_id: group.engineId,
        entry_protections: group.entryProtections,
        exit_reasons: group.exitReasons,
      });
    }
  }
  send({
    ...common,
    fills: replaceFills || mirrorAll ? snapshot.fills.slice().reverse() : pendingSyncFills,
    replace_fills: replaceFills,
    engine_id: engineId,
    entry_protections: localEntryProtections,
    exit_reasons: localExitReasons,
  });
  replaceFillsOnNextSync = false;
  restoreHistoryOnNextSync = false;
  mirrorAllFillsOnNextSync = false;
  pendingClearSymbol = null;
  pendingSyncSnapshot = undefined;
  pendingSyncFills = [];
}

function queueLocalSync(snapshot: LocalSnapshot): void {
  if (!syncLocal) return;
  pendingSyncSnapshot = snapshot;
  const newFills = snapshot.fills
    .filter((fill) => !seenFills.has(fillIdentity(fill)))
    .reverse();
  for (const fill of newFills) {
    seenFills.add(fillIdentity(fill));
    pendingSyncFills.push(fill);
  }
  if (replaceFillsOnNextSync || pendingClearSymbol !== null || newFills.length > 0) {
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
  const incomingExitReasons = snapshot.exit_reasons ?? {};
  localExitReasons = {
    ...localExitReasons,
    ...incomingExitReasons,
  };
  if (Object.keys(incomingExitReasons).length > 0) {
    persistExitReasons(localExitReasons);
  }
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
  // Re-mirror the stop/TP and exit verdicts this tab already planned before
  // the reload. Engine values still win: they are the live ones.
  localEntryProtections = { ...persistedProtections(), ...localEntryProtections };
  localExitReasons = { ...loadStoredExitReasons(), ...localExitReasons };
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
          preserve_clock: preserveClock,
          candles: rows,
          base_candles: base,
          base_step_ms: TIMEFRAME_MS[BASE_TIMEFRAME],
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
        candles: rows,
        base_candles: base,
        base_step_ms: TIMEFRAME_MS[BASE_TIMEFRAME],
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
