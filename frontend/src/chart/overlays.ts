// Pure overlay math for chart drawings (no DOM, no chart API).
// Pixel mapping (timeToCoordinate/priceToCoordinate) lives in Chart.tsx;
// everything here is unit-tested in test/overlays.test.ts.
import type { FillLine } from "../types/protocol";

export interface PxBox {
  left: number;
  top: number;
  width: number;
  height: number;
}

export interface TradeLink {
  entryTimestamp: number;
  entryPrice: number;
  exitTimestamp: number;
  exitPrice: number;
  direction: Direction;
}

/** Pair opposing fills FIFO into closed position legs, including partial exits and reversals. */
export function pairTradeFills(fills: FillLine[], symbol: string): TradeLink[] {
  const chronological = fills
    .filter((fill) => fill.symbol === symbol)
    .slice()
    .reverse()
    .sort((a, b) => a.timestamp - b.timestamp);
  const lots: Array<{ remaining: number; timestamp: number; price: number; direction: 1 | -1 }> = [];
  const links: TradeLink[] = [];
  for (const fill of chronological) {
    let remaining = fill.amount;
    const direction = fill.side === "buy" ? 1 : -1;
    while (remaining > 1e-10 && lots.length > 0 && lots[0].direction !== direction) {
      const lot = lots[0];
      const closed = Math.min(remaining, lot.remaining);
      links.push({
        entryTimestamp: lot.timestamp,
        entryPrice: lot.price,
        exitTimestamp: fill.timestamp,
        exitPrice: fill.price,
        direction: lot.direction > 0 ? "long" : "short",
      });
      lot.remaining -= closed;
      remaining -= closed;
      if (lot.remaining <= 1e-10) lots.shift();
    }
    if (remaining > 1e-10) lots.push({
      remaining,
      timestamp: fill.timestamp,
      price: fill.price,
      direction,
    });
  }
  return links;
}

/** Normalize two pixel corners into a box (handles any click order). */
export function toBox(x1: number, y1: number, x2: number, y2: number): PxBox {
  return {
    left: Math.min(x1, x2),
    top: Math.min(y1, y2),
    width: Math.abs(x2 - x1),
    height: Math.abs(y2 - y1),
  };
}

export type Direction = "long" | "short";

/** Long when the stop sits below entry, short otherwise (TradingView rule). */
export function positionDirection(entry: number, sl: number): Direction {
  return sl < entry ? "long" : "short";
}

/** Reward/risk ratio; null when the risk leg is degenerate. */
export function riskReward(entry: number, sl: number, tp: number): number | null {
  const risk = Math.abs(entry - sl);
  if (!(risk > 0)) return null;
  return Math.abs(tp - entry) / risk;
}

export interface Band {
  /** Fraction of the box height measured from the box top (0..1). */
  from: number;
  to: number;
  cls: "tp" | "sl" | "entry";
}

/**
 * Split a position box (top price, bottom price) into TP / entry / SL bands
 * as height fractions. Works for both directions.
 */
export function positionBands(top: number, bottom: number, entry: number, sl: number, tp: number): Band[] {
  const span = top - bottom;
  if (!(span > 0)) return [];
  const f = (p: number): number => Math.min(1, Math.max(0, (top - p) / span));
  const fe = f(entry);
  const fsl = f(sl);
  const ftp = f(tp);
  // Entry line sits between the two zones; zones span entry->tp and entry->sl.
  const hiEdge = Math.min(fe, fsl, ftp);
  const loEdge = Math.max(fe, fsl, ftp);
  const isTpAboveEntry = ftp < fe;
  const tpBand: Band = isTpAboveEntry
    ? { from: hiEdge, to: fe, cls: "tp" }
    : { from: fe, to: loEdge, cls: "tp" };
  const slBand: Band = isTpAboveEntry
    ? { from: fe, to: loEdge, cls: "sl" }
    : { from: hiEdge, to: fe, cls: "sl" };
  return [tpBand, { from: fe, to: fe, cls: "entry" }, slBand];
}

export interface RulerStats {
  pct: number;
  bars: number;
  pts: number;
}

