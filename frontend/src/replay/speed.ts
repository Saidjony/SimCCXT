// Replay speed ladder — one source of truth for the selector AND the hotkeys.
//
// Pure (no DOM, no store) so `node --test` can cover it: see test/speed.test.ts.

/** The speeds the replay can run at, slowest first. */
export const REPLAY_SPEEDS: readonly number[] = [0.5, 1, 2, 4, 10];

/** The speed the replay starts at when the server has not said otherwise. */
export const DEFAULT_SPEED = 1;

/**
 * The speed one step in `dir` from `current`.
 *
 * Falls back to the nearest rung when `current` is not on the ladder (a server
 * may report something we do not offer), and clamps at the ends instead of
 * wrapping — pressing "slower" at 0.5× must not jump to 10×.
 */
export function nextSpeed(current: number, dir: 1 | -1): number {
  const exact = REPLAY_SPEEDS.indexOf(current);
  if (exact >= 0) {
    const step = exact + dir;
    if (step < 0) return REPLAY_SPEEDS[0];
    if (step >= REPLAY_SPEEDS.length) return REPLAY_SPEEDS[REPLAY_SPEEDS.length - 1];
    return REPLAY_SPEEDS[step];
  }
  // Off-ladder: pick the next rung strictly past `current` in this direction.
  const candidates = REPLAY_SPEEDS.filter((s) => (dir > 0 ? s > current : s < current));
  if (candidates.length === 0) {
    return dir > 0 ? REPLAY_SPEEDS[REPLAY_SPEEDS.length - 1] : REPLAY_SPEEDS[0];
  }
  return dir > 0 ? candidates[0] : candidates[candidates.length - 1];
}

/** Keyboard codes that change the speed, whatever the keyboard layout. */
export const SPEED_UP_CODES: ReadonlySet<string> = new Set([
  "BracketRight", // ] (US) / ъ (RU) — layout-independent
  "Equal",
  "NumpadAdd",
]);

export const SPEED_DOWN_CODES: ReadonlySet<string> = new Set([
  "BracketLeft", // [ (US) / х (RU)
  "Minus",
  "NumpadSubtract",
]);
