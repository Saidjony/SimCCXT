// Pure diary view-model tests: URL param, markers, chart window, stats.
// Run with `npm test` (node --test).
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  chartRange,
  diaryMarkers,
  diarySymbols,
  fmtChartTime,
  fmtPnl,
  fmtPrice,
  markersOutsideCandles,
  overallR,
  safeStats,
  tradeRef,
  traderFromSearch,
} from "../src/trade/diary.ts";
import type { CandleRow, DiaryData, DiaryPosition, DiaryTrade } from "../src/types/protocol.ts";
import { apiBaseFrom } from "../src/types/protocol.ts";

describe("traderFromSearch", () => {
  it("reads and percent-decodes the nick", () => {
    assert.equal(traderFromSearch("?trader=%D0%91%D1%8B%D0%BA-7"), "Бык-7");
    assert.equal(traderFromSearch("?trader=abc&x=1"), "abc");
    assert.equal(traderFromSearch("?x=1&trader=abc"), "abc");
  });

  it("returns null for missing/blank nick (terminal, not diary)", () => {
    assert.equal(traderFromSearch(""), null);
    assert.equal(traderFromSearch("?x=1"), null);
    assert.equal(traderFromSearch("?trader="), null);
    assert.equal(traderFromSearch("?trader=%20%20"), null);
  });
});

describe("apiBaseFrom (HTTP = ws port + 1)", () => {
  it("bumps the port and swaps the scheme", () => {
    assert.equal(apiBaseFrom("ws://127.0.0.1:19003"), "http://127.0.0.1:19004");
    assert.equal(apiBaseFrom("ws://127.0.0.1:9001"), "http://127.0.0.1:9002");
    assert.equal(apiBaseFrom("wss://example.com/sim"), "https://example.com");
  });

  it("falls back on garbage input", () => {
    assert.equal(apiBaseFrom("not a url"), "http://127.0.0.1:9002");
  });
});

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
    entry_time_ms: 1_000,
    exit_time_ms: 2_000,
    entry_fee: 0.1,
    exit_fee: 0.1,
    fee: 0.2,
    entry_order_id: "e1",
    exit_order_id: "x1",
    ...over,
  };
}

function pos(over: Partial<DiaryPosition> = {}): DiaryPosition {
  return {
    symbol: "BTC/USDT",
    side: "long",
    qty: 1,
    entry_price: 100,
    entry_time_ms: 500,
    mark_price: 105,
    unrealized: 5,
    ...over,
  };
}

function data(over: Partial<DiaryData> = {}): DiaryData {
  return { type: "diary", nick: "Тест", ...over };
}

describe("diaryMarkers", () => {
  it("emits entry+exit per trade, ordered by time", () => {
    const m = diaryMarkers([trade()], [], "BTC/USDT");
    assert.equal(m.length, 2);
    assert.equal(m[0].kind, "entry");
    assert.equal(m[1].kind, "exit");
    assert.ok(m[0].time_ms <= m[1].time_ms);
    assert.match(m[0].text, /ВХОД LONG/);
    assert.match(m[1].text, /ВЫХОД 110\.00/);
    assert.match(m[1].text, /\+10/, "the exit label carries the PnL");
  });

  it("labels a short correctly", () => {
    const m = diaryMarkers([trade({ side: "short", direction: "sell→buy" })], [], "BTC/USDT");
    assert.match(m[0].text, /ВХОД SHORT/);
    assert.equal(m[0].side, "short");
  });

  it("labels every marker with its UTC time, so entry and exit are told apart", () => {
    // The chart time axis is UTC, so the label must match it.
    const m = diaryMarkers(
      [trade({ entry_time_ms: Date.UTC(2026, 8, 5, 12, 5), exit_time_ms: Date.UTC(2026, 8, 7, 1, 55) })],
      [],
      "BTC/USDT",
    );
    assert.match(m[0].text, /09-05 12:05/, `entry label: ${m[0].text}`);
    assert.match(m[1].text, /09-07 01:55/, `exit label: ${m[1].text}`);
    assert.match(m[0].text, /100\.00/, "entry shows its price");
    assert.match(m[1].text, /110\.00/, "exit shows its price");
  });

  it("adds an entry marker for an open position", () => {
    const m = diaryMarkers([], [pos()], "BTC/USDT");
    assert.equal(m.length, 1);
    assert.equal(m[0].kind, "entry");
    assert.equal(m[0].time_ms, 500);
  });

  it("only includes the requested symbol", () => {
    const m = diaryMarkers(
      [trade(), trade({ symbol: "ETH/USDT" })],
      [pos({ symbol: "ETH/USDT" })],
      "BTC/USDT",
    );
    assert.equal(m.length, 2);
    assert.ok(m.every((x) => x.text.length > 0));
  });
});

describe("diarySymbols", () => {
  it("lists symbols in first-seen order, deduped", () => {
    const s = diarySymbols(
      data({ trades: [trade(), trade({ symbol: "ETH/USDT" }), trade()], positions: [] }),
    );
    assert.deepEqual(s, ["BTC/USDT", "ETH/USDT"]);
  });

  it("is empty for an empty diary", () => {
    assert.deepEqual(diarySymbols(data()), []);
  });
});