/** Click-click measurement stats; msPerBar converts time distance to bars. */
export function rulerStats(
  a: { t: number; p: number },
  b: { t: number; p: number },
  msPerBar: number,
): RulerStats {
  const pct = a.p !== 0 ? ((b.p - a.p) / Math.abs(a.p)) * 100 : 0;
  const bars = Math.max(1, Math.round(Math.abs(b.t - a.t) / Math.max(1, msPerBar)));
  return { pct, bars, pts: b.p - a.p };
}

export function formatRuler(s: RulerStats): string {
  const sign = s.pct >= 0 ? "+" : "";
  const barsWord = s.bars === 1 ? "bar" : "bars";
  return `${sign}${s.pct.toFixed(2)}% · ${s.bars} ${barsWord} · ${s.pts >= 0 ? "+" : ""}${s.pts.toFixed(1)} pts`;
}

/** Russian plural for "bar" (chart hint uses timeframe bars). */
export function barWord(n: number): string {
  const m10 = n % 10;
  const m100 = n % 100;
  if (m10 === 1 && m100 !== 11) return "бар";
  if (m10 >= 2 && m10 <= 4 && (m100 < 12 || m100 > 14)) return "бара";
  return "баров";
}

/** How many bar timestamps fall inside [t1, t2] (ruler bar count). */
export function countBars(times: number[], t1: number, t2: number): number {
  const lo = Math.min(t1, t2);
  const hi = Math.max(t1, t2);
  let n = 0;
  for (const t of times) {
    if (t >= lo && t <= hi) n += 1;
  }
  return Math.max(n, 1);
}

/** Snap a timestamp to the nearest bar (drag magnet). */
export function snapTime(times: number[], ms: number): number {
  if (!times.length) return ms;
  let best = times[0];
  let bd = Math.abs(ms - best);
  for (const t of times) {
    const d = Math.abs(ms - t);
    if (d < bd) {
      bd = d;
      best = t;
    }
  }
  return best;
}

/**
 * Exact pixel->bar-time inversion: binary search over timeToCoordinate.
 * Unlike linear interpolation over the visible range this accounts for the
 * price-axis width and padding, so placed points render exactly where the
 * user clicked (no left-shift after finishing a drawing).
 */
export function invertTime(
  x: number,
  times: number[],
  toCoord: (ms: number) => number | null,
): number | null {
  if (!times.length) return null;
  const c0 = toCoord(times[0]);
  const cN = toCoord(times[times.length - 1]);
  if (c0 === null || cN === null) return null;
  const lo = Math.min(c0, cN);
  const hi = Math.max(c0, cN);
  if (x <= lo) return times[c0 <= cN ? 0 : times.length - 1];
  if (x >= hi) return times[c0 <= cN ? times.length - 1 : 0];
  // Ascending in time; coordinates ascend left->right.
  let a = 0;
  let b = times.length - 1;
  while (b - a > 1) {
    const m = (a + b) >> 1;
    const c = toCoord(times[m]);
    if (c === null) return null;
    if (c < x) a = m;
    else b = m;
  }
  const ca = toCoord(times[a]);
  const cb = toCoord(times[b]);
  if (ca === null || cb === null) return null;
  return Math.abs(ca - x) <= Math.abs(cb - x) ? times[a] : times[b];
}

/**
 * Compact money for axis labels: 8813493358.81 -> "8.81B".
 * Keeps the OI price axis narrow (shorter than price labels) so the
 * width-equalizing sync always converges on the main chart's stable width
 * instead of chasing the OI axis as data arrives.
 */
export function formatCompactUsdt(v: number): string {
  if (!Number.isFinite(v)) return "—";
  const sign = v < 0 ? "-" : "";
  const a = Math.abs(v);
  if (a >= 1e9) return `${sign}${(a / 1e9).toFixed(2)}B`;
  if (a >= 1e6) return `${sign}${(a / 1e6).toFixed(2)}M`;
  if (a >= 1e3) return `${sign}${(a / 1e3).toFixed(1)}K`;
  return `${sign}${a.toFixed(0)}`;
}

