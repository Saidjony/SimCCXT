// The terminal's market-data provider.
//
// One call, one window of 5m bars: memory → OPFS/IndexedDB cache → direct
// exchange download. This is the browser-side equivalent of the old
// `data/ohlcv` disk cache that used to live on the server; the server is no
// longer involved in market data at all.
//
// Rules enforced here (each one is a fixed bug, not a preference):
// 1. ONLY 5m is ever downloaded. 1h/1d are aggregated from it (./aggregate.ts).
// 2. A window is downloaded ONCE. If the cache already covers it for the same
//    requested window — down to the same end date — nothing is fetched again,
//    even if the clock has since moved past that end. Otherwise opening the
//    same coin twice re-downloaded up to a day of bars for nothing.
// 3. Every cache write also records an expiry: the moment the requested end
//    date + one day. Until then the series is served from disk untouched; after
//    it, the window is refreshed once.
// 4. All requests for one series are serialized through a single queue, so
//    chart panes, the warm-up and a coin switch can never duplicate a download.

import type { CandleRow, ExchangeId } from "../types/protocol.ts";
import {
  clearCachedBars,
  loadAllWindows,
  readCachedBars,
  writeCachedBars,
} from "./cache.ts";
import { downloadBars, downloadOi } from "./exchanges.ts";
import type { OiSeries } from "./exchanges.ts";
import { BASE_TIMEFRAME } from "./timeframes.ts";

/** Bars kept per series in memory (a 2-year 5m window is ~210k). */
const MEMORY_LIMIT = 250_000;
const BAR_MS = 300_000;
const DAY_MS = 86_400_000;
/** Slack at the START of a window: the venue may have no bar exactly on the
 *  boundary (a listing, a short outage), so one bar of tolerance is fine. */
const COVERAGE_SLACK_MS = BAR_MS;
/** Slack at the END of a window: the venue returns the bar that is still
 *  forming, so the last held bar sits at most a bar behind the requested end.
 *  Deliberately NOT days: a window is not a tolerance, and the diary asks for
 *  several DISJOINT windows (one around each trade). A three-day slack made
 *  the first window's bars "cover" a window a day later, so the later trades
 *  never downloaded and their chart stayed empty. */
const END_SLACK_MS = 2 * BAR_MS;
/** A saved window is recorded one bar wider on each side (see `loadBaseBars`). */
const PAD_MS = BAR_MS;
/** A saved window still counts as "the same window" within this margin. */
const REFRESH_SLACK_MS = 10 * 60_000;

export interface LoadOptions {
  /** Re-download even when the cache covers the range. */
  force?: boolean;
  signal?: AbortSignal;
  onProgress?: (done: number, total: number) => void;
}

interface MemoryEntry {
  exchange: ExchangeId;
  symbol: string;
  rows: CandleRow[];
}

interface CachedRange {
  /** First and last bar requested (bar-aligned, inclusive). */
  from: number;
  to: number;
  /** When this window should be refreshed again. */
  freshUntil: number;
}

const memory = new Map<string, MemoryEntry>();

/** How many kline downloads this page has performed, per `exchange|symbol`. */
const downloadsBySeries = new Map<string, number>();

/** Per-series download counters (a page refresh resets them). */
export function downloadStats(): Record<string, number> {
  return Object.fromEntries(downloadsBySeries);
}
/** Per-series serialization of downloads. */
const queues = new Map<string, Promise<unknown>>();
/**
 * Windows already downloaded for a series. A series can hold several DISJOINT
 * ones — the diary asks for one window around each trade and never for the
 * days between them — so this must stay a list, not "the newest window".
 */
const covered = new Map<string, CachedRange[]>();

/** Record one window, keeping the newer expiry for an identical one. */
function rememberWindow(key: string, range: CachedRange): void {
  const ranges = covered.get(key) ?? [];
  const at = ranges.findIndex((r) => r.from === range.from && r.to === range.to);
  if (at < 0) ranges.push(range);
  else if (range.freshUntil > ranges[at].freshUntil) ranges[at] = range;
  ranges.sort((a, b) => a.from - b.from);
  covered.set(key, ranges);
}

