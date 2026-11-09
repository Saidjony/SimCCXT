// Restoring the trade history persisted in the browser after a reload.
// Run with `npm test` (node --test).
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  fillIdentity,
  groupHistory,
  mapsForEngine,
  tagEngineFills,
} from "../src/replay/historyRestore.ts";
import type { FillLine } from "../src/types/protocol.ts";

function fill(overrides: Partial<FillLine> & Pick<FillLine, "id" | "timestamp">): FillLine {
  return {
    symbol: "BTC/USDT",
    side: "buy",
    type: "market",
    price: 100,
    amount: 1,
    cost: 100,
    fee: 0,
    orderId: "1",
    datetime: "2026-01-01T00:00:00Z",
    maker: false,
    ...overrides,
  };
}

const protection = (stop: number) => ({ stop_price: stop, take_price: stop + 10 });

describe("restoring persisted trade history", () => {
  it("tags fresh fills with the engine that produced them", () => {
    const tagged = tagEngineFills([
      fill({ id: "1_0", timestamp: 1 }),
      fill({ id: "2_0", timestamp: 2, engine_id: "already" }),
    ], "live");
    assert.equal(tagged[0].engine_id, "live");
    assert.equal(tagged[1].engine_id, "already");
  });

  it("groups fills by the engine instance that produced them", () => {
    const live = fill({ id: "1_0", timestamp: 5, engine_id: "live" });
    const oldA = fill({ id: "1_0", timestamp: 100, engine_id: "page-a" });
    const oldB = fill({ id: "2_0", timestamp: 200, engine_id: "page-a" });
    const groups = groupHistory(
      [live, oldB, oldA],
      new Set([fillIdentity(live)]),
      "live",
      "legacy",
      null,
      {},
      {},
    );
    assert.equal(groups.length, 1);
    assert.equal(groups[0].engineId, "page-a");
    assert.deepEqual(
      groups[0].fills.map((item) => item.timestamp),
      [100, 200],
      "the server reconstructs oldest-first",
    );
  });

  it("keeps legacy fills (no engine id) under one stable id", () => {
    const groups = groupHistory(
      [fill({ id: "1_0", timestamp: 10 }), fill({ id: "2_0", timestamp: 20 })],
      new Set(),
      "live",
      "legacy-id",
      null,
      {},
      {},
    );
    assert.equal(groups.length, 1);
    assert.equal(groups[0].engineId, "legacy-id");
    assert.equal(groups[0].fills.length, 2);
  });

  it("does not restore the live engine's own fills or a cleared symbol", () => {
    const live = fill({ id: "1_0", timestamp: 1, engine_id: "live" });
    const gone = fill({ id: "2_0", timestamp: 2, engine_id: "old", symbol: "ETH/USDT" });
    const kept = fill({ id: "3_0", timestamp: 3, engine_id: "old" });
    const groups = groupHistory(
      [live, gone, kept],
      new Set([fillIdentity(live)]),
      "live",
      "legacy",
      "ETH/USDT",
      {},
      {},
    );
    assert.deepEqual(groups.map((group) => group.engineId), ["old"]);
    assert.deepEqual(groups[0].fills.map((item) => item.id), ["3_0"]);
  });

  it("re-keys legacy plans but leaves tagged engine plans alone", () => {
    const protections = {
      "live|BTC/USDT|1|1": protection(90),
      "page-a|BTC/USDT|1|100": protection(80),
      "BTC/USDT|1|5": protection(70),
    };
    const tagged = mapsForEngine(protections, "page-a", "legacy", new Set(["page-a"]), "live");
    assert.deepEqual(Object.keys(tagged), ["page-a|BTC/USDT|1|100"]);

    const legacy = mapsForEngine(protections, "legacy", "legacy", new Set(["page-a"]), "live");
    assert.deepEqual(
      Object.keys(legacy),
      ["legacy|BTC/USDT|1|5"],
      "the live plan is mirrored by the live sync; the tagged plan by its own group",
    );
  });

  it("keeps exit verdicts with the matching engine group", () => {
    const exits = {
      "live|BTC/USDT|2|1": "stop",
      "page-a|BTC/USDT|2|100": "take",
      "BTC/USDT|2|5": "stop",
    };
    assert.deepEqual(
      mapsForEngine(exits, "page-a", "legacy", new Set(["page-a"]), "live"),
      { "page-a|BTC/USDT|2|100": "take" },
    );
    assert.deepEqual(
      mapsForEngine(exits, "legacy", "legacy", new Set(["page-a"]), "live"),
      { "legacy|BTC/USDT|2|5": "stop" },
    );
  });
});
