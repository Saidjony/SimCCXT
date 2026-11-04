// The market-data fallback: when the browser cannot reach a venue (region
// block, corporate proxy, CORS), the page asks its own server for the same
// klines page. No DOM, no network — `fetch` is stubbed.
import { afterEach, beforeEach, describe, it } from "node:test";
import assert from "node:assert/strict";
import { downloadBars, downloadTicker, proxyKlineUrl, proxyTickerUrl, RateLimitedError } from "../src/market/exchanges.ts";
import { apiUrl, wsUrl } from "../src/websocket/origins.ts";

const ORIGIN = "http://127.0.0.1:19010";
const realFetch = globalThis.fetch;
const calls: string[] = [];

function binancePage(rows: number): string {
  return JSON.stringify(
    Array.from({ length: rows }, (_, i) => [
      1_700_000_000_000 + i * 300_000,
      "100.0",
      "101.0",
      "99.0",
      "100.5",
      "10.0",
    ]),
  );
}

beforeEach(() => {
  calls.length = 0;
  // `apiUrl()` reads the origin the server injected into index.html.
  (globalThis as unknown as { window: unknown }).window = {
    __SIMCCXT_ORIGIN__: ORIGIN,
    location: { hostname: "127.0.0.1", protocol: "http:", host: "127.0.0.1:19010" },
  };
});

afterEach(() => {
  globalThis.fetch = realFetch;
  delete (globalThis as unknown as { window?: unknown }).window;
});

describe("market fallback proxy", () => {
  it("builds the same request on the server's own origin", () => {
    const url = proxyKlineUrl("http://127.0.0.1:19010/", {
      exchange: "binance",
      symbol: "BTC/USDT",
      startMs: 1000,
      endMs: 2000,
      limit: 1000,
    });
    assert.equal(
      url,
      "http://127.0.0.1:19010/api/market/klines?exchange=binance&symbol=BTC%2FUSDT&start_ms=1000&end_ms=2000&limit=1000",
    );
  });

  it("retries through the server when the venue is unreachable", async () => {
    globalThis.fetch = (async (input: string | URL) => {
      const url = String(input);
      calls.push(url);
      if (url.startsWith("https://api.binance.com/")) {
        throw new TypeError("Failed to fetch");
      }
      assert.ok(url.startsWith(`${ORIGIN}/api/market/klines?`));
      return new Response(binancePage(1), { status: 200 });
    }) as typeof fetch;

    const rows = await downloadBars({
      exchange: "binance",
      symbol: "BTC/USDT",
      startMs: 1_700_000_000_000,
      endMs: 1_700_000_300_000,
    });

    assert.equal(rows.length, 1);
    assert.equal(rows[0][4], 100.5);
    assert.equal(calls.length, 2);
    assert.ok(calls[0].startsWith("https://api.binance.com/"));
    assert.ok(calls[1].startsWith(`${ORIGIN}/api/market/klines?`));
  });

  it("never reroutes a rate limit: that is not an outage", async () => {
    globalThis.fetch = (async (input: string | URL) => {
      calls.push(String(input));
      return new Response("{}", { status: 429 });
    }) as typeof fetch;

    await assert.rejects(
      downloadBars({
        exchange: "binance",
        symbol: "BTC/USDT",
        startMs: 1_700_000_000_000,
        endMs: 1_700_000_300_000,
      }),
      (error: unknown) => error instanceof RateLimitedError,
    );
    assert.equal(calls.length, 1);
  });

  it("reports both failures when neither path works", async () => {    globalThis.fetch = (async (input: string | URL) => {
      const url = String(input);
      calls.push(url);
      if (url.startsWith("https://")) throw new TypeError("Failed to fetch");
      return new Response(JSON.stringify({ error: "venue request failed: blocked" }), {
        status: 502,
      });
    }) as typeof fetch;

    await assert.rejects(
      downloadBars({
        exchange: "binance",
        symbol: "BTC/USDT",
        startMs: 1_700_000_000_000,
        endMs: 1_700_000_300_000,
      }),
      (error: unknown) => {
        const message = error instanceof Error ? error.message : String(error);
        assert.match(message, /нет связи с api\.binance\.com/);
        assert.match(message, /через сервер тоже не вышло/);
        return true;
      },
    );
  });

  it("fetches a watchlist quote through the server too", async () => {
    assert.equal(
      proxyTickerUrl(ORIGIN, { exchange: "bybit", symbol: "ETH/USDT" }),
      `${ORIGIN}/api/market/ticker?exchange=bybit&symbol=ETH%2FUSDT`,
    );

    globalThis.fetch = (async (input: string | URL) => {
      const url = String(input);
      calls.push(url);
      if (url.startsWith("https://api.binance.com/")) throw new TypeError("Failed to fetch");
      return new Response(JSON.stringify({ price: "123.45" }), { status: 200 });
    }) as typeof fetch;

    assert.equal(await downloadTicker("binance", "BTC/USDT"), 123.45);
    assert.equal(calls.length, 2);
    assert.ok(calls[1].startsWith(`${ORIGIN}/api/market/ticker?`));
  });
});

describe("api origin behind a proxy", () => {
  const w = globalThis as unknown as { window?: unknown };

  it("keeps the API on the page's own host when the socket shares it", () => {
    // `ssh -R 5173:localhost:19010`: the page and the socket are both on
    // :5173, so `/api/*` is on :5173 as well — NOT :5174.
    w.window = {
      location: {
        hostname: "130.49.178.95",
        host: "130.49.178.95:5173",
        origin: "http://130.49.178.95:5173",
        protocol: "http:",
      },
    };
    assert.equal(wsUrl(), "ws://130.49.178.95:5173");
    assert.equal(apiUrl(), "http://130.49.178.95:5173");
  });

  it("still points the API at ws_port + 1 in local vite dev", () => {
    w.window = {
      location: {
        hostname: "localhost",
        host: "localhost:5173",
        origin: "http://localhost:5173",
        protocol: "http:",
      },
    };
    assert.equal(wsUrl(), "ws://127.0.0.1:9001");
    assert.equal(apiUrl(), "http://127.0.0.1:9002");
  });
});