/** Manifest windows are read once per page and then kept in step locally. */
let windowsLoaded: Promise<void> | null = null;
function ensureWindowsLoaded(): Promise<void> {
  windowsLoaded ??= loadAllWindows().then((byKey) => {
    for (const [key, ranges] of byKey) {
      // Merge rather than replace: a download may already have completed while
      // the manifest was being read, and its window is just as real.
      for (const range of ranges) rememberWindow(key, range);
    }
  });
  return windowsLoaded;
}
const inflight = new Map<string, Promise<CandleRow[]>>();

function keyOf(exchange: string, symbol: string): string {
  return `${exchange}|${symbol}`;
}

/** Bar-aligned first/last bar of a requested [start, end) window. */
function barRange(startMs: number, endMs: number): { from: number; to: number } {
  const from = Math.floor(startMs / BAR_MS) * BAR_MS;
  const to = Math.max(from, Math.floor((endMs - 1) / BAR_MS) * BAR_MS);
  return { from, to };
}

/** Bars of `rows` inside [start, end] (binary search on the sorted array). */
function slice(rows: readonly CandleRow[], startMs: number, endMs: number): CandleRow[] {
  let low = 0;
  let high = rows.length;
  while (low < high) {
    const middle = (low + high) >> 1;
    if (rows[middle][0] < startMs) low = middle + 1;
    else high = middle;
  }
  const begin = low;
  while (low < rows.length && rows[low][0] <= endMs) low += 1;
  return rows.slice(begin, low);
}

/** Bars inside the requested [start, end) window. */
function windowOf(rows: readonly CandleRow[], startMs: number, endMs: number): CandleRow[] {
  return rows.filter((row) => row[0] >= startMs && row[0] < endMs);
}

/** Union of two bar arrays, newer row winning on the same timestamp. */
function mergeRows(a: readonly CandleRow[], b: readonly CandleRow[]): CandleRow[] {
  const byTime = new Map<number, CandleRow>();
  for (const row of a) byTime.set(row[0], row);
  for (const row of b) byTime.set(row[0], row);
  return [...byTime.values()].sort((x, y) => x[0] - y[0]);
}

/** Extra bars a default ("ends NOW") window gets on each side. */
const NOW_PAD_BARS = 8;

/**
 * Whether a window's end is "now" rather than a date the user typed: the site's
 * default window ends at `Date.now()`, a few seconds before the next page load
 * computes it again.
 */
function endsAtNow(endMs: number, now: number): boolean {
  return endMs > now - 6 * 3_600_000;
}

/** The last end date a window was downloaded for (a date the user can type). */
const END_KEY = "simccxt.request-end.v1";

function sameEnd(endMs: number, now: number): boolean {
  try {
    const raw = localStorage.getItem(END_KEY);
    if (raw === null) return endsAtNow(endMs, now);
    return Math.abs(Number(raw) - Math.floor(endMs / BAR_MS) * BAR_MS) <= BAR_MS;
  } catch {
    return endsAtNow(endMs, now);
  }
}

function rememberEnd(endMs: number): void {
  try {
    localStorage.setItem(END_KEY, String(Math.floor(endMs / BAR_MS) * BAR_MS));
  } catch {
    // storage unavailable: the inference above still applies this page
  }
}

/** The default window is pinned by this bar offset, so repeated loads match. */
const REQUEST_KEY = "simccxt.request-window.v1";

function rememberedShift(): number {
  try {
    const raw = localStorage.getItem(REQUEST_KEY);
    if (raw !== null) {
      const value = Number(raw);
      if (Number.isFinite(value)) return value;
    }
  } catch {
    // storage unavailable: fall back to no shift
  }
  return 1;
}

function rememberShift(shift: number): void {
  try {
    localStorage.setItem(REQUEST_KEY, String(shift));
  } catch {
    // storage unavailable: the shift stays for this page only
  }
}

