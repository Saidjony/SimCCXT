// Pure overlay-math tests: no DOM, no network, no dependencies.
// Run with `bun test` or `npm test` (node --test).
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  barWord,
  calcQty,
  countBars,
  alignOiToCandles,
  cumulativeDelta,
  formatRuler,
  invertTime,
  nearestCandleTime,
  positionBands,
  positionDirection,
  profileBarLayout,
  riskReward,
  rulerStats,
  snapTime,
  toBox,
  volumeProfile,
} from "../src/chart/overlays.ts";

describe("toBox", () => {
  it("normalizes any click order", () => {
    assert.deepEqual(toBox(10, 20, 30, 5), { left: 10, top: 5, width: 20, height: 15 });
    assert.deepEqual(toBox(30, 5, 10, 20), { left: 10, top: 5, width: 20, height: 15 });
  });
});

describe("position tool", () => {
  it("detects direction by stop side", () => {
    assert.equal(positionDirection(100, 90), "long");
    assert.equal(positionDirection(100, 110), "short");
  });

  it("computes reward/risk", () => {
    assert.equal(riskReward(100, 90, 120), 2);
    assert.equal(riskReward(100, 100, 120), null);
  });

  it("splits long bands tp/entry/sl from top", () => {
    const bands = positionBands(120, 90, 100, 90, 120);
    const tp = bands.find((b) => b.cls === "tp");
    const sl = bands.find((b) => b.cls === "sl");
    assert.ok(tp && tp.from < tp.to);
    assert.ok(sl && sl.from < sl.to);
    // tp band sits above entry for a long
    assert.ok(tp.to <= 2 / 3 + 1e-9);
  });

  it("splits short bands mirrored", () => {
    const bands = positionBands(110, 80, 100, 110, 80);
    const tp = bands.find((b) => b.cls === "tp");
    assert.ok(tp && tp.from > 1 / 3 - 1e-9);
  });
});

describe("ruler", () => {
  it("computes pct/bars/points", () => {
    const s = rulerStats({ t: 0, p: 100 }, { t: 120000, p: 105 }, 60000);
    assert.equal(s.bars, 2);
    assert.equal(s.pct, 5);
    assert.equal(s.pts, 5);
    assert.equal(formatRuler(s), "+5.00% · 2 bars · +5.0 pts");
  });

  it("counts bars covered by a rectangle", () => {
    assert.equal(countBars([1, 2, 3, 4, 5], 2, 4), 3);
    assert.equal(countBars([1, 2, 3, 4, 5], 4, 2), 3);
    assert.equal(countBars([1, 2, 3], 10, 20), 1);
  });

  it("snaps time to the nearest bar", () => {
    assert.equal(snapTime([100, 200, 300], 240), 200);
    assert.equal(snapTime([], 240), 240);
  });
});

describe("invertTime", () => {
  const times = [1000, 2000, 3000, 4000];
  const toCoord = (ms: number): number | null => (ms - 1000) / 10; // 0,100,200,300

  it("inverts pixels to the nearest bar time", () => {
    assert.equal(invertTime(105, times, toCoord), 2000);
    assert.equal(invertTime(0, times, toCoord), 1000);
    assert.equal(invertTime(300, times, toCoord), 4000);
  });

  it("clamps outside the data range", () => {
    assert.equal(invertTime(-50, times, toCoord), 1000);
    assert.equal(invertTime(9999, times, toCoord), 4000);
  });

  it("returns null without data", () => {
    assert.equal(invertTime(50, [], toCoord), null);
  });
});

describe("calcQty", () => {
  it("sizes by fixed USDT risk", () => {
    assert.equal(calcQty(10, 100, 90), 1);
    assert.equal(calcQty(10, 100, 110), 1);
  });

  it("blocks degenerate input", () => {
    assert.equal(calcQty(10, 100, 100), null);
    assert.equal(calcQty(0, 100, 90), null);
    assert.equal(calcQty(10, Number.NaN, 90), null);
  });

  it("pluralizes bars in Russian", () => {
    assert.equal(barWord(1), "бар");
    assert.equal(barWord(3), "бара");
    assert.equal(barWord(5), "баров");
  });
});

