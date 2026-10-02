// Pure-protocol tests: no DOM, no network, no dependencies.
// Run with `bun test` or `npm test` (node --test).
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  applyCandleDelta,
  formatMoney,
  formatTs,
  parseServerMessage,
} from "../src/types/protocol.ts";

describe("parseServerMessage", () => {
  it("accepts a snapshot", () => {
    const m = parseServerMessage(
      '{"type":"snapshot","symbol":"BTC/USDT","timeframe":"1m","candles":[],"account":{},"replay":{}}',
    );
    assert.notEqual(m, null);
    assert.equal(m?.type, "snapshot");
  });

  it("accepts a candle delta", () => {
    const m = parseServerMessage(
      '{"type":"candle","symbol":"BTC/USDT","timeframe":"1m","candle":[1,2,3,4,5,6],"is_new":true}',
    );
    assert.equal(m?.type, "candle");
  });

  it("accepts heartbeat and resync (liveness protocol)", () => {
    assert.equal(parseServerMessage('{"type":"heartbeat","now_ms":1,"paused":true}')?.type, "heartbeat");
    assert.equal(parseServerMessage('{"type":"resync","to":7}')?.type, "resync");
  });

  it("rejects mouse/unknown traffic (never sent to Rust anyway)", () => {
    assert.equal(parseServerMessage('{"type":"mousemove","x":1}'), null);
    assert.equal(parseServerMessage("not json"), null);
    assert.equal(parseServerMessage('{"no":"type"}'), null);
  });

  it("accepts a snapshot with open interest", () => {
    const m = parseServerMessage(
      '{"type":"snapshot","symbol":"BTC/USDT","timeframe":"1m","candles":[],' +
      '"account":{},"replay":{},"oi":[[1000,5.5]],"oi_period":"5m"}',
    );
    assert.equal(m?.type, "snapshot");
    if (m?.type === "snapshot") {
      assert.deepEqual(m.oi, [[1000, 5.5]]);
      assert.equal(m.oi_period, "5m");
    }
  });

  it("accepts challenge traffic (login_ok / challenge_ok / board)", () => {
    const ok = parseServerMessage('{"type":"login_ok","to":7,"nick":"Бык-1"}');
    assert.equal(ok?.type, "login_ok");
    const ch = parseServerMessage('{"type":"challenge_ok","to":7,"joined":true,"nick":"Бык-1"}');
    assert.equal(ch?.type, "challenge_ok");
    const board = parseServerMessage(
      '{"type":"challenge_board","board":[{"nick":"A","risk":10,"pnl":20,"r":2,"equity":1000020}]}',
    );
    assert.equal(board?.type, "challenge_board");
    if (board?.type === "challenge_board") {
      assert.equal(board.board.length, 1);
      assert.equal(board.board[0].nick, "A");
      assert.equal(board.board[0].r, 2);
    }
  });

  it("accepts incremental open-interest updates", () => {
    const message = parseServerMessage(
      '{"type":"oi_update","to":7,"samples":[[2000,6.25]]}',
    );
    assert.equal(message?.type, "oi_update");
    if (message?.type === "oi_update") {
      assert.deepEqual(message.samples, [[2000, 6.25]]);
    }
  });

  it("accepts background preload progress and completion", () => {
    const queued = parseServerMessage(
      '{"type":"preload_progress","to":7,"done":0,"total":24,"failed":0,"status":"queued","note":""}',
    );
    assert.equal(queued?.type, "preload_progress");
    const step = parseServerMessage(
      '{"type":"preload_progress","to":7,"done":1,"total":24,"failed":0,' +
      '"symbol":"BTC/USDT","timeframe":"1h","status":"cached","note":"5000 bars from cache"}',
    );
    if (step?.type === "preload_progress") {
      assert.equal(step.done, 1);
      assert.equal(step.total, 24);
      assert.equal(step.symbol, "BTC/USDT");
      assert.equal(step.timeframe, "1h");
    } else {
      assert.fail("expected preload_progress");
    }
    const done = parseServerMessage(
      '{"type":"preload_done","to":7,"done":24,"total":24,"failed":0}',
    );
    assert.equal(done?.type, "preload_done");
    if (done?.type === "preload_done") {
      assert.equal(done.done, 24);
      assert.equal(done.failed, 0);
    }
  });
});

describe("applyCandleDelta", () => {
  const a: [number, number, number, number, number, number] = [1000, 1, 2, 0, 1, 5];
  const b: [number, number, number, number, number, number] = [2000, 1, 2, 0, 1, 5];

  it("appends a new bar", () => {
    assert.deepEqual(applyCandleDelta([a], { candle: b }), [a, b]);
  });

  it("replaces the current bar in place (no full rebuild)", () => {
    const updated: [number, number, number, number, number, number] = [1000, 1, 3, 0, 2, 7];
    assert.deepEqual(applyCandleDelta([a], { candle: updated }), [updated]);
  });

  it("ignores stale bars (Rust stays authoritative)", () => {
    const stale: [number, number, number, number, number, number] = [500, 1, 1, 1, 1, 1];
    assert.deepEqual(applyCandleDelta([a, b], { candle: stale }), [a, b]);
  });
});

describe("formatters", () => {
  it("formats epoch ms as UTC", () => {
    assert.equal(formatTs(1704067200000), "2024-01-01 00:00:00");
  });

  it("formats money or a placeholder", () => {
    assert.equal(formatMoney(1234.5), "1,234.50");
    assert.equal(formatMoney(Number.NaN), "—");
  });
});