/** Whether the in-memory series covers the requested bar window. */
function memoryCovers(entry: MemoryEntry | undefined, from: number, to: number): boolean {
  if (!entry || entry.rows.length === 0) return false;
  const first = entry.rows[0][0];
  const last = entry.rows[entry.rows.length - 1][0];
  // `from = 0` means "from the very first held bar" (see `peekBaseBars`).
  if (from > 0 && first > from + COVERAGE_SLACK_MS) return false;
  // `to = 0` means "no upper bound" (a worker restart wants everything).
  if (to === 0) return true;
  // A window that begins after the bars we hold is a DIFFERENT window, not a
  // partial hit: the diary loads one window per trade, and treating it as
  // covered returned an empty slice while the chart still showed the first
  // window — the trade looked like it never happened.
  if (from > last + COVERAGE_SLACK_MS) return false;
  return last >= to - END_SLACK_MS;
}

/**
 * Remember which window a series was requested for, both in memory and in the
 * manifest ("downloaded once" survives a page reload, not just a tab switch).
 */
async function recordWindow(key: string, exchange: ExchangeId, symbol: string, range: CachedRange): Promise<void> {
  // Synchronously first: a second caller for the same series must see the
  // record even before the manifest write commits.
  rememberWindow(key, range);
  await writeCachedBars(exchange, symbol, null, range);
}

/** Serialize work per series: one download at a time, everybody else waits. */
function serialize<T>(key: string, work: () => Promise<T>): Promise<T> {
  const previous = queues.get(key) ?? Promise.resolve();
  const next = previous.then(work, work);
  queues.set(key, next.catch(() => undefined));
  return next;
}

/**
 * 5m bars for `symbol` inside [startMs, endMs) on `exchange`.
 *
 * Served from memory or the browser cache whenever that cache already covers
 * this exact window; downloaded (once) otherwise.
 */