/**
 * Risk-based size, fees included.
 *
 * The trader's risk budget must cover the WHOLE cost of being wrong: the price
 * distance to the stop PLUS the round-trip commission. Sizing on price distance
 * alone ("risk / |entry - stop|") silently overspends the budget, because a
 * tight stop means a large notional and therefore a large fee: at a 1% stop on
 * BTC a $10 risk became a $1000 position paying ~$5 of fees, i.e. a 1.6R loss
 * on a "10 dollar" trade.
 *
 * With a taker fee of `feeRate` per side, one unit costs
 * `|entry - sl| + 2 * entry * feeRate`, so:
 *
 *     qty = risk / (|entry - sl| + 2 * entry * feeRate)
 *
 * `feeRate` is a fraction (0.0025 = 0.25%); null/0 means fees are ignored.
 * Null when inputs are unusable (missing/degenerate stop) — entry is blocked.
 */
export function calcQty(
  riskUsdt: number,
  entry: number,
  sl: number,
  feeRate = 0,
): number | null {
  if (!(riskUsdt > 0) || !Number.isFinite(entry) || !Number.isFinite(sl)) return null;
  const dist = Math.abs(entry - sl);
  if (!(dist > 0)) return null;
  const fee = Number.isFinite(feeRate) && feeRate > 0 ? feeRate : 0;
  const perUnit = dist + 2 * Math.abs(entry) * fee;
  const qty = riskUsdt / perUnit;
  return Number.isFinite(qty) && qty > 0 ? qty : null;
}

/**
 * What the trade costs if the stop is hit: the price distance plus the
 * round-trip commission, in USDT. Null when inputs are unusable.
 *
 * This is the number the trader must compare with `riskUsdt` — it is what the
 * diary's R is measured against, so anything else makes R a lie.
 */
export function riskAtStop(
  qty: number,
  entry: number,
  sl: number,
  feeRate = 0,
): number | null {
  if (!(qty > 0) || !Number.isFinite(entry) || !Number.isFinite(sl)) return null;
  const dist = Math.abs(entry - sl);
  if (!(dist > 0)) return null;
  const fee = Number.isFinite(feeRate) && feeRate > 0 ? feeRate : 0;
  return qty * (dist + 2 * Math.abs(entry) * fee);
}

export type OhlcRow = [number, number, number, number, number, number];

/** Map an arbitrary drawing timestamp onto the nearest candle in this pane. */
export function nearestCandleTime(times: number[], timestamp: number): number | null {
  if (times.length === 0 || !Number.isFinite(timestamp)) return null;
  let lo = 0;
  let hi = times.length - 1;
  while (lo < hi) {
    const mid = Math.floor((lo + hi) / 2);
    if (times[mid] < timestamp) lo = mid + 1;
    else hi = mid;
  }
  if (lo > 0 && timestamp - times[lo - 1] <= times[lo] - timestamp) lo -= 1;
  return times[lo];
}

/** Align sparse OI samples to candle timestamps using the last known value. */
export function alignOiToCandles(
  rows: OhlcRow[],
  samples: Array<[number, number]>,
): Array<[number, number]> {
  const ordered = samples
    .filter(([time, value]) => Number.isFinite(time) && Number.isFinite(value))
    .slice()
    .sort((a, b) => a[0] - b[0]);
  const out: Array<[number, number]> = [];
  let sampleIndex = 0;
  let current: number | null = null;
  for (const row of rows) {
    while (sampleIndex < ordered.length && ordered[sampleIndex][0] <= row[0]) {
      current = ordered[sampleIndex][1];
      sampleIndex += 1;
    }
    if (current !== null) out.push([row[0], current]);
  }
  return out;
}

export interface ProfileBarPosition {
  top: number;
  height: number;
}

