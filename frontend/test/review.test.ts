// Post-trade review logic: excursions, verdicts, streaks, session summary.
// Pure functions — run with `npm test` (node --test).
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { buildReview, classifyExit, excursion, summarize, toR } from "../src/trade/review.ts";
import type { CandleRow, DiaryTrade } from "../src/types/protocol.ts";

const T0 = 1_789_000_000_000;

/** One bar: [ts, open, high, low, close, volume]. */
function bar(ts: number, o: number, h: number, l: number, c: number): CandleRow {
  return [ts, o, h, l, c, 10];
}

function trade(over: Partial<DiaryTrade> = {}): DiaryTrade {
  return {
    symbol: "BTC/USDT",
    side: "long",
    direction: "buy→sell",
    qty: 1,
    entry_price: 100,
    exit_price: 110,
    pnl: 10,
    r: 1,
    entry_time_ms: T0,
    exit_time_ms: T0 + 600_000,
    entry_fee: 0.1,
    exit_fee: 0.1,
    fee: 0.2,
    entry_order_id: "e1",
    exit_order_id: "x1",
    ...over,
  };
}

describe("excursion", () => {
  it("measures the best and worst move between entry and exit", () => {
    // Long from 100: the exit bar itself is included (its range is the last
    // thing the trade saw), so +32 is the best it got, −1 the worst.
    const candles = [
      bar(T0 - 60_000, 100, 105, 99, 104),
      bar(T0, 100, 130, 99, 128),
      bar(T0 + 600_000, 128, 132, 120, 126),
    ];
    const got = excursion(candles, T0, T0 + 600_000, "long", 100);
    assert.equal(got.mfe, 32, "the exit bar's high 132 is +32 in our favour");
    assert.equal(got.mae, 1, "low 99 is only −1 against us");
    assert.equal(got.bars, 2);
  });

  it("mirrors the extremes for a short", () => {
    const candles = [bar(T0, 100, 106, 80, 84)];
    const got = excursion(candles, T0, T0 + 300_000, "short", 100);
    assert.equal(got.mfe, 20, "low 80 is +20 in a short's favour");
    assert.equal(got.mae, 6, "high 106 is −6 against a short");
  });

  it("ignores bars outside the trade window", () => {
    const candles = [bar(T0 - 600_000, 1, 500, 1, 1), bar(T0, 100, 101, 99, 100)];
    const got = excursion(candles, T0, T0, "long", 100);
    assert.equal(got.bars, 1);
    assert.equal(got.mfe, 1, "the bar before the entry must not count");
  });

  it("is zero for an empty or out-of-range window", () => {
    assert.deepEqual(excursion([], T0, T0 + 1, "long", 100), { mfe: 0, mae: 0, bars: 0 });
    const got = excursion([bar(T0 - 10_000_000, 1, 9, 1, 1)], T0, T0 + 1, "long", 100);
    assert.equal(got.bars, 0);
  });
});

describe("toR", () => {
  it("divides a distance by the planned risk", () => {
    assert.equal(toR(30, 10), 3);
    assert.equal(toR(0, 10), 0);
  });

  it("returns null without a defined risk instead of Infinity", () => {
    // An entry with no stop has no R: rendering "∞R" would be a lie.
    assert.equal(toR(30, 0), null);
    assert.equal(toR(30, Number.NaN), null);
  });
});