export async function loadBaseBars(
  exchange: ExchangeId,
  symbol: string,
  startMs: number,
  endMs: number,
  options: LoadOptions = {},
): Promise<CandleRow[]> {
  if (!(startMs < endMs)) return [];
  const key = keyOf(exchange, symbol);
  const nowAtRequest = Date.now();
  // Pad the request itself when it ends "now": the next load computes a bar or
  // two later, and would otherwise ask for a window the cache does not cover.
  // "Same window" is judged against the END DATE the user asked for: a typed
  // date is stable, while the default end is `Date.now()` and moves every load.
  const shift = sameEnd(endMs, nowAtRequest) ? Math.max(0, rememberedShift()) * BAR_MS : 0;
  const { from: rawFrom, to: rawTo } = barRange(startMs, endMs);
  const from = rawFrom;
  const to = rawTo + shift;
  const requestKey = `${key}|${from}|${to}`;

  if (!options.force && memoryCovers(memory.get(key), from, to)) {
    return windowOf(memory.get(key)!.rows, startMs, endMs);
  }
  const running = inflight.get(requestKey);
  if (!options.force && running) return running.then((rows) => windowOf(rows, startMs, endMs));

  const task = serialize(key, async () => {
    const now = Date.now();
    // Memory may have been filled while this call waited in the queue.
    if (!options.force && memoryCovers(memory.get(key), from, to)) {
      return windowOf(memory.get(key)!.rows, startMs, endMs);
    }
    // What this series already has: after a reload the in-memory record is
    // gone, so the manifest is read once and consulted here.
    await ensureWindowsLoaded();
    const windows = covered.get(key) ?? [];
    const covers = (window: CachedRange): boolean =>
      window.from <= from + REFRESH_SLACK_MS && window.to >= to - REFRESH_SLACK_MS;

    // A window downloaded for this date range and not due for a refresh: read
    // it back and never touch the network.
    const reusable = windows.find((window) => window.freshUntil > now && covers(window));
    if (!options.force && reusable) {
      const rows = mergeRows(memory.get(key)?.rows ?? [], await readCachedBars(exchange, symbol));
      // Trust the manifest, not the bars' timestamps: an exchange returns the
      // bar that STARTS at the end of a range too, and a window whose end is
      // one bar late is still the same series. Demanding a non-empty slice
      // re-downloaded everything whenever the last bar sat just outside.
      if (rows.length > 0) {
        memory.set(key, { exchange, symbol, rows });
        return windowOf(rows, startMs, endMs);
      }
    }

    // The request is inside a window we already downloaded, but that window is
    // due for a refresh: keep its bars, re-record it (and fetch only whatever
    // lies past its end). Re-downloading a day of bars for a page reload is the
    // behaviour this cache exists to avoid.
    const coveredWindow = windows.find(covers);
    // Otherwise extend the closest known window on one side instead of
    // downloading the whole request again — but only when the request actually
    // touches it. A window with a gap before it (the diary's next trade hours
    // later) is downloaded on its own, so the gap is never fetched.
    const head = windows
      .filter(
        (window) =>
          window.from > from + REFRESH_SLACK_MS &&
          window.to >= to - REFRESH_SLACK_MS &&
          window.from <= to + REFRESH_SLACK_MS,
      )
      .sort((a, b) => a.from - b.from)[0];
    const tail = windows
      .filter(
        (window) =>
          window.from <= from + REFRESH_SLACK_MS &&
          window.to < to - REFRESH_SLACK_MS &&
          window.to >= from - REFRESH_SLACK_MS,
      )
      .sort((a, b) => b.to - a.to)[0];
    const batch = coveredWindow
      ? ({ side: "tail", base: coveredWindow } as const)
      : head && (!tail || head.from - from < to - tail.to)
        ? ({ side: "head", base: head } as const)
        : tail
          ? ({ side: "tail", base: tail } as const)
          : null;
    if (!options.force && batch) {
      const { side, base } = batch;
      const missingFrom = side === "head" ? from : base.to + BAR_MS;
      const missingTo = side === "head" ? base.from - BAR_MS : to;
      let failed: unknown = null;
      let missing: CandleRow[] = [];
      if (missingFrom < missingTo) {
        try {
          missing = await downloadBars({
            exchange,
            symbol,
            startMs: missingFrom,
            endMs: missingTo,
            signal: options.signal,
          });
        } catch (error) {
          failed = error;
          if (options.signal?.aborted) throw error;
        }
      }
      const rows = mergeRows(memory.get(key)?.rows ?? (await readCachedBars(exchange, symbol)), missing);
      if (rows.length > 0) {
        if (failed) {
          // Refresh failed (offline, rate limit): serve what is on disk and
          // retry on the next load instead of failing the chart.
          rememberWindow(key, base);
        } else {
          rememberEnd(endMs);
          const record: CachedRange =
            side === "head"
              ? { from, to: base.to, freshUntil: base.freshUntil }
              : { from: base.from, to: to + PAD_MS, freshUntil: endMs + DAY_MS };
          rememberWindow(key, record);
          await writeCachedBars(exchange, symbol, rows, record);
        }
        memory.set(key, { exchange, symbol, rows });
        return windowOf(rows, startMs, endMs);
      }
      if (failed) throw failed;
    }

    downloadsBySeries.set(key, (downloadsBySeries.get(key) ?? 0) + 1);
    const previous = memory.get(key)?.rows ?? (await readCachedBars(exchange, symbol));
    const downloaded = await downloadBars(
      { exchange, symbol, startMs, endMs, signal: options.signal },
      options.onProgress,
    );
    let rows = mergeRows(previous, downloaded);
    if (rows.length > MEMORY_LIMIT) rows = rows.slice(rows.length - MEMORY_LIMIT);
    memory.set(key, { exchange, symbol, rows });
    const record: CachedRange = { from: from - PAD_MS, to: to + PAD_MS, freshUntil: endMs + DAY_MS };
    rememberEnd(endMs);
    if (shift > 0) rememberShift(Math.ceil(shift / BAR_MS) + 1);
    await recordWindow(key, exchange, symbol, record);
    await writeCachedBars(exchange, symbol, rows, record);
    return windowOf(rows, startMs, endMs);
  });

  if (!options.force) inflight.set(requestKey, task);
  try {
    return await task;
  } finally {
    inflight.delete(requestKey);
  }
}

