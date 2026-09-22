// Pure overlay-math tests: no DOM, no network, no dependencies.
// Run with `bun test` or `npm test` (node --test).
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  barWord,
  calcQty,
  countBars,
  formatRuler,
  invertTime,
  positionBands,
  positionDirection,
  riskReward,
  rulerStats,
  snapTime,
  toBox,
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