/** Project each volume level through the chart's current price scale. */
export function profileBarLayout(
  prices: number[],
  toY: (price: number) => number | null,
  plotHeight?: number,
): Array<ProfileBarPosition | null> {
  const ys = prices.map(toY);
  return ys.map((y, i) => {
    if (y === null || !Number.isFinite(y)) return null;
    const maxY = plotHeight === undefined ? Infinity : Math.max(0, plotHeight - 26);
    if (y < 0 || y > maxY) return null;
    const previous = ys[i - 1];
    const next = ys[i + 1];
    const gaps = [
      previous !== undefined && previous !== null ? Math.abs(y - previous) : 0,
      next !== undefined && next !== null ? Math.abs(next - y) : 0,
    ].filter((gap) => gap > 0 && Number.isFinite(gap));
    const height = gaps.length ? gaps.reduce((sum, gap) => sum + gap, 0) / gaps.length : 1;
    const top = Math.max(0, y - height / 2);
    const bottom = Math.min(maxY, y + height / 2);
    return bottom > top ? { top, height: bottom - top } : null;
  });
}

/**
 * Cumulative volume delta over the whole series: +volume on up bars
 * (close >= open), -volume on down bars. Recomputed per tick; 10k bars
 * is far below a millisecond, so no incremental bookkeeping.
 */
export function cumulativeDelta(rows: OhlcRow[]): number[] {
  const out: number[] = new Array(rows.length);
  let acc = 0;
  for (let i = 0; i < rows.length; i++) {
    const r = rows[i];
    acc += r[4] >= r[1] ? r[5] : -r[5];
    out[i] = acc;
  }
  return out;
}

export interface ProfileRow {
  /** Level mid price. */
  price: number;
  /** Volume assigned to the level. */
  volume: number;
  /** Point of control (max-volume level). */
  poc: boolean;
}

/**
 * Fixed-range volume profile over bar indices [fromIdx, toIdx] (inclusive,
 * clamped, order-insensitive): splits the visible high–low range into
 * `levels` rows and spreads each bar's volume across overlapped rows by
 * overlap fraction. Returns rows top (high) → bottom (low), POC flagged.
 * Pure — the chart calls it on every pan/zoom/data tick with the visible
 * logical range, so the profile always matches the bars on screen.
 */
export function volumeProfile(
  rows: OhlcRow[],
  fromIdx: number,
  toIdx: number,
  levels = 24,
): { rows: ProfileRow[]; poc: number | null } {
  const n = rows.length;
  if (!n || levels < 1) return { rows: [], poc: null };
  const a = Math.max(0, Math.min(n - 1, Math.floor(fromIdx)));
  const b = Math.max(0, Math.min(n - 1, Math.ceil(toIdx)));
  const lo = Math.min(a, b);
  const hi = Math.max(a, b);
  let mn = Infinity;
  let mx = -Infinity;
  for (let i = lo; i <= hi; i++) {
    const r = rows[i];
    if (r[3] < mn) mn = r[3];
    if (r[2] > mx) mx = r[2];
  }
  if (!Number.isFinite(mn) || !(mx > mn)) {
    const p = Number.isFinite(mn) ? mn : 0;
    let v = 0;
    for (let i = lo; i <= hi; i++) v += rows[i][5];
    return { rows: [{ price: p, volume: v, poc: true }], poc: p };
  }
  const vols: number[] = new Array(levels).fill(0);
  const h = (mx - mn) / levels;
  for (let i = lo; i <= hi; i++) {
    const r = rows[i];
    const span = r[2] - r[3];
    if (!(span > 0)) {
      const k = Math.min(levels - 1, Math.max(0, Math.floor((r[4] - mn) / h)));
      vols[k] += r[5];
      continue;
    }
    const k0 = Math.max(0, Math.floor((r[3] - mn) / h));
    const k1 = Math.min(levels - 1, Math.floor((r[2] - mn) / h));
    for (let k = k0; k <= k1; k++) {
      const cellLo = mn + k * h;
      const ov = Math.min(r[2], cellLo + h) - Math.max(r[3], cellLo);
      if (ov > 0) vols[k] += (r[5] * ov) / span;
    }
  }
  let pocK = 0;
  for (let k = 1; k < levels; k++) {
    if (vols[k] > vols[pocK]) pocK = k;
  }
  const out: ProfileRow[] = [];
  for (let k = levels - 1; k >= 0; k--) {
    out.push({ price: mn + (k + 0.5) * h, volume: vols[k], poc: k === pocK });
  }
  return { rows: out, poc: mn + (pocK + 0.5) * h };
}
