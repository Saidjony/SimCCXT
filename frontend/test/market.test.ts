// Pure market-data tests: local aggregation, venue URLs, timeframe aliases.
// No DOM, no network, no OPFS — everything here is a pure function.
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  aggregateCandles,
  aggregateOi,
  barsFor,
  visibleBase,
} from "../src/market/aggregate.ts";
import { oiUrl } from "../src/market/exchanges.ts";
import {
  BASE_TIMEFRAME,
  TIMEFRAMES,
  TIMEFRAME_MS,
  factorOf,
  isTimeframe,
  timeframeOr,
} from "../src/market/timeframes.ts";
import type { CandleRow } from "../src/types/protocol.ts";

const BASE = TIMEFRAME_MS["5m"];

/** `count` 5m bars starting at `firstMs`, close = index so buckets are checkable. */
function bars(firstMs: number, count: number): CandleRow[] {
  const out: CandleRow[] = [];
  for (let i = 0; i < count; i += 1) {
    const close = 100 + i;
    out.push([firstMs + i * BASE, close - 1, close + 2, close - 3, close, 10]);
  }
  return out;
}

describe("timeframes", () => {
  it("exposes exactly 5m, 1h and 1d, with 5m as the only downloaded base", () => {
    assert.deepEqual([...TIMEFRAMES], ["5m", "1h", "1d"]);
    assert.equal(BASE_TIMEFRAME, "5m");
    assert.equal(factorOf("5m"), 1);
    assert.equal(factorOf("1h"), 12);
    assert.equal(factorOf("1d"), 288);
  });

  it("drops the removed 1m/15m/4h aliases instead of passing them through", () => {
    for (const gone of ["1m", "15m", "4h", "1s", "", null, 5]) {
      assert.equal(isTimeframe(gone), false, `${String(gone)} must not be a timeframe`);
    }
    assert.equal(timeframeOr("1m", "5m"), "5m");
    assert.equal(timeframeOr("4h", "1d"), "1d");
    assert.equal(timeframeOr("1h", "5m"), "1h");
    assert.equal(timeframeOr(undefined, "5m"), "5m");
  });
});

describe("aggregateCandles", () => {
  it("keeps 5m bars as-is (a copy, not the same array)", () => {
    const rows = bars(0, 3);
    const out = aggregateCandles(rows, "5m");
    assert.deepEqual(out, rows);
    assert.notEqual(out, rows);
  });

  it("builds a 1h bar from twelve 5m bars", () => {
    const out = aggregateCandles(bars(0, 12), "1h");
    assert.equal(out.length, 1);
    const [ts, open, high, low, close, volume] = out[0];
    assert.equal(ts, 0);
    assert.equal(open, 99, "open comes from the first bar");
    assert.equal(close, 111, "close comes from the last bar");
    assert.equal(high, 113);
    assert.equal(low, 97, "low is the minimum 5m low: bar 0 low = 99 - 3");
    assert.equal(volume, 120);
  });

  it("aligns buckets to absolute step boundaries, not to the first row", () => {
    // Start halfway through an hour: the first bucket is partial. 5m bars at
    // :30, :35, :40, :45, :50, :55 land in hour 0; :00 and later in hour 1.
    const out = aggregateCandles(bars(30 * 60_000, 12), "1h");
    assert.deepEqual(out.map((row) => row[0]), [0, 3_600_000]);
    assert.equal(out[0][5], 60, "6 bars x volume 10");
    assert.equal(out[1][5], 60, "the other 6 bars");
    // The partial bucket keeps its real open, never a padded one.
    assert.equal(out[0][1], 99);
  });

  it("builds a 1d bar from 288 5m bars and keeps UTC day boundaries", () => {
    const out = aggregateCandles(bars(0, 288 * 2), "1d");
    assert.deepEqual(out.map((row) => row[0]), [0, 86_400_000]);
    assert.equal(out[1][5], 2880);
  });

  it("returns [] for empty input — never the source rows", () => {
    // Regression: folding an empty array used to return the input, which made a
    // 1d chart sell 5m bars as if each covered 288 base bars.
    assert.deepEqual(aggregateCandles([], "1d"), []);
    assert.deepEqual(aggregateCandles([], "5m"), []);
  });

  it("skips empty buckets: a data gap stays a gap", () => {
    const rows = [...bars(0, 12), ...bars(3 * 3_600_000, 12)];
    const out = aggregateCandles(rows, "1h");
    assert.deepEqual(out.map((row) => row[0]), [0, 3 * 3_600_000]);
  });

  it("is idempotent in output shape: aggregated 1h -> 1d equals direct 1d", () => {
    const rows = bars(0, 288);
    const direct = aggregateCandles(rows, "1d");
    // Re-aggregating a day from only the FIRST hour must not invent a full day,
    // but aggregating the whole day's 1h bars must equal the direct 1d bar.
    const viaHour = aggregateCandles(aggregateCandles(rows, "1h"), "1d");
    assert.deepEqual(viaHour, direct);
  });
});

describe("barsFor / visibleBase", () => {
  it("derives both bar counts from the same revealed count", () => {
    const rows = bars(0, 288 * 3);
    assert.equal(barsFor(rows, "5m", 0).length, 0);
    assert.equal(barsFor(rows, "1h", 13).length, 2);
    assert.deepEqual(
      barsFor(rows, "1d", 288).map((row) => row[0]),
      barsFor(rows, "1d", rows.length).map((row) => row[0]).slice(0, 1),
    );
  });

  it("clamps fed beyond the series length", () => {
    const rows = bars(0, 12);
    assert.equal(visibleBase(rows, 999).length, 12);
    assert.equal(barsFor(rows, "1h", 999).length, 1);
  });
});

describe("aggregateOi", () => {
  it("takes the last sample of each bucket", () => {
    const samples: Array<[number, number]> = [
      [0, 1],
      [5 * 60_000, 2],
      [3_600_000, 3],
      [3_605_000, 4],
    ];
    assert.deepEqual(aggregateOi(samples, "1h"), [
      [0, 2],
      [3_600_000, 4],
    ]);
  });

  it("never fills an empty bucket (a 5m sample must not become a whole day)", () => {
    const samples: Array<[number, number]> = [[0, 7], [4 * 86_400_000, 9]];
    const out = aggregateOi(samples, "1d");
    assert.deepEqual(out, [
      [0, 7],
      [4 * 86_400_000, 9],
    ]);
  });

  it("keeps fine samples untouched below the target grid and handles empty input", () => {
    const samples: Array<[number, number]> = [[0, 1], [300_000, 2]];
    assert.deepEqual(aggregateOi(samples, "1d"), [[0, 2]]);
    assert.deepEqual(aggregateOi([], "1h"), []);
  });
});

describe("oiUrl", () => {
  it("maps the unified symbol to each venue's swap endpoint", () => {
    assert.match(oiUrl("binance", "BTC/USDT", "5m", 500), /fapi\.binance\.com\/futures\/data\/openInterestHist\?symbol=BTCUSDT&period=5m/);
    assert.match(oiUrl("bybit", "ETH/USDT", "1h", 100), /bybit\.com\/v5\/market\/open-interest\?category=linear&symbol=ETHUSDT&intervalTime=1h/);
    assert.match(oiUrl("okx", "SOL/USDT", "1d", 100), /okx\.com\/api\/v5\/rubik\/stat\/contracts\/open-interest-volume\?ccy=SOL&period=1D/);
  });
});
