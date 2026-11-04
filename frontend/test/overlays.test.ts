// Pure overlay-math tests: no DOM, no network, no dependencies.
// Run with `bun test` or `npm test` (node --test).
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  barWord,
  calcQty,
  riskAtStop,
  priceAxisFormat,
  priceTick,
  countBars,
  cumulativeDelta,
  formatCompactUsdt,
  formatPriceValue,
  formatRuler,
  invertTime,
  nearestCandleTime,
  pairTradeFills,
  positionBands,
  positionDirection,
  profileBarLayout,
  riskReward,
  rulerStats,
  snapTime,
  toBox,
  tradeBoxShape,
  tradeOutcome,
  tradeVisible,
  tradeZones,
  volumeProfile,
} from "../src/chart/overlays.ts";
import type { FillLine } from "../src/types/protocol.ts";
import { followLatest } from "../src/chart/rangeSync.ts";

describe("toBox", () => {
  it("normalizes any click order", () => {
    assert.deepEqual(toBox(10, 20, 30, 5), { left: 10, top: 5, width: 20, height: 15 });
    assert.deepEqual(toBox(30, 5, 10, 20), { left: 10, top: 5, width: 20, height: 15 });
  });
});

describe("follow mode", () => {
  it("scrolls to latest only when follow is enabled", () => {
    let scrolls = 0;
    followLatest(false, () => { scrolls += 1; });
    assert.equal(scrolls, 0);
    followLatest(true, () => { scrolls += 1; });
    assert.equal(scrolls, 1);
  });
});