describe("classifyExit", () => {
  it("calls a fill at the stop a stop-out", () => {
    assert.equal(classifyExit({ entry_price: 100, exit_price: 95 }, { stop: 95 }), "stop");
  });

  it("tolerates a little slippage around the level", () => {
    // 0.1% of the entry is the tolerance; 0.05 counts as "hit the level",
    // 0.2 does not (that is a different exit entirely).
    assert.equal(classifyExit({ entry_price: 100, exit_price: 95.05 }, { stop: 95 }), "stop");
    assert.equal(classifyExit({ entry_price: 100, exit_price: 110.05 }, { target: 110 }), "target");
    assert.equal(classifyExit({ entry_price: 100, exit_price: 110.2 }, { target: 110 }), "manual");
  });

  it("calls anything else a manual exit", () => {
    assert.equal(classifyExit({ entry_price: 100, exit_price: 97 }, { stop: 95 }), "manual");
    assert.equal(classifyExit({ entry_price: 100, exit_price: 97 }, {}), "manual");
  });

  it("marks an unfinished trade as open", () => {
    assert.equal(classifyExit({ entry_price: 100, exit_price: 100 }, {}, true), "open");
  });

  it("trusts the server's reason over price proximity", () => {
    // A stop is a MARKET order: on a wide bar it can fill far past its trigger
    // (measured in replay — trigger 79519, fill 79233). Without the explicit
    // reason that stop-out would be reported as a manual exit and blamed on
    // the trader.
    assert.equal(
      classifyExit({ entry_price: 100, exit_price: 91, exit_reason: "stop" }, { stop: 95 }),
      "stop",
      "0.4% past the trigger is still the stop doing its job",
    );
    assert.equal(
      classifyExit({ entry_price: 100, exit_price: 115, exit_reason: "take" }, { target: 110 }),
      "target",
      "a gapped take-profit counts too",
    );
    assert.equal(
      classifyExit({ entry_price: 100, exit_price: 100, exit_reason: "stop" }, {}),
      "stop",
      "the reason wins even with no recorded level",
    );
    // No reason recorded ⇒ fall back to the old price test.
    assert.equal(classifyExit({ entry_price: 100, exit_price: 95.05 }, { stop: 95 }), "stop");
  });
});

