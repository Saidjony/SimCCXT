// Pure helpers for re-mirroring the trade history persisted in the browser.
//
// A closed tab loses its session, so the server starts a new account with no
// fills while the Trades table is rebuilt from localStorage. These helpers
// group those persisted fills by the engine instance that produced them and
// re-key legacy protective levels; `localEngine.ts` only handles the sync I/O.

import type { FillLine } from "../types/protocol";

/** Identity of one execution inside the browser's history. */
export function fillIdentity(fill: FillLine): string {
  return `${fill.id}|${fill.symbol}|${fill.timestamp}|${fill.price}|${fill.amount}`;
}

/**
 * Stamp a freshly produced batch with the engine instance that owns it before
 * it enters the browser's persisted history. The server does the same for its
 * mirror; without the browser copy a reload cannot tell which page load
 * produced a fill and would have to lump every past session together.
 */
export function tagEngineFills(fills: readonly FillLine[], engineId: string): FillLine[] {
  return fills.map((fill) => (
    fill.engine_id && fill.engine_id.length > 0 ? fill : { ...fill, engine_id: engineId }
  ));
}

export interface HistoryGroup {
  engineId: string;
  fills: FillLine[];
  entryProtections: Record<string, Record<string, unknown>>;
  exitReasons: Record<string, string>;
}

/** Engine id prefix of a `engine|symbol|orderId|timestamp` map key. */
function keyEngine(key: string): string {
  const parts = key.split("|");
  return parts.length >= 4 ? parts[0] : "";
}

/** Last three parts of a `engine|symbol|orderId|timestamp` key. */
function keySuffix(key: string): string {
  return key.split("|").slice(-3).join("|");
}

/**
 * Rebuild one engine group's protections/exit reasons.
 *
 * Keys written by an engine already carry its id, so a tagged group uses them
 * as-is. Legacy fills (persisted before fills were tagged) have no id; their
 * maps are re-keyed to the stable legacy id so the diary can match them.
 */
export function mapsForEngine<T>(
  map: Record<string, T>,
  groupEngine: string,
  legacyEngine: string,
  taggedEngines: ReadonlySet<string>,
  liveEngine: string,
): Record<string, T> {
  const out: Record<string, T> = {};
  for (const [key, value] of Object.entries(map)) {
    const engine = keyEngine(key);
    if (groupEngine === legacyEngine) {
      // A key owned by a real engine group travels with that group; the live
      // engine's keys travel with the live sync. Only the rest is legacy.
      if ((liveEngine.length > 0 && engine === liveEngine) || taggedEngines.has(engine)) {
        continue;
      }
      out[`${groupEngine}|${keySuffix(key)}`] = value;
    } else if (engine === groupEngine) {
      out[key] = value;
    }
  }
  return out;
}

/**
 * Fills this tab did NOT produce, grouped by the engine instance that did.
 *
 * `fills` is the browser's persisted history (newest first). A fill produced
 * by the live engine is in `seen` and travels with the normal sync; everything
 * else belonged to an earlier page load and has to be restored explicitly.
 * Order ids restart at 1 per engine, so groups must stay separate or FIFO
 * would pair one page's entry with another page's exit.
 */
export function groupHistory(
  fills: readonly FillLine[],
  seen: ReadonlySet<string>,
  liveEngine: string,
  legacyEngine: string,
  clearSymbol: string | null,
  entryProtections: Record<string, Record<string, unknown>>,
  exitReasons: Record<string, string>,
): HistoryGroup[] {
  const byEngine = new Map<string, FillLine[]>();
  for (const fill of fills) {
    if (seen.has(fillIdentity(fill))) continue;
    if (clearSymbol !== null && fill.symbol === clearSymbol) continue;
    const id = fill.engine_id && fill.engine_id.length > 0 ? fill.engine_id : legacyEngine;
    const group = byEngine.get(id);
    if (group) group.push(fill);
    else byEngine.set(id, [fill]);
  }
  if (byEngine.size === 0) return [];
  const taggedEngines = new Set(byEngine.keys());
  return [...byEngine].map(([groupEngine, groupFills]) => ({
    engineId: groupEngine,
    // The server reconstructs oldest-first; the browser stores newest-first.
    fills: groupFills.slice().reverse(),
    entryProtections: mapsForEngine(
      entryProtections,
      groupEngine,
      legacyEngine,
      taggedEngines,
      liveEngine,
    ),
    exitReasons: mapsForEngine(
      exitReasons,
      groupEngine,
      legacyEngine,
      taggedEngines,
      liveEngine,
    ),
  }));
}
