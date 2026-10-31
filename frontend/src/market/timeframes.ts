// Timeframes of this terminal: 5m is the only interval downloaded, 1h and 1d
// are aggregated from it in the browser (see ./aggregate.ts and ./provider.ts).
//
// 1m used to exist as a selectable interval. It is gone on purpose: a 60-day 1m
// window is 86 400 bars, 2 880 HTTP requests and ~15 MB just to open one chart,
// and every extra interval multiplied that. Three intervals, one download.

export const TIMEFRAMES = ["5m", "1h", "1d"] as const;

export type Timeframe = (typeof TIMEFRAMES)[number];

/** The interval every other one is aggregated from. */
export const BASE_TIMEFRAME: Timeframe = "5m";

export const TIMEFRAME_MS: Record<Timeframe, number> = {
  "5m": 300_000,
  "1h": 3_600_000,
  "1d": 86_400_000,
};

/** Chart panes draw this many base bars per aggregated bar. */
export const AGGREGATION_FACTOR: Record<Timeframe, number> = {
  "5m": 1,
  "1h": 12,
  "1d": 288,
};

export function isTimeframe(value: unknown): value is Timeframe {
  return typeof value === "string" && (TIMEFRAMES as readonly string[]).includes(value);
}

/** Coerce anything (stored preference, server echo, URL) to a valid timeframe. */
export function timeframeOr(value: unknown, fallback: Timeframe): Timeframe {
  return isTimeframe(value) ? value : fallback;
}

/** Base bars per aggregated bar; 1 for the base interval itself. */
export function factorOf(timeframe: Timeframe): number {
  return AGGREGATION_FACTOR[timeframe];
}