describe("buildReview", () => {
  const levels = {
    "e1-x1": { stop: 95 },
    "e2-x2": { stop: 95 },
    "e3-x3": { stop: 95 },
  };

  function candlesFor(peak: number, dip: number) {
    return (_t: DiaryTrade): CandleRow[] => [
      bar(T0, 100, peak, dip, 100),
      bar(T0 + 600_000, 100, peak, dip, 100),
    ];
  }

  it("turns the excursion into R against the planned risk", () => {
    // Planned risk = |100 − 95| = 5 per unit, so +30 of favourable excursion
    // is 6R — which is exactly why a 1R loss here is worth looking at.
    const rows = buildReview(
      [trade({ pnl: -5, r: -0.5 })],
      candlesFor(130, 99),
      { levels },
    );
    assert.equal(rows.length, 1);
    const row = rows[0];
    assert.equal(row.mfeR, 6, "+30 over a 5-point risk = 6R");
    assert.equal(row.maeR, 0.2, "−1 over 5 = 0.2R");
    // Realized R comes from pnl / (stop distance × qty) = −5 / (5 × 1) = −1R —
    // the same unit the excursion uses — so the whole swing was given back.
    assert.equal(row.gaveBackR, 7, "6R of favourable move, and it ended −1R");
  });

  it("keeps the excursion in the same R as the PnL column", () => {
    // Real trade: SOL short 119.76, stop 121.00, TP 118.30, risk budget 10
    // USDT. The stop distance is 1.24, but sizing adds the round-trip fees, so
    // 1R of MONEY is 10 USDT, not 1.24 × 5.429. The excursion must use the
    // same denominator, otherwise "было в +1.2R" sits next to "+0.47R" for the
    // very same take-profit exit (the gap is the 0.32R of fees).
    const rows = buildReview(
      [
        trade({
          side: "short",
          direction: "sell→buy",
          qty: 5.429176393941054,
          entry_price: 119.76,
          exit_price: 118.3,
          pnl: 4.695423204299964,
          r: 0.46954232042999644,
          entry_order_id: "1",
          exit_order_id: "2",
        }),
      ],
      () => [bar(T0, 119.76, 120.628, 118.272, 118.3)],
      { levels: { "1-2": { stop: 121, target: 118.3 } }, riskUsdt: 10 },
    );
    const row = rows[0];
    // The take-profit closed the position AT 118.30, so the favourable move
    // stops there even though the exit bar wicked 0.028 lower.
    assert.ok(Math.abs((row.mfeR ?? 0) - 0.79266) < 1e-5, `mfeR ${row.mfeR}`);
    assert.ok(Math.abs((row.maeR ?? 0) - 0.471253) < 1e-5, `maeR ${row.maeR}`);
    assert.ok(
      Math.abs((row.gaveBackR ?? 0) - 0.32312) < 1e-4,
      `gave back ${row.gaveBackR}`,
    );
    // Sanity: a full stop-out is exactly −1R of money, by construction.
    assert.ok(row.mfeR !== null && row.mfeR < 1, "TP inside 1R must read < 1R");
  });

  it("caps a stop-out's excursion at the level that closed it", () => {
    // The stop fired on a wide bar (low 80), but the position was closed at
    // 95: the trader never lived through the rest of that move, so "было в −"
    // must read −1R, not −4R. Money R is the 10 USDT budget (2 × 5 points).
    const rows = buildReview(
      [trade({ qty: 2, exit_price: 95, pnl: -10, r: -1, exit_reason: "stop" })],
      () => [bar(T0, 100, 101, 80, 95)],
      { levels: { "e1-x1": { stop: 95 } }, riskUsdt: 10 },
    );
    assert.equal(rows[0].kind, "stop");
    assert.equal(rows[0].maeR, 1, "the stop distance, not the bar's full wick");
  });

  it("caps a take-profit's excursion at the target", () => {
    // The TP filled at 110 while the bar ran to 130. The trade made 1R, so
    // "было в +" cannot claim +3R of profit it never booked.
    const rows = buildReview(
      [trade({ exit_price: 110, pnl: 10, r: 1, exit_reason: "take" })],
      () => [bar(T0, 100, 130, 99, 110)],
      { levels: { "e1-x1": { stop: 95, target: 110 } }, riskUsdt: 10 },
    );
    assert.equal(rows[0].kind, "target");
    assert.equal(rows[0].mfeR, 1, "the target distance, not the bar's full range");
  });

  it("flags a trade that had no stop", () => {
    const rows = buildReview([trade()], () => [], {});
    assert.ok(rows[0].flags.includes("без стопа — риск не определён"));
    assert.equal(rows[0].mfeR, null, "no stop means no R at all");
  });

  it("flags closing by hand before the stop", () => {
    const rows = buildReview(
      [trade({ exit_price: 97, pnl: -3, r: -0.3 })],
      candlesFor(101, 99),
      { levels },
    );
    assert.equal(rows[0].kind, "manual");
    assert.ok(rows[0].flags.includes("закрыл до стопа: правило не выдержано"));
  });

  it("falls back to percent when the entry had no stop", () => {
    // No stop ⇒ no R (there is no defined risk), but "how far did it go" is
    // still the question worth answering, so it is reported in percent.
    const rows = buildReview(
      [trade({ pnl: -3, r: 0 })],
      () => [bar(T0, 100, 120, 90, 110), bar(T0 + 600_000, 110, 120, 90, 110)],
      {},
    );
    const row = rows[0];
    assert.equal(row.mfeR, null, "no stop means no R");
    assert.equal(row.mfePct, 20, "+20 off a 100 entry");
    assert.equal(row.maePct, 10, "−10 off a 100 entry");
    assert.ok(row.flags.includes("без стопа — риск не определён"));
  });

  it("calls out a stop that filled past its trigger", () => {
    // The market gapped through the level: 95 → 91 on a 100 entry is a 4% slip,
    // which is a real cost of using a market stop and must not be hidden.
    const rows = buildReview(
      [trade({ exit_price: 91, pnl: -9, r: -1, exit_reason: "stop" })],
      () => [bar(T0, 100, 101, 90, 92)],
      { levels: { "e1-x1": { stop: 95 } } },
    );
    assert.equal(rows[0].kind, "stop", "the reason from the fill decides");
    assert.ok(
      rows[0].flags.some((f) => f.startsWith("стоп исполнен с пробоем: −4.00%")),
      `the slip must be measured, got: ${rows[0].flags.join(" | ")}`,
    );
    assert.ok(
      !rows[0].flags.includes("закрыл до стопа: правило не выдержано"),
      "the trader followed the rule — the market gapped, they did not fold",
    );
  });

  it("flags a losing trade that had been well in the profit", () => {
    const rows = buildReview(
      [trade({ exit_price: 95, pnl: -5, r: -1 })],
      candlesFor(130, 99), // ran +30 (6R) before stopping out
      { levels },
    );
    assert.ok(rows[0].flags.includes("упущенная прибыль"));
  });

  it("counts the run of stop-outs before each trade (прогрештор)", () => {
    const stopped = (n: string) =>
      trade({ exit_price: 95, pnl: -5, r: -1, entry_order_id: n, exit_order_id: `x${n}` });
    const rows = buildReview(
      [
        stopped("a"),
        stopped("b"),
        trade({ exit_price: 120, pnl: 20, r: 2, entry_order_id: "c", exit_order_id: "xc" }),
        stopped("d"),
      ],
      () => [bar(T0, 100, 101, 99, 100)],
      { levels: { ...levels, "a-xa": { stop: 95 }, "b-xb": { stop: 95 }, "c-xc": { stop: 95 }, "d-xd": { stop: 95 } } },
    );
    assert.deepEqual(rows.map((r) => r.priorStopStreak), [0, 1, 2, 0]);
  });

  it("orders trades oldest first regardless of input order", () => {
    const late = trade({ entry_time_ms: T0 + 9_000_000, entry_order_id: "z", exit_order_id: "xz" });
    const early = trade({ entry_time_ms: T0, entry_order_id: "a", exit_order_id: "xa" });
    const rows = buildReview([late, early], () => [], { levels: {} });
    assert.deepEqual(rows.map((r) => r.trade.entry_time_ms), [T0, T0 + 9_000_000]);
  });
});

