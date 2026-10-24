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

export function requestLocalSync(replaceFills = false): void {
  replaceFillsOnNextSync = replaceFillsOnNextSync || replaceFills;
  if (ready) worker?.postMessage({ type: "snapshot", include_fills: true });
}

let worker: Worker | null = null;
let workerKey = "";
let currentSeriesCandles: CandleRow[] = [];
let generation = 0;
let initializing = false;
let ready = false;
let syncLocal: ((message: ClientMessage) => void) | undefined;
let replaceFillsOnNextSync = true;
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

function applyLocalSnapshot(snapshot: LocalSnapshot): void {
  if (snapshot.full_reset) {
    if (currentSeriesCandles.length === 0) {
      store.setError("Локальный реплей не получил свечи для полного обновления");
      return;
    }
    store.applySnapshot(
      snapshot.symbol,
      snapshot.exchange,
      snapshot.timeframe,
      currentSeriesCandles.slice(0, snapshot.replay.fed),
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
  const replaceFills = replaceFillsOnNextSync;
  syncLocal({
    type: "local_sync",
    account: snapshot.account,
    fills: replaceFills ? snapshot.fills.slice().reverse() : pendingSyncFills,
    replace_fills: replaceFills,
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

function keyFor(message: Extract<ServerMessage, { type: "snapshot" }>): string {
  const range = getState().replayWindow;
  return `${message.exchange}|${message.symbol}|${message.timeframe}|${range.start_ms}|${range.end_ms}`;
}

function matchesWorkerSeries(snapshot: LocalSnapshot): boolean {
  const [exchange, symbol, timeframe] = workerKey.split("|");
  return snapshot.exchange === exchange
    && snapshot.symbol === symbol
    && snapshot.timeframe === timeframe;
}

export function isLocalSnapshotCurrent(
  message: Extract<ServerMessage, { type: "snapshot" }>,
): boolean {
  return isLocalEngineActive() && workerKey === keyFor(message);
}

export async function initializeLocalEngine(
  message: Extract<ServerMessage, { type: "snapshot" }>,
  apiBase: string,
  send: (message: ClientMessage) => void,
): Promise<void> {
  syncLocal = send;
  const state = getState();
  const key = keyFor(message);
  if (worker && workerKey === key) return;
  if (initializing && workerKey === key) return;
  const previousKey = workerKey;
  const preserveEngine = worker !== null && ready;
  const previousCandles = currentSeriesCandles;
  const requestGeneration = ++generation;
  workerKey = key;
  initializing = true;
  let historyLoaded = false;
  const nextWorker = worker ?? new Worker(new URL("./engine.worker.ts", import.meta.url), {
    type: "module",
  });
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
        currentSeriesCandles = previousCandles;
      }
      store.setError(event.data.message ?? "Local replay engine failed");
      if (historyLoaded && !preserveEngine) {
        syncLocal?.({ type: "replay", action: "play" });
      }
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
      currentSeriesCandles = previousCandles;
    }
    store.setError(`Local replay engine failed: ${event.message}`);
    if (historyLoaded && !preserveEngine) {
      syncLocal?.({ type: "replay", action: "play" });
    }
  };
  worker = nextWorker;

  try {
    const range = state.replayWindow;
    const params = new URLSearchParams({
      exchange: message.exchange,
      symbol: message.symbol,
      timeframe: message.timeframe,
      start_ms: String(range.start_ms),
      end_ms: String(range.end_ms),
    });
    const response = await fetch(`${apiBase}/api/candles?${params.toString()}`);
    const result = await response.json() as {
      error?: string;
      candles?: CandleRow[];
      start_ms?: number;
      end_ms?: number;
    };
    if (!response.ok) {
      throw new Error(result.error ?? `History request failed (${response.status})`);
    }
    if (result.error) throw new Error(result.error);
    if (!Array.isArray(result.candles) || result.candles.length === 0) {
      throw new Error("The server returned no candles for the selected replay range");
    }
    if (requestGeneration !== generation) return;
    currentSeriesCandles = result.candles;
    historyLoaded = true;
    const stepMs = message.replay.step_ms;
    const reveal = range.reveal_start_ms;
    const firstRevealed = result.candles.find((candle) => candle[0] >= reveal);
    const revealResetMs = firstRevealed
      ? firstRevealed[0] - stepMs
      : message.replay.now_ms;
    const startMs = Math.max(result.start_ms ?? result.candles[0][0], revealResetMs);
    if (preserveEngine) {
      const previousRange = previousKey.split("|").slice(3);
      const preserveClock = previousRange[0] === String(range.start_ms)
        && previousRange[1] === String(range.end_ms);
      nextWorker.postMessage({
        type: "command",
        command: {
          type: "switch_series",
          symbol: message.symbol,
          exchange: message.exchange,
          timeframe: message.timeframe,
          step_ms: stepMs,
          speed: state.replay.speed,
          market_kind: "linear",
          reveal_reset_ms: revealResetMs,
          range_end_ms: range.end_ms,
          oi: message.oi,
          oi_period: message.oi_period,
          preserve_clock: preserveClock,
          candles: result.candles,
        },
      });
    } else {
      nextWorker.postMessage({
        type: "init",
        symbol: message.symbol,
        exchange: message.exchange,
        timeframe: message.timeframe,
        start_ms: startMs,
        step_ms: stepMs,
        speed: message.replay.speed,
        market_kind: "linear",
        reveal_reset_ms: revealResetMs,
        range_end_ms: range.end_ms,
        oi: message.oi,
        oi_period: message.oi_period,
        candles: result.candles,
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
      currentSeriesCandles = previousCandles;
    }
    store.setError(`Не удалось запустить локальный реплей: ${String(error)}`);
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