describe("markersOutsideCandles", () => {
  const candles = (...ts: number[]): CandleRow[] =>
    ts.map((t) => [t, 1, 2, 0.5, 1.5, 10]);

  it("returns null when every marker falls inside the loaded candles", () => {
    assert.equal(
      markersOutsideCandles(
        [trade({ entry_time_ms: 2_000, exit_time_ms: 8_000 })],
        [],
        "BTC/USDT",
        candles(1_000, 3_000, 5_000, 7_000, 9_000),
      ),
      null,
    );
  });

  it("counts the markers the chart cannot draw", () => {
    // The server's cache holds only a recent tail: the early trade is off-chart.
    const got = markersOutsideCandles(
      [
        trade({ entry_time_ms: 1_000, exit_time_ms: 2_000 }),
        trade({ entry_time_ms: 70_000, exit_time_ms: 90_000, entry_order_id: "e2", exit_order_id: "x2" }),
      ],
      [pos({ entry_time_ms: 95_000 })],
      "BTC/USDT",
      candles(50_000, 60_000, 70_000),
    );
    assert.ok(got);
    // The first trade's entry AND exit (2), the second trade's exit only —
    // its entry sits exactly on the last bar (1) — plus the position (1).
    assert.equal(got.outside, 4);
    assert.equal(got.total, 5);
    assert.equal(got.from, 50_000);
    assert.equal(got.to, 70_000);
  });

  it("returns null without candles or without markers", () => {
    assert.equal(markersOutsideCandles([trade()], [], "BTC/USDT", []), null);
    assert.equal(markersOutsideCandles([], [], "BTC/USDT", candles(1, 2)), null);
  });

  it("ignores trades of another symbol", () => {
    assert.equal(
      markersOutsideCandles(
        [trade({ symbol: "ETH/USDT", entry_time_ms: 1, exit_time_ms: 2 })],
        [],
        "BTC/USDT",
        candles(50_000, 60_000),
      ),
      null,
    );
  });
});

describe("chartRange", () => {
  it("spans entries..exits with at least 1h padding", () => {
    const r = chartRange([trade()], [], "BTC/USDT");
    assert.ok(r, "range computed");
    assert.equal(r!.start_ms, 1_000 - 3_600_000); // span is 1s → min pad 1h
    assert.equal(r!.end_ms, 2_000 + 3_600_000);
  });

  it("pads 10% for a wide window", () => {
    const r = chartRange(
      [trade({ entry_time_ms: 0 + 3_600_000, exit_time_ms: 10_000_000 })],
      [],
      "BTC/USDT",
    );
    assert.ok(r);
    assert.ok(r!.start_ms < 3_600_000);
    assert.ok(r!.end_ms > 10_000_000);
  });

  it("returns null when the symbol has no events", () => {
    assert.equal(chartRange([], [], "BTC/USDT"), null);
    assert.equal(chartRange([trade({ symbol: "ETH/USDT" })], [], "BTC/USDT"), null);
  });

  it("ignores zero timestamps (unset times must not stretch the chart)", () => {
    assert.equal(chartRange([trade({ entry_time_ms: 0, exit_time_ms: 0 })], [], "BTC/USDT"), null);
  });

  it("uses the replay session window when provided (more context)", () => {
    const r = chartRange([trade()], [], "BTC/USDT", { start_ms: 500, end_ms: 9000 });
    assert.deepEqual(r, { start_ms: 500, end_ms: 9000 });
  });

  it("still covers trades that happen outside the replay window", () => {
    const r = chartRange(
      [trade({ entry_time_ms: 100, exit_time_ms: 200 })],
      [],
      "BTC/USDT",
      { start_ms: 5000, end_ms: 9000 },
    );
    assert.equal(r?.start_ms, 100);
    assert.equal(r?.end_ms, 9000);
  });
});

describe("stats guards", () => {
  it("safeStats fills defaults when the server omitted stats", () => {
    const s = safeStats(data());
    assert.equal(s.total_trades, 0);
    assert.equal(s.win_rate, 0);
    assert.equal(Number.isFinite(s.avg_loss), true);
  });

  it("overallR divides by risk, and never by zero", () => {
    assert.equal(overallR(data({ stats: safeStats({ ...data(), stats: undefined }), risk_usdt: 10 })), 0);
    const d = data({ risk_usdt: 10, stats: { ...safeStats(data()), total_pnl: 25 } });
    assert.equal(overallR(d), 2.5);
    assert.equal(overallR(data({ risk_usdt: 0, stats: { ...safeStats(data()), total_pnl: 25 } })), 0);
  });
});

describe("formatters", () => {
  it("fmtChartTime stamps in UTC, matching the chart's time axis", () => {
    assert.equal(fmtChartTime(Date.UTC(2026, 8, 5, 12, 5)), "09-05 12:05");
    assert.equal(fmtChartTime(Date.UTC(2026, 0, 1, 0, 0)), "01-01 00:00");
    assert.equal(fmtChartTime(0), "", "unset times get no label");
    assert.equal(fmtChartTime(NaN), "");
  });

  it("fmtPrice keeps 2 decimals for normal prices", () => {
    assert.equal(fmtPrice(64120.5), "64120.50");
    assert.equal(fmtPrice(Number.NaN), "—");
  });

  it("fmtPnl signs positives and tolerates NaN", () => {
    assert.equal(fmtPnl(1234.5), "+1,234.5");
    assert.equal(fmtPnl(-12), "-12");
    assert.equal(fmtPnl(Number.NaN), "—");
    // A fee-free session passes `-Math.abs(0)`; it must not read "-0".
    assert.equal(fmtPnl(-Math.abs(0)), "0");
  });

  it("tradeRef keeps page loads apart, order ids restart at 1", () => {
    // Two different trades, same symbol, same order ids (a reloaded tab
    // replays from the same bar). The engine instance must distinguish them,
    // otherwise the diary gives both the same review row and one "Итог".
    const first = { engine_id: "page-1", entry_order_id: "1", exit_order_id: "2" };
    const second = { engine_id: "page-2", entry_order_id: "1", exit_order_id: "2" };
    assert.notEqual(tradeRef(first), tradeRef(second));
    // Legacy payloads without an engine id still produce the old key.
    assert.equal(tradeRef({ entry_order_id: "1", exit_order_id: "2" }), "1-2");
  });
});