describe("summarize", () => {
  it("aggregates the session without inventing a win rate", () => {
    const rows = buildReview(
      [
        trade({ pnl: 20, r: 2, fee: 0.3, exit_price: 120, entry_order_id: "a", exit_order_id: "xa" }),
        trade({ pnl: -5, r: -1, fee: 0.2, exit_price: 95, entry_order_id: "b", exit_order_id: "xb" }),
      ],
      () => [bar(T0, 100, 101, 99, 100)],
      { levels: { "a-xa": { stop: 95 }, "b-xb": { stop: 95 } } },
    );
    const sum = summarize(rows);
    assert.equal(sum.trades, 2);
    assert.equal(sum.wins, 1);
    assert.equal(sum.losses, 1);
    assert.equal(sum.winRate, 0.5);
    assert.ok(Math.abs(sum.totalR - 1) < 1e-9);
    assert.ok(Math.abs(sum.avgR - 0.5) < 1e-9);
    assert.ok(Math.abs(sum.fees - 0.5) < 1e-9);
    assert.equal(sum.protectedShare, 1, "both trades had a stop");
    assert.equal(sum.worstStopStreak, 1);
  });

  it("is all zeros for an empty session", () => {
    const sum = summarize([]);
    assert.equal(sum.trades, 0);
    assert.equal(sum.winRate, 0, "no trades means no win rate, not NaN");
    assert.equal(sum.avgR, 0);
    assert.equal(sum.protectedShare, 0);
  });

  it("counts how many losing trades were given back after being in profit", () => {
    const rows = buildReview(
      [trade({ exit_price: 95, pnl: -5, r: -1 })],
      () => [bar(T0, 100, 130, 99, 100)],
      { levels: { "e1-x1": { stop: 95 } } },
    );
    assert.equal(summarize(rows).gaveBack, 1);
  });
});