/**
 * Bars already in memory for a series (no download), or null when this series
 * was never loaded in this page. `startMs = 0` means "from the first cached
 * bar", which is what a worker restart needs to rebuild the full history.
 */
export function peekBaseBars(
  exchange: ExchangeId,
  symbol: string,
  startMs: number,
  endMs: number,
): CandleRow[] | null {
  const entry = memory.get(keyOf(exchange, symbol));
  if (!entry) return null;
  const from = startMs > 0 ? Math.floor(startMs / BAR_MS) * BAR_MS : 0;
  if (!memoryCovers(entry, from, 0)) return null;
  return startMs > 0 ? slice(entry.rows, startMs, endMs) : entry.rows.slice();
}

/** Whether this series is already fully available without a download. */
export function hasSeriesInMemory(exchange: ExchangeId, symbol: string): boolean {
  return memory.has(keyOf(exchange, symbol));
}

/** Open interest for the chart's timeframe (empty when the venue has none). */
export async function loadOi(
  exchange: ExchangeId,
  symbol: string,
  timeframe: "5m" | "1h" | "1d",
  signal?: AbortSignal,
): Promise<OiSeries> {
  return downloadOi(exchange, symbol, timeframe, signal);
}

/**
 * Download every series in `symbols` for the given window, one symbol at a
 * time. Already-cached coins resolve from disk instantly, so this is cheap and
 * idempotent; the caller decides which coins still need it.
 */
export async function prefetchSeries(
  symbols: readonly string[],
  exchange: ExchangeId,
  startMs: number,
  endMs: number,
  hooks: {
    onStart?: (symbol: string) => void;
    onDone?: (symbol: string, failed: boolean) => void;
    shouldStop?: () => boolean;
  } = {},
): Promise<string[]> {
  const failed: string[] = [];
  for (const symbol of symbols) {
    if (hooks.shouldStop?.()) break;
    hooks.onStart?.(symbol);
    try {
      await loadBaseBars(exchange, symbol, startMs, endMs);
      hooks.onDone?.(symbol, false);
    } catch {
      failed.push(symbol);
      hooks.onDone?.(symbol, true);
    }
  }
  return failed;
}

/**
 * Forget a coin's bars — memory AND the browser cache.
 *
 * Called when the user removes a coin from the watchlist: an evicted coin must
 * not keep holding quota, and re-adding it later must download it fresh instead
 * of reading a stale series back.
 */
export async function evictSeries(exchange: ExchangeId, symbol: string): Promise<void> {
  const key = keyOf(exchange, symbol);
  memory.delete(key);
  covered.delete(key);
  // Wait for any download in flight for this series before deleting its file,
  // otherwise the write that finishes last recreates the cache entry.
  await serialize(key, () => clearCachedBars(exchange, symbol));
}

/** Test/dev helper: forget everything held in memory and on disk. */
export async function clearAllSeries(): Promise<void> {
  memory.clear();
  covered.clear();
  await clearCachedBars();
}

/**
 * What the cache holds for a series (a reload and a coin switch inspect this in
 * the browser; it is also what a "data" panel would show).
 */
export async function cacheState(
  exchange: ExchangeId,
  symbol: string,
): Promise<{ known: CachedRange | null; bars: number; fresh: boolean }> {
  await ensureWindowsLoaded();
  const ranges = covered.get(keyOf(exchange, symbol)) ?? [];
  const known = ranges.reduce<CachedRange | null>(
    (best, range) => (!best || range.freshUntil > best.freshUntil ? range : best),
    null,
  );
  const entry = memory.get(keyOf(exchange, symbol));
  return {
    known,
    bars: entry?.rows.length ?? 0,
    fresh: !!known && known.freshUntil > Date.now(),
  };
}

/** Which interval this provider downloads (everything else is aggregated). */
export { BASE_TIMEFRAME };