describe("position tool", () => {
  it("detects direction by stop side", () => {
    assert.equal(positionDirection(100, 90), "long");
    assert.equal(positionDirection(100, 110), "short");
  });

  describe("trade fill pairing", () => {
    const fill = (side: string, amount: number, price: number, timestamp: number): FillLine => ({
      id: `${side}-${timestamp}-${price}`,
      symbol: "BTC/USDT",
      side,
      type: "market",
      price,
      amount,
      cost: amount * price,
      fee: 0,
      orderId: `${timestamp}`,
      timestamp,
      datetime: "",
      maker: false,
    });

    it("connects an entry to partial exits using FIFO lots", () => {
      const links = pairTradeFills([
        fill("sell", 0.4, 110, 3),
        fill("sell", 0.6, 105, 2),
        fill("buy", 1, 100, 1),
      ], "BTC/USDT");
      assert.deepEqual(links, [
        {
          entryTimestamp: 1,
          entryPrice: 100,
          exitTimestamp: 2,
          exitPrice: 105,
          direction: "long",
          entryOrderId: "1",
        },
        {
          entryTimestamp: 1,
          entryPrice: 100,
          exitTimestamp: 3,
          exitPrice: 110,
          direction: "long",
          entryOrderId: "1",
        },
      ]);
    });

    it("keeps a trade closed on the entry millisecond in the right direction", () => {
      // A manual close inside the entry bar leaves both fills on one timestamp,
      // so the stored array order is arbitrary. The engine sequence decides.
      const entry: FillLine = { ...fill("buy", 1, 100, 5), id: "1_0", orderId: "1" };
      const exit: FillLine = { ...fill("sell", 1, 100, 5), id: "2_0", orderId: "2" };
      for (const fills of [[entry, exit], [exit, entry]]) {
        const links = pairTradeFills(fills, "BTC/USDT");
        assert.equal(links.length, 1);
        assert.equal(links[0].direction, "long");
        assert.equal(links[0].entryOrderId, "1");
        assert.equal(links[0].entryPrice, 100);
        assert.equal(links[0].exitPrice, 100);
      }
    });

    it("starts a new opposite position after a reversal", () => {
      const links = pairTradeFills([
        fill("sell", 1.5, 90, 2),
        fill("buy", 1, 100, 1),
        fill("buy", 0.5, 80, 3),
      ], "BTC/USDT");
      assert.deepEqual(links, [
        {
          entryTimestamp: 1,
          entryPrice: 100,
          exitTimestamp: 2,
          exitPrice: 90,
          direction: "long",
          entryOrderId: "1",
        },
        {
          entryTimestamp: 2,
          entryPrice: 90,
          exitTimestamp: 3,
          exitPrice: 80,
          direction: "short",
          entryOrderId: "2",
        },
      ]);
    });
  });

  it("computes reward/risk", () => {
    assert.equal(riskReward(100, 90, 120), 2);
    assert.equal(riskReward(100, 100, 120), null);
  });

  describe("closed-trade box", () => {
    const link = (
      direction: "long" | "short",
      entryPrice: number,
      exitPrice: number,
    ) => ({
      entryTimestamp: 1,
      entryPrice,
      exitTimestamp: 2,
      exitPrice,
      direction,
      entryOrderId: "1",
    });

    it("measures a long's result and R against its stop", () => {
      const outcome = tradeOutcome(link("long", 100, 120), 90);
      assert.equal(outcome.move, 20);
      assert.equal(outcome.pct, 20);
      assert.equal(outcome.r, 2);
      assert.equal(outcome.profitable, true);
    });

    it("measures a short symmetrically", () => {
      const outcome = tradeOutcome(link("short", 100, 80), 110);
      assert.equal(outcome.move, 20);
      assert.equal(outcome.r, 2);
      assert.equal(outcome.profitable, true);
    });

    it("reports a loss and no R when the stop is unknown", () => {
      const outcome = tradeOutcome(link("long", 100, 95), null);
      assert.equal(outcome.profitable, false);
      assert.equal(outcome.r, null);
      assert.equal(outcome.pct, -5);
    });

    it("spans entry, exit and stop, and keeps same-bar trades visible", () => {
      const box = tradeBoxShape(10, 100, 50, 80, 120);
      assert.equal(box.left, 10);
      assert.equal(box.width, 40);
      assert.equal(box.top, 80);
      assert.equal(box.height, 40);
      assert.equal(box.stopY, 120);
      assert.equal(box.rewardY, null);
      assert.equal(tradeBoxShape(10, 100, 10, 90, null).width, 3);
    });

    it("stretches the box up to the target when the trade had one", () => {
      const box = tradeBoxShape(10, 100, 50, 120, 140, 60);
      assert.equal(box.top, 60);
      assert.equal(box.height, 80);
      assert.equal(box.rewardY, 60);
    });

    describe("visibility", () => {
      const times = [10, 20, 30, 40];

      it("hides a trade whose bars the replay has not revealed yet", () => {
        // After a reload the replay restarts: a persisted fill from later in
        // the session must not pile up on the last visible candle.
        assert.equal(tradeVisible({ entryTimestamp: 50, exitTimestamp: 60 }, times), false);
        assert.equal(tradeVisible({ entryTimestamp: 30, exitTimestamp: 50 }, times), false);
      });

      it("shows a trade once both of its bars are on the chart", () => {
        assert.equal(tradeVisible({ entryTimestamp: 20, exitTimestamp: 40 }, times), true);
        assert.equal(tradeVisible({ entryTimestamp: 10, exitTimestamp: 10 }, times), true);
      });

      it("hides trades from before the loaded range", () => {
        assert.equal(tradeVisible({ entryTimestamp: 5, exitTimestamp: 20 }, times), false);
      });

      it("shows nothing without candles", () => {
        assert.equal(tradeVisible({ entryTimestamp: 1, exitTimestamp: 2 }, []), false);
      });
    });

    describe("zones", () => {
      it("shows the planned target even when the stop was hit", () => {
        // Long stopped out: the green leg must still be the target, not nothing.
        const zones = tradeZones(link("long", 100, 90), 90, 130);
        assert.equal(zones.rewardPrice, 130);
        assert.equal(zones.adversePrice, 90);
      });

      it("shows the planned target on a short stop-out too", () => {
        const zones = tradeZones(link("short", 100, 110), 110, 70);
        assert.equal(zones.rewardPrice, 70);
        assert.equal(zones.adversePrice, 110);
      });

      it("falls back to the realized exit when there was no target", () => {
        const win = tradeZones(link("long", 100, 120), 90, null);
        assert.equal(win.rewardPrice, 120);
        assert.equal(win.adversePrice, 90);
        const loss = tradeZones(link("long", 100, 90), 90, null);
        assert.equal(loss.rewardPrice, null);
        assert.equal(loss.adversePrice, 90);
      });

      it("extends the risk leg to an exit that slipped past the stop", () => {
        const zones = tradeZones(link("long", 100, 85), 90, null);
        assert.equal(zones.adversePrice, 85);
      });

      it("ignores levels on the wrong side of the entry", () => {
        const zones = tradeZones(link("long", 100, 100), 120, 80);
        assert.equal(zones.rewardPrice, null);
        assert.equal(zones.adversePrice, null);
      });
    });
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

describe("priceTick", () => {
  it("scales the axis step with the price magnitude", () => {
    // The bug: lightweight-charts defaults minMove to 0.01, so DOGE at 0.09
    // could only ever step by whole cents — "точную цену не видно" and the order
    // ticket was unusable.
    assert.equal(priceTick(0.09), 0.0001, "a sub-cent pair needs sub-cent steps");
    assert.equal(priceTick(0.9), 0.001);
    assert.equal(priceTick(8), 0.01);
    assert.equal(priceTick(80), 0.1);
    assert.equal(priceTick(80_000), 1);
  });

  it("keeps the tick a clean power of ten", () => {
    for (const price of [0.00012, 0.09, 1, 1.05, 79_967.9, 123_456]) {
      const tick = priceTick(price);
      const exponent = Math.round(Math.log10(tick));
      assert.ok(
        Math.abs(tick - Math.pow(10, exponent)) < 1e-12,
        `${price} → ${tick} must be a clean power of ten`,
      );
    }
  });

  it("falls back for a missing or broken price", () => {
    assert.equal(priceTick(0), 0.01);
    assert.equal(priceTick(Number.NaN), 0.01);
    assert.equal(priceTick(-5), 0.01);
  });

  it("derives the decimals the axis must print", () => {
    assert.deepEqual(priceAxisFormat(0.09), { precision: 4, minMove: 0.0001 });
    // Whole units are as fine as BTC needs: labels stay 79 967 / 79 968, not
    // "79967.000".
    assert.deepEqual(priceAxisFormat(79_967.9), { precision: 0, minMove: 1 });
    assert.equal(priceAxisFormat(0).precision, 2);
  });

  it("adds axis precision when the visible price range is zoomed in", () => {
    assert.deepEqual(priceAxisFormat(1.33, { from: 1.329, to: 1.331 }), {
      precision: 5,
      minMove: 0.00001,
    });
    assert.deepEqual(priceAxisFormat(80_000, { from: 79_999.999, to: 80_000.001 }), {
      precision: 5,
      minMove: 0.00001,
    });
  });
});

describe("formatPriceValue", () => {
  it("preserves the precision present in OHLC values", () => {
    assert.equal(formatPriceValue(1.3115), "1.3115");
    assert.equal(formatPriceValue(79_967.9), "79967.9");
    assert.equal(formatPriceValue(0.000001), "0.000001");
  });
});

describe("calcQty", () => {
  it("sizes by fixed USDT risk", () => {
    assert.equal(calcQty(10, 100, 90), 1);
    assert.equal(calcQty(10, 100, 110), 1);
  });

  it("keeps the whole budget inside the risk, commission included", () => {
    // The regression: sizing on price distance alone overspends the budget,
    // because a tight stop means a large notional and a large fee. A 1% stop on
    // a $79 700 BTC with a $10 budget used to become a $1000 position, so the
    // stop cost $15.91 instead of $10 (1.59R).
    const RISK = 10, BTC = 79_704, TAKER = 0.0025;
    for (const stopPct of [0.5, 1, 2, 3, 5]) {
      const entry = BTC;
      const sl = entry * (1 - stopPct / 100);
      const qty = calcQty(RISK, entry, sl, TAKER);
      assert.ok(qty !== null, `${stopPct}% stop must be sizeable`);
      const worst = riskAtStop(qty, entry, sl, TAKER);
      assert.ok(worst !== null, `${stopPct}% stop must have a worst case`);
      assert.ok(
        Math.abs(worst - RISK) < 1e-9,
        `${stopPct}% stop: worst case ${worst} must equal the ${RISK} budget`,
      );
    }
  });

  it("shrinks the size as the fee grows", () => {
    const noFee = calcQty(10, 100, 99, 0);
    const withFee = calcQty(10, 100, 99, 0.0025);
    assert.ok(noFee !== null && withFee !== null);
    assert.ok(withFee < noFee, `fee must reduce size: ${withFee} vs ${noFee}`);
  });

  it("budgets the exit fee at the stop price", () => {
    const qty = calcQty(10, 100, 90, 0.01);
    assert.ok(qty !== null);
    assert.ok(Math.abs(qty - 10 / 11.9) < 1e-12);
    assert.ok(Math.abs(riskAtStop(qty, 100, 90, 0.01)! - 10) < 1e-12);
  });

  it("treats a missing fee rate as no fee", () => {
    assert.equal(calcQty(10, 100, 90, 0), 1, "feeRate 0 = price distance only");
  });

  it("blocks degenerate input", () => {
    assert.equal(calcQty(10, 100, 100), null);
    assert.equal(calcQty(0, 100, 90), null);
    assert.equal(calcQty(10, Number.NaN, 90), null);
    assert.equal(calcQty(10, 100, 90, -0.1), 1, "a negative rate is treated as no fee");
    assert.equal(calcQty(10, 100, 90, Number.NaN), 1);
  });
});

describe("riskAtStop", () => {
  it("is price distance plus the round-trip commission", () => {
    // 1 unit at 100 with the stop at 99 loses 1, plus 0.1 entry + 0.099 exit fee.
    assert.equal(riskAtStop(2, 100, 99, 0.0005), 2 * (1 + (100 + 99) * 0.0005));
    assert.equal(riskAtStop(2, 100, 99), 2, "without a fee rate it is pure distance");
  });

  it("blocks degenerate input", () => {
    assert.equal(riskAtStop(0, 100, 99), null);
    assert.equal(riskAtStop(2, 100, 100), null);
    assert.equal(riskAtStop(2, Number.NaN, 99), null);
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

describe("formatCompactUsdt", () => {
  it("shortens billions/millions/thousands", () => {
    assert.equal(formatCompactUsdt(8813493358.81), "8.81B");
    assert.equal(formatCompactUsdt(463650000), "463.65M");
    assert.equal(formatCompactUsdt(12500), "12.5K");
    assert.equal(formatCompactUsdt(999), "999");
    assert.equal(formatCompactUsdt(Number.NaN), "—");
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
