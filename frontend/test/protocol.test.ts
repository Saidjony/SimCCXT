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
