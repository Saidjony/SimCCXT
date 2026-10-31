// Provider cache rules, tested without a browser.
//
// OPFS, IndexedDB and `fetch` are replaced with in-memory fakes, so these tests
// pin down exactly what the user sees: a window is downloaded once, a reload or
// a coin switch costs nothing, a stale window refreshes only its tail, and a
// removed coin loses its cache.
import { describe, it, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";

const BAR = 300_000;
const DAY = 86_400_000;

/** In-memory stand-in state, shared with the fakes that live on `globalThis`. */
interface Harness {
  files: Map<string, ArrayBuffer>;
  manifest: Map<string, Array<{ from: number; to: number; freshUntil: number }>>;
  fetches: Array<{ symbol: string; start: number; end: number }>;
}
const harness: Harness = { files: new Map(), manifest: new Map(), fetches: [] };

function installBrowserFakes(): void {
  const g = globalThis as unknown as Record<string, unknown>;
  // --- OPFS ---
  const navigatorStub = {
    storage: {
      async getDirectory() {
        return {
          async getFileHandle(name: string, options?: { create?: boolean }) {
            if (!harness.files.has(name) && !options?.create) throw new Error("NotFound");
            return {
              async getFile() {
                const buffer = harness.files.get(name);
                if (!buffer) throw new Error("NotFound");
                return { arrayBuffer: async () => buffer };
              },
              async createWritable() {
                let pending: ArrayBuffer = new ArrayBuffer(0);
                return {
                  async write(buffer: ArrayBuffer) {
                    pending = buffer;
                  },
                  async close() {
                    harness.files.set(name, pending);
                  },
                };
              },
            };
          },
          async removeEntry(name: string) {
            if (!harness.files.delete(name)) throw new Error("NotFound");
          },
          [Symbol.asyncIterator]: () => {
            const names = [...harness.files.keys()];
            let index = 0;
            return {
              async next() {
                return index < names.length
                  ? { done: false as const, value: [names[index++], {}] }
                  : { done: true as const, value: undefined };
              },
            };
          },
        };
      },
    },
  };

  // `navigator` already exists in Node; it is defined as a getter, so replace it.
  Object.defineProperty(globalThis, "navigator", {
    value: navigatorStub,
    configurable: true,
    writable: true,
  });

  // --- IndexedDB (only the tiny surface cache.ts uses) ---
  const openRequests: Array<{ onsuccess?: () => void }> = [];
  class FakeDb {
    objectStoreNames = { contains: () => true };
    transaction(_store: string, mode: string) {
      const keyed = {
        get(key: string) {
          const request: Record<string, unknown> = {};
          queueMicrotask(() => {
            if (mode === "readonly") {
              request["result"] = harness.manifest.has(key)
                ? { key, ranges: harness.manifest.get(key) }
                : undefined;
              (request["onsuccess"] as () => void)?.();
            } else {
              (request["oncomplete"] as () => void)?.();
            }
          });
          return request;
        },
        put(record: { key: string; ranges: unknown }) {
          harness.manifest.set(record.key, record.ranges as never);
        },
        delete(key: string) {
          harness.manifest.delete(key);
        },
        clear() {
          harness.manifest.clear();
        },
        getAll() {
          const request: Record<string, unknown> = {};
          queueMicrotask(() => {
            request["result"] = [...harness.manifest.entries()].map(([key, ranges]) => ({ key, ranges }));
            (request["onsuccess"] as () => void)?.();
          });
          return request;
        },
      };
      const transaction: Record<string, unknown> = {
        objectStore: () => keyed,
        oncomplete: null as null | (() => void),
        onerror: null,
        onabort: null,
      };
      queueMicrotask(() => (transaction["oncomplete"] as (() => void) | null)?.());
      return transaction;
    }
  }
  g.indexedDB = {
    open() {
      const request: Record<string, unknown> = { result: new FakeDb() };
      openRequests.push(request as never);
      queueMicrotask(() => (request["onsuccess"] as () => void)?.());
      return request;
    },
  };

  // --- venue ---
  g.fetch = async (url: string) => {
    const query = new URL(url).searchParams;
    const symbol = query.get("symbol") ?? "";
    const start = Number(query.get("startTime"));
    const end = Number(query.get("endTime"));
    harness.fetches.push({ symbol, start, end });
    const rows: unknown[] = [];
    let ts = Math.ceil(start / BAR) * BAR;
    // At most one page (1000 bars), like the real venue.
    while (ts <= end && rows.length < 1000) {
      rows.push([ts, 100, 101, 99, 100.5, 1]);
      ts += BAR;
    }
    return { ok: true, json: async () => rows };
  };
}

let provider: typeof import("../src/market/provider.ts");

beforeEach(async () => {
  harness.files.clear();
  harness.manifest.clear();
  harness.fetches.length = 0;
  installBrowserFakes();
  provider = await import("../src/market/provider.ts");
  await provider.clearAllSeries();
  harness.fetches.length = 0;
});

afterEach(() => {
  const g = globalThis as unknown as Record<string, unknown>;
  delete g.navigator;
  delete g.indexedDB;
  delete g.fetch;
});

const WINDOW = { start: 20_000 * BAR, end: 21_700 * BAR };

describe("provider: download once", () => {
  it("downloads a window once and serves repeats from memory", async () => {
    await provider.loadBaseBars("binance", "BTC/USDT", WINDOW.start, WINDOW.end);
    const first = harness.fetches.length;
    assert.ok(first > 0, "the first load must hit the venue");

    const again = await provider.loadBaseBars("binance", "BTC/USDT", WINDOW.start, WINDOW.end);
    assert.equal(harness.fetches.length, first, "a repeat must not fetch again");
    assert.ok(again.length > 0);
  });

  it("keeps a downloaded window across a simulated page reload (manifest only)", async () => {
    await provider.loadBaseBars("binance", "BTC/USDT", WINDOW.start, WINDOW.end);
    const first = harness.fetches.length;
    // A reload throws away all module state; only OPFS + IndexedDB survive.
    const fresh = await import(`../src/market/provider.ts?reload=${Date.now()}`);
    const rows = await fresh.loadBaseBars("binance", "BTC/USDT", WINDOW.start + BAR, WINDOW.end);
    assert.equal(harness.fetches.length, first, "a reload must not re-download");
    assert.ok(rows.length > 0);
  });

  it("refreshes only the tail once the window's day is over", async () => {
    await provider.loadBaseBars("binance", "BTC/USDT", WINDOW.start, WINDOW.end);
    const first = harness.fetches.length;
    // Pretend the saved window expired yesterday.
    const key = "binance|BTC/USDT";
    const windows = harness.manifest.get(key)!;
    harness.manifest.set(key, windows.map((w) => ({ ...w, freshUntil: Date.now() - DAY })));
    const fresh = await import(`../src/market/provider.ts?expired=${Date.now()}`);
    const later = WINDOW.end + 12 * BAR; // a handful of new bars
    await fresh.loadBaseBars("binance", "BTC/USDT", WINDOW.start, later);
    const extra = harness.fetches.length - first;
    assert.ok(extra >= 1, "the stale window is refreshed once");
    assert.ok(extra <= 2, `only the tail is fetched, got ${extra} requests`);
    assert.equal(harness.fetches[harness.fetches.length - 1].start >= WINDOW.end, true);
  });
});

describe("provider: eviction", () => {
  it("removing a coin deletes only its cache and leaves the others intact", async () => {
    await provider.loadBaseBars("binance", "BTC/USDT", WINDOW.start, WINDOW.end);
    await provider.loadBaseBars("binance", "ETH/USDT", WINDOW.start, WINDOW.end);
    assert.deepEqual([...harness.files.keys()].sort(), [
      "binance_BTCUSDT_5m.bin",
      "binance_ETHUSDT_5m.bin",
    ]);

    await provider.evictSeries("binance", "ETH/USDT");
    assert.deepEqual([...harness.files.keys()], ["binance_BTCUSDT_5m.bin"]);
    assert.equal(harness.manifest.has("binance|ETH/USDT"), false);

    // The surviving coin still costs nothing...
    const before = harness.fetches.length;
    await provider.loadBaseBars("binance", "BTC/USDT", WINDOW.start, WINDOW.end);
    assert.equal(harness.fetches.length, before, "an untouched coin must not be re-downloaded");

    // ...and the removed one downloads again when it comes back.
    const fresh = await import(`../src/market/provider.ts?readd=${Date.now()}`);
    await fresh.loadBaseBars("binance", "ETH/USDT", WINDOW.start, WINDOW.end);
    assert.ok(harness.fetches.length > before, "a re-added coin is downloaded fresh");
  });
});
