import type { CandleRow } from "../types/protocol.ts";
import { BASE_TIMEFRAME, TIMEFRAME_MS, type Timeframe, factorOf } from "./timeframes.ts";

/**
 * Aggregate base (5m) bars into `timeframe` buckets.
 *
 * This is the ONE aggregation the terminal uses: the replay engine, the chart
 * panes and the open-interest pane all go through here, so a 1h bar on the main
 * chart is byte-identical to the 1h bar on a lower pane.
 *
 * Buckets align to absolute multiples of the target step (UTC days for 1d),
 * exactly like exchange klines: the bucket's time is its OPEN time.
 *
 * Called on empty input it returns [] — never the input itself. (Folding an
 * empty array used to hand back the source rows, which would stamp 5m bars with
 * 288-base-bar scaling on a 1d chart.)
 */
export function aggregateCandles(base: readonly CandleRow[], timeframe: Timeframe): CandleRow[] {
  const step = TIMEFRAME_MS[timeframe];
  if (base.length === 0) return [];
  if (timeframe === BASE_TIMEFRAME) return base.slice();

  const out: CandleRow[] = [];
  let bucketStart = -1;
  let open = 0;
  let high = 0;
  let low = 0;
  let close = 0;
  let volume = 0;
  for (const row of base) {
    const bucket = Math.floor(row[0] / step) * step;
    if (bucket !== bucketStart) {
      if (bucketStart >= 0) out.push([bucketStart, open, high, low, close, volume]);
      bucketStart = bucket;
      open = row[1];
      high = row[2];
      low = row[3];
      close = row[4];
      volume = row[5];
    } else {
      if (row[2] > high) high = row[2];
      if (row[3] < low) low = row[3];
      close = row[4];
      volume += row[5];
    }
  }
  if (bucketStart >= 0) out.push([bucketStart, open, high, low, close, volume]);
  return out;
}

/** Slice base bars to exactly where the replay clock is: `fed` bars visible. */
export function visibleBase(base: readonly CandleRow[], fed: number): CandleRow[] {
  if (fed <= 0) return [];
  if (fed >= base.length) return base.slice();
  return base.slice(0, fed);
}

/**
 * The bars a chart of `timeframe` shows when the replay has revealed `fed` of
 * `total` base bars.
 *
 * Both bar counts are derived from the SAME `fed` count, so the main chart and
 * every pane always agree on how far the replay has gone.
 */
export function barsFor(
  base: readonly CandleRow[],
  timeframe: Timeframe,
  fed: number,
): CandleRow[] {
  return aggregateCandles(visibleBase(base, fed), timeframe);
}

/**
 * Aggregate an open-interest series into `timeframe` buckets for the chart.
 *
 * OI samples arrive on a grid the venue has (5m, 15m, 1h, ...), not
 * necessarily ours. For a target of 1h or 1d each bucket takes the LAST sample
 * inside it — the value the market had by the end of that hour/day — and
 * buckets with no sample are skipped, never filled or scaled: an eight-hour gap
 * must stay an eight-hour gap (multiplying samples by bars-per-bucket turns one
 * 5m observation into 288 hours' worth).
 */
export function aggregateOi(
  samples: ReadonlyArray<readonly [number, number]>,
  timeframe: Timeframe,
): Array<[number, number]> {
  if (samples.length === 0) return [];
  const step = TIMEFRAME_MS[timeframe];
  const out: Array<[number, number]> = [];
  let bucketStart = Number.NaN;
  for (const [ts, value] of samples) {
    const bucket = Math.floor(ts / step) * step;
    if (bucket === bucketStart) {
      out[out.length - 1] = [bucket, value];
    } else {
      bucketStart = bucket;
      out.push([bucket, value]);
    }
  }
  return out;
}

/** How many base bars one chart bar covers (for labels: "1 bar = 12 × 5m"). */
export function baseBarsPerChartBar(timeframe: Timeframe): number {
  return factorOf(timeframe);
}
