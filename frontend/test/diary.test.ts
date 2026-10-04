// Pure diary view-model tests: URL param, markers, chart window, stats.
// Run with `npm test` (node --test).
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  chartRange,
  diaryMarkers,
  diarySymbols,
  fmtPnl,
  fmtPrice,
  overallR,
  safeStats,
  traderFromSearch,
} from "../src/trade/diary.ts";
import type { DiaryData, DiaryPosition, DiaryTrade } from "../src/types/protocol.ts";
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
    assert.match(m[1].text, /ВЫХОД \+10/);
  });

  it("labels a short correctly", () => {
    const m = diaryMarkers([trade({ side: "short", direction: "sell→buy" })], [], "BTC/USDT");
    assert.match(m[0].text, /ВХОД SHORT/);
    assert.equal(m[0].side, "short");
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
  it("fmtPrice keeps 2 decimals for normal prices", () => {
    assert.equal(fmtPrice(64120.5), "64120.50");
    assert.equal(fmtPrice(Number.NaN), "—");
  });

  it("fmtPnl signs positives and tolerates NaN", () => {
    assert.equal(fmtPnl(1234.5), "+1,234.5");
    assert.equal(fmtPnl(-12), "-12");
    assert.equal(fmtPnl(Number.NaN), "—");
  });
});