describe("nearestCandleTime", () => {
  it("projects drawing times onto each timeframe's nearest bar", () => {
    const hourly = [0, 3_600_000, 7_200_000];
    assert.equal(nearestCandleTime(hourly, 3_600_000), 3_600_000);
    assert.equal(nearestCandleTime(hourly, 5_100_000), 3_600_000);
    assert.equal(nearestCandleTime(hourly, 5_500_000), 7_200_000);
    assert.equal(nearestCandleTime(hourly, -1), 0);
    assert.equal(nearestCandleTime([], 100), null);
  });
});

describe("cumulativeDelta", () => {
  it("adds volume on up bars, subtracts on down bars", () => {
    // [ts, o, h, l, c, v]
    const rows = [
      [1, 10, 11, 9, 11, 5],
      [2, 11, 12, 10, 10, 3],
      [3, 10, 10, 10, 10, 7],
    ] as Array<[number, number, number, number, number, number]>;
    assert.deepEqual(cumulativeDelta(rows), [5, 2, 9]);
  });

  it("is empty without bars", () => {
    assert.deepEqual(cumulativeDelta([]), []);
  });
});

describe("alignOiToCandles", () => {
  const candles = [
    [1000, 10, 11, 9, 10, 1],
    [2000, 10, 11, 9, 11, 1],
    [3000, 11, 12, 10, 11, 1],
    [4000, 11, 12, 10, 12, 1],
  ] as Array<[number, number, number, number, number, number]>;

  it("projects coarse OI samples onto candle timestamps without looking ahead", () => {
    assert.deepEqual(
      alignOiToCandles(candles, [[1500, 20], [3500, 30]]),
      [[2000, 20], [3000, 20], [4000, 30]],
    );
  });

  it("returns no OI before its first sample or when the ranges do not overlap", () => {
    assert.deepEqual(alignOiToCandles(candles, [[5000, 20]]), []);
    assert.deepEqual(alignOiToCandles(candles, []), []);
  });
});

describe("profileBarLayout", () => {
  it("anchors profile rows to the active price scale coordinates", () => {
    const prices = [3, 2, 1];
    const first = profileBarLayout(prices, (p) => 100 - p * 10);
    const zoomed = profileBarLayout(prices, (p) => 200 - p * 20);
    assert.deepEqual(first, [
      { top: 65, height: 10 },
      { top: 75, height: 10 },
      { top: 85, height: 10 },
    ]);
    assert.deepEqual(zoomed, [
      { top: 130, height: 20 },
      { top: 150, height: 20 },
      { top: 170, height: 20 },
    ]);
  });

  it("skips prices that the chart cannot project", () => {
    const layout = profileBarLayout([3, 2, 1], (p) => p === 2 ? null : p * 10);
    assert.ok(layout[0]);
    assert.equal(layout[1], null);
    assert.ok(layout[2]);
  });

  it("clips levels outside the visible price plot", () => {
    assert.deepEqual(
      profileBarLayout([3, 2, 1], (p) => 100 - p * 10, 60),
      [null, null, null],
    );
  });
});

describe("volumeProfile", () => {
  // two bars: up bar 10→12 vol 4, down bar 12→10 vol 6
  const rows = [
    [1, 10, 12, 10, 12, 4],
    [2, 12, 12, 10, 10, 6],
  ] as Array<[number, number, number, number, number, number]>;

  it("splits the visible range into levels top-down with a POC", () => {
    const { rows: levels, poc } = volumeProfile(rows, 0, 1, 2);
    assert.equal(levels.length, 2);
    assert.ok(levels[0].price > levels[1].price);
    // all 10 units of volume are assigned somewhere
    const total = levels.reduce((s, r) => s + r.volume, 0);
    assert.ok(Math.abs(total - 10) < 1e-9);
    const pocs = levels.filter((r) => r.poc);
    assert.equal(pocs.length, 1);
    assert.equal(poc, pocs[0].price);
  });

  it("clamps out-of-range indices and tolerates reversed order", () => {
    const fwd = volumeProfile(rows, 0, 1, 4);
    const rev = volumeProfile(rows, 1, 0, 4);
    const far = volumeProfile(rows, -100, 999, 4);
    assert.deepEqual(rev.rows.map((r) => r.volume), fwd.rows.map((r) => r.volume));
    assert.deepEqual(far.rows.map((r) => r.volume), fwd.rows.map((r) => r.volume));
  });

  it("handles empty input and a single level", () => {
    assert.deepEqual(volumeProfile([], 0, 5).rows, []);
    const one = volumeProfile(rows, 0, 0, 1);
    assert.equal(one.rows.length, 1);
    assert.equal(one.rows[0].poc, true);
  });
});
