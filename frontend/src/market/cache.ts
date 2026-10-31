// Browser-side candle cache: OPFS file per symbol + IndexedDB manifest.
//
// Why OPFS: a replay window is often hundreds of thousands of 5m bars, which is
// far past what localStorage (5 MB strings) or a plain object in IndexedDB can
// hold efficiently. Origin Private File System stores the bars as a compact
// binary blob; IndexedDB keeps only a tiny manifest of WHICH ranges a file
// covers, so the provider can answer "is this window already on disk?" without
// reading the bars back.
//
// Everything here is per-origin and private to this browser profile: the data
// never leaves the user's machine, and nothing is uploaded anywhere.

import type { CandleRow } from "../types/protocol.ts";

const DB_NAME = "simccxt.market.v2";
// v2: the manifest stores requested windows + their refresh expiry (v1 kept
// bare covered intervals, which could never expire or be matched to a window).
const DB_VERSION = 1;
const STORE = "ranges";

/** A cached window: first/last bar requested, and when to refresh it again. */
interface Range {
  from: number;
  to: number;
  freshUntil: number;
}

/** Manifest entry of one cached series file. */
interface RangeRecord {
  key: string; // `${exchange}|${symbol}`
  ranges: Range[];
}

interface IndexedDb {
  db: IDBDatabase;
}

function openDb(): Promise<IndexedDb | null> {
  return new Promise((resolve) => {
    if (typeof indexedDB === "undefined") {
      resolve(null);
      return;
    }
    const request = indexedDB.open(DB_NAME, DB_VERSION);
    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(STORE)) db.createObjectStore(STORE, { keyPath: "key" });
    };
    request.onsuccess = () => resolve({ db: request.result });
    request.onerror = () => resolve(null);
    request.onblocked = () => resolve(null);
  });
}

let dbPromise: Promise<IndexedDb | null> | null = null;

function db(): Promise<IndexedDb | null> {
  dbPromise ??= openDb();
  return dbPromise;
}

function seriesKey(exchange: string, symbol: string): string {
  return `${exchange}|${symbol}`;
}

function seriesFile(exchange: string, symbol: string): string {
  return `${exchange}_${symbol.replace("/", "")}_5m.bin`;
}

/** 6 doubles per bar: ts, open, high, low, close, volume. */
const BAR_BYTES = 48;

function encode(rows: readonly CandleRow[]): ArrayBuffer {
  const buffer = new ArrayBuffer(rows.length * BAR_BYTES);
  const view = new DataView(buffer);
  rows.forEach((row, index) => {
    const offset = index * BAR_BYTES;
    for (let field = 0; field < 6; field += 1) {
      view.setFloat64(offset + field * 8, row[field], true);
    }
  });
  return buffer;
}

function decode(buffer: ArrayBuffer): CandleRow[] {
  const view = new DataView(buffer);
  const count = Math.floor(buffer.byteLength / BAR_BYTES);
  const rows: CandleRow[] = [];
  for (let index = 0; index < count; index += 1) {
    const offset = index * BAR_BYTES;
    rows.push([
      view.getFloat64(offset, true),
      view.getFloat64(offset + 8, true),
      view.getFloat64(offset + 16, true),
      view.getFloat64(offset + 24, true),
      view.getFloat64(offset + 32, true),
      view.getFloat64(offset + 40, true),
    ]);
  }
  return rows;
}

let directoryPromise: Promise<FileSystemDirectoryHandle | null> | null = null;

/**
 * The OPFS root, or null where the browser has no OPFS (Firefox today). The
 * cache then degrades to memory-only for the page load — the provider still
 * works, it just re-downloads on the next visit.
 */
function directoryHandle(): Promise<FileSystemDirectoryHandle | null> {
  directoryPromise ??= (async () => {
    const storage = navigator.storage as StorageManager & {
      getDirectory?: () => Promise<FileSystemDirectoryHandle>;
    };
    if (!storage || typeof storage.getDirectory !== "function") return null;
    try {
      return await storage.getDirectory();
    } catch {
      return null;
    }
  })();
  return directoryPromise;
}

async function readFile(name: string): Promise<ArrayBuffer | null> {
  const directory = await directoryHandle();
  if (!directory) return null;
  try {
    const handle = await directory.getFileHandle(name);
    return await (await handle.getFile()).arrayBuffer();
  } catch {
    return null;
  }
}

async function writeFile(name: string, buffer: ArrayBuffer): Promise<void> {
  const directory = await directoryHandle();
  if (!directory) return;
  try {
    const handle = await directory.getFileHandle(name, { create: true });
    const writable = await handle.createWritable();
    await writable.write(buffer);
    await writable.close();
  } catch {
    // Quota or private mode: the bars stay in memory for this page load.
  }
}

async function readRanges(key: string): Promise<Range[]> {
  const store = await db();
  if (!store) return [];
  return await new Promise((resolve) => {
    const request = store.db.transaction(STORE, "readonly").objectStore(STORE).get(key);
    request.onsuccess = () => {
      const record = request.result as RangeRecord | undefined;
      resolve(record?.ranges ?? []);
    };
    request.onerror = () => resolve([]);
  });
}

