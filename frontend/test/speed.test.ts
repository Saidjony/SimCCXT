// Replay speed ladder + the hotkey that walks it.
// Pure functions — run with `npm test` (node --test).
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  DEFAULT_SPEED,
  REPLAY_SPEEDS,
  SPEED_DOWN_CODES,
  SPEED_UP_CODES,
  nextSpeed,
} from "../src/replay/speed.ts";

describe("nextSpeed", () => {
  it("walks the ladder one rung at a time", () => {
    assert.equal(nextSpeed(1, 1), 2);
    assert.equal(nextSpeed(2, 1), 4);
    assert.equal(nextSpeed(2, -1), 1);
    assert.equal(nextSpeed(DEFAULT_SPEED, -1), 0.5);
  });

  it("clamps at the ends instead of wrapping", () => {
    // Pressing "slower" at the slowest speed must not jump to 10×.
    assert.equal(nextSpeed(REPLAY_SPEEDS[0], -1), REPLAY_SPEEDS[0]);
    assert.equal(nextSpeed(REPLAY_SPEEDS[REPLAY_SPEEDS.length - 1], 1), 10);
  });

  it("reaches the top by repeated presses", () => {
    let speed = 1;
    for (let i = 0; i < 10; i++) speed = nextSpeed(speed, 1);
    assert.equal(speed, 10);
  });

  it("picks the nearest rung when the speed is off-ladder", () => {
    // The server may report a speed we do not offer; the hotkey still has to
    // land somewhere sensible rather than resetting to 1.
    assert.equal(nextSpeed(3, 1), 4, "up from between 2 and 4");
    assert.equal(nextSpeed(3, -1), 2, "down from between 2 and 4");
    assert.equal(nextSpeed(0.1, -1), REPLAY_SPEEDS[0], "below the floor");
    assert.equal(nextSpeed(100, 1), 10, "above the ceiling");
  });

  it("never returns a speed outside the ladder", () => {
    for (const off of [0, 0.25, 1.5, 3, 7, 42]) {
      for (const dir of [1, -1] as const) {
        assert.ok(
          REPLAY_SPEEDS.includes(nextSpeed(off, dir)),
          `${off} ${dir > 0 ? "up" : "down"} → ${nextSpeed(off, dir)}`,
        );
      }
    }
  });
});

describe("speed hotkeys", () => {
  it("matches on the physical key, so any layout works", () => {
    // e.code is layout-independent: on a Russian keyboard BracketLeft types
    // "х", which a key === "[" test would never see.
    assert.ok(SPEED_UP_CODES.has("BracketRight"));
    assert.ok(SPEED_DOWN_CODES.has("BracketLeft"));
    assert.ok(SPEED_UP_CODES.has("NumpadAdd"));
    assert.ok(SPEED_DOWN_CODES.has("NumpadSubtract"));
  });

  it("keeps the two directions disjoint", () => {
    for (const code of SPEED_UP_CODES) {
      assert.ok(!SPEED_DOWN_CODES.has(code), `${code} cannot be both`);
    }
    assert.ok(SPEED_UP_CODES.size > 0 && SPEED_DOWN_CODES.size > 0);
  });
});