async function writeRanges(key: string, ranges: Range[]): Promise<void> {
  const store = await db();
  if (!store) return;
  await new Promise<void>((resolve) => {
    const transaction = store.db.transaction(STORE, "readwrite");
    transaction.objectStore(STORE).put({ key, ranges } satisfies RangeRecord);
    transaction.oncomplete = () => resolve();
    transaction.onerror = () => resolve();
    transaction.onabort = () => resolve();
  });
}

/**
 * Merge the newly covered window into the manifest, keeping one entry per
 * requested window (the newest expiry wins for identical windows).
 */
function mergeRanges(ranges: Range[], added: Range): Range[] {
  const byWindow = new Map<string, Range>();
  for (const range of [...ranges, added]) {
    const id = `${range.from}|${range.to}`;
    const existing = byWindow.get(id);
    byWindow.set(id, existing && existing.freshUntil > range.freshUntil ? existing : range);
  }
  return [...byWindow.values()].sort((a, b) => a.from - b.from);
}

/**
 * Bars of one series cached on disk, or [] when nothing is cached.
 *
 * A corrupt or truncated file must degrade to "not cached" (the provider then
 * downloads), never to an exception: the user would otherwise see a dead chart.
 */
export async function readCachedBars(exchange: string, symbol: string): Promise<CandleRow[]> {
  try {
    const buffer = await readFile(seriesFile(exchange, symbol));
    if (!buffer || buffer.byteLength < BAR_BYTES) return [];
    return decode(buffer);
  } catch {
    return [];
  }
}

/**
 * Store the full bar set of one series and record the window it was requested
 * for, plus when that window should be refreshed.
 */
export async function writeCachedBars(
  exchange: string,
  symbol: string,
  rows: readonly CandleRow[] | null,
  window: Range,
): Promise<void> {
  const key = seriesKey(exchange, symbol);
  // `rows === null`: the bars are already on disk, only record the window.
  if (rows !== null) {
    if (rows.length === 0) return;
    await writeFile(seriesFile(exchange, symbol), encode(rows));
  }
  const ranges = await readRanges(key);
  await writeRanges(key, mergeRanges(ranges, window));
}

/** Whether [startMs, endMs] is fully covered by cached ranges. */
export async function isRangeCached(
  exchange: string,
  symbol: string,
  startMs: number,
  endMs: number,
): Promise<boolean> {
  const ranges = await readRanges(seriesKey(exchange, symbol));
  return ranges.some((range) => range.from <= startMs && range.to >= endMs);
}

/**
 * The requested windows of ALL series, read in one IndexedDB transaction.
 *
 * One read per series used to mean four separate transactions at page start,
 * and every one of them could land before the previous had committed — so a
 * reload saw an empty manifest and downloaded everything again.
 */
export async function loadAllWindows(): Promise<Map<string, Range[]>> {
  const out = new Map<string, Range[]>();
  const adapter = await db();
  if (!adapter) return out;
  await new Promise<void>((resolve) => {
    const request = adapter.db.transaction(STORE, "readonly").objectStore(STORE).getAll();
    request.onsuccess = () => {
      for (const record of request.result as RangeRecord[]) out.set(record.key, record.ranges ?? []);
      resolve();
    };
    request.onerror = () => resolve();
  });
  return out;
}

/** Manifest entries of one series (test/dev helper). */
export async function cachedWindows(
  exchange: string,
  symbol: string,
): Promise<Range[]> {
  return readRanges(seriesKey(exchange, symbol));
}

/**
 * Delete the cached bars of ONE series (a coin removed from the watchlist).
 * Other coins keep their files: clearing the whole directory would make every
 * remaining coin download again.
 */
export async function clearCachedBars(exchange?: string, symbol?: string): Promise<void> {
  const directory = await directoryHandle();
  if (directory && exchange !== undefined && symbol !== undefined) {
    try {
      await directory.removeEntry(seriesFile(exchange, symbol));
    } catch {
      // nothing cached for it: fine
    }
    const store = await db();
    if (store) {
      await new Promise<void>((resolve) => {
        const transaction = store.db.transaction(STORE, "readwrite");
        transaction.objectStore(STORE).delete(seriesKey(exchange, symbol));
        transaction.oncomplete = () => resolve();
        transaction.onerror = () => resolve();
      });
    }
    return;
  }
  // No series given: drop everything (test/dev reset only).
  if (directory) {
    try {
      // `for await` over a directory handle (lib.dom.asynciterable) is not in
      // this project's `lib` list, so iterate through the AsyncIterator shape.
      const iterator = (directory as unknown as AsyncIterable<[string, FileSystemHandle]>)[
        Symbol.asyncIterator
      ]();
      for (;;) {
        const next = await iterator.next();
        if (next.done) break;
        if (next.value[0].endsWith(".bin")) await directory.removeEntry(next.value[0]);
      }
    } catch {
      // ignore: nothing cached
    }
  }
  const store = await db();
  if (store) {
    await new Promise<void>((resolve) => {
      const transaction = store.db.transaction(STORE, "readwrite");
      transaction.objectStore(STORE).clear();
      transaction.oncomplete = () => resolve();
      transaction.onerror = () => resolve();
    });
  }
}
