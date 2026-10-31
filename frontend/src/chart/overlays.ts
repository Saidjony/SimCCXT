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
  /** Order that opened the leg: its `entry_protections` carry the planned stop. */
  entryOrderId: string;
}

/**
 * The engine's own sequence for a fill: `orderId` (the matching engine hands
 * them out in creation order) and the per-order fill index encoded in `id` as
 * `orderId_index`.
 */
function fillSequence(fill: FillLine): [number, number] {
  const order = Number.parseInt(fill.orderId, 10);
  const index = Number.parseInt(fill.id.split("_").pop() ?? "", 10);
  return [
    Number.isFinite(order) ? order : Number.MAX_SAFE_INTEGER,
    Number.isFinite(index) ? index : 0,
  ];
}

/**
 * Chronological order of fills.
 *
 * The timestamp alone is not a total order: an entry and its exit can land on
 * the same millisecond (close inside the entry bar, a stop hit by the entry
 * candle), and the store keeps fills newest-first with an arbitrary order for
 * such ties. Relying on the array order then reversed the trade: a BUY entry
 * was paired as the exit, so a long was drawn and labelled as a short. The
 * engine's sequence breaks the tie deterministically.
 */
export function fillChronology(a: FillLine, b: FillLine): number {
  if (a.timestamp !== b.timestamp) return a.timestamp - b.timestamp;
  const [aOrder, aIndex] = fillSequence(a);
  const [bOrder, bIndex] = fillSequence(b);
  return aOrder - bOrder || aIndex - bIndex;
}

/** Pair opposing fills FIFO into closed position legs, including partial exits and reversals. */
export function pairTradeFills(fills: FillLine[], symbol: string): TradeLink[] {
  const chronological = fills
    .filter((fill) => fill.symbol === symbol)
    .slice()
    .sort(fillChronology);
  const lots: Array<{
    remaining: number;
    timestamp: number;
    price: number;
    direction: 1 | -1;
    orderId: string;
  }> = [];
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
        entryOrderId: lot.orderId,
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
      orderId: fill.orderId,
    });
  }
  return links;
}

/** Realized result of one closed leg, in price, percent and (when the stop is known) R. */
export interface TradeOutcome {
  /** Favorable-minus-adverse price move; positive = profit. */
  move: number;
  /** Move as a percentage of the entry price. */
  pct: number;
  /** Move divided by the planned risk; null when there was no usable stop. */
  r: number | null;
  profitable: boolean;
}

export function tradeOutcome(link: TradeLink, stopPrice: number | null): TradeOutcome {
  const long = link.direction === "long";
  const move = long ? link.exitPrice - link.entryPrice : link.entryPrice - link.exitPrice;
  const pct = link.entryPrice > 0 ? (move / link.entryPrice) * 100 : 0;
  let r: number | null = null;
  if (stopPrice !== null && stopPrice > 0) {
    const risk = long ? link.entryPrice - stopPrice : stopPrice - link.entryPrice;
    if (risk > 0) r = move / risk;
  }
  return { move, pct, r, profitable: move > 0 };
}

/**
 * A closed trade may only be drawn once the bars it happened on are on the
 * chart. Persisted fills survive a reload, but the replay starts over at the
 * window's reveal boundary — without this check every old trade from "the
 * future" would pile up on the last visible bar.
 */
export function tradeVisible(
  link: Pick<TradeLink, "entryTimestamp" | "exitTimestamp">,
  times: ReadonlyArray<number>,
): boolean {
  if (times.length === 0) return false;
  const first = times[0];
  const last = times[times.length - 1];
  return link.entryTimestamp >= first
    && link.exitTimestamp >= first
    && link.entryTimestamp <= last
    && link.exitTimestamp <= last;
}

/**
 * The two price legs of a closed trade, as TradingView's position tool draws
 * them: a reward leg above the entry (long) and a risk leg below it.
 *
 * The reward leg is the planned take-profit when there was one, otherwise the
 * realized exit if the trade ended in profit. Without a target the leg stays
 * empty rather than inventing a level — a stop-out with no TP has nothing on
 * the profit side to show.
 */
export interface TradeZones {
  /** Price bounding the profit zone (target, or the profitable exit). */
  rewardPrice: number | null;
  /** Price bounding the risk zone (stop, extended to a losing exit). */
  adversePrice: number | null;
}

export function tradeZones(
  link: TradeLink,
  stopPrice: number | null,
  takePrice: number | null,
): TradeZones {
  const long = link.direction === "long";
  const favorable = (price: number): boolean =>
    long ? price > link.entryPrice : price < link.entryPrice;
  /** The more favourable of two prices (higher for a long). */
  const farther = (a: number, b: number): number => (long ? Math.max(a, b) : Math.min(a, b));
  /** The more adverse of two prices (lower for a long). */
  const deeper = (a: number, b: number): number => (long ? Math.min(a, b) : Math.max(a, b));

  let rewardPrice: number | null = null;
  if (takePrice !== null && takePrice > 0 && favorable(takePrice)) rewardPrice = takePrice;
  if (favorable(link.exitPrice)) {
    rewardPrice = rewardPrice === null ? link.exitPrice : farther(rewardPrice, link.exitPrice);
  }

  let adversePrice: number | null = null;
  if (stopPrice !== null && stopPrice > 0 && !favorable(stopPrice)) adversePrice = stopPrice;
  if (!favorable(link.exitPrice) && link.exitPrice !== link.entryPrice) {
    adversePrice = adversePrice === null
      ? link.exitPrice
      : deeper(adversePrice, link.exitPrice);
  }

  return { rewardPrice, adversePrice };
}

/** Pixel box of one closed trade: time span x, entry/exit/stop/target y. */
export interface TradeBoxShape {
  left: number;
  top: number;
  width: number;
  height: number;
  entryY: number;
  exitY: number;
  stopY: number | null;
  rewardY: number | null;
}

/**
 * TradingView-style position box: the rectangle spans the trade's lifetime and
 * covers entry, exit and (when known) the planned stop and target, so every
 * level is visible at once instead of a single diagonal entry→exit line.
 */
export function tradeBoxShape(
  x1: number,
  entryY: number,
  x2: number,
  exitY: number,
  stopY: number | null,
  rewardY: number | null = null,
): TradeBoxShape {
  const left = Math.min(x1, x2);
  // Same-bar trades would otherwise be invisible: keep a sliver of width.
  const width = Math.max(Math.abs(x2 - x1), 3);
  const ys = [entryY, exitY];
  if (stopY !== null) ys.push(stopY);
  if (rewardY !== null) ys.push(rewardY);
  const top = Math.min(...ys);
  const bottom = Math.max(...ys);
  return { left, top, width, height: bottom - top, entryY, exitY, stopY, rewardY };
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

/**
 * Tick size for a price axis, derived from the price's own magnitude.
 *
 * lightweight-charts defaults `minMove` to 0.01, so on a 0.09 DOGE/USDT the
 * axis can only step by whole cents: the labels read 0.08 / 0.09, the real
 * price is invisible and the order ticket becomes unusable. The rule is
 * "keep two decimals below the leading digit":
 *
 *   0.09 → 0.0001   0.9 → 0.001   8 → 0.01   80 → 0.1   80 000 → 1
 *
 * so every pair gets a readable axis — BTC keeps whole-unit ticks, DOGE gets
 * four decimals. It is the SMALLEST allowed step; the library still picks a
 * tidy multiple of it to suit the visible range.
 */
export function priceTick(price: number): number {
  if (!Number.isFinite(price) || price <= 0) return 0.01;
  const decimals = Math.max(0, Math.min(12, 2 - Math.floor(Math.log10(price))));
  return decimals === 0 ? 1 : Math.pow(10, -decimals);
}

/** Precision + minMove pair for a price format, adapting to the visible zoom. */
export function priceAxisFormat(
  price: number,
  visibleRange?: { from: number; to: number } | null,
): { precision: number; minMove: number } {
  let minMove = priceTick(price);
  if (visibleRange) {
    const span = Math.abs(visibleRange.to - visibleRange.from);
    if (Number.isFinite(span) && span > 0) {
      const zoomStep = Math.pow(10, Math.floor(Math.log10(span / 100)));
      if (Number.isFinite(zoomStep) && zoomStep > 0) minMove = Math.min(minMove, zoomStep);
    }
  }
  const precision =
    minMove >= 1 ? 0 : Math.max(0, Math.min(12, Math.round(-Math.log10(minMove))));
  return { precision, minMove };
}

/** Formats candle OHLC without discarding the precision present in the data. */
export function formatPriceValue(value: number): string {
  const [significand, exponentText] = value.toString().toLowerCase().split("e");
  const decimals = Math.max(
    0,
    Math.min(12, (significand.split(".")[1]?.length ?? 0) - Number(exponentText ?? 0)),
  );
  return value.toFixed(decimals);
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
  if (ms <= times[0]) return times[0];
  const last = times[times.length - 1];
  if (ms >= last) return last;
  let lo = 0;
  let hi = times.length - 1;
  while (hi - lo > 1) {
    const mid = (lo + hi) >> 1;
    if (times[mid] <= ms) lo = mid;
    else hi = mid;
  }
  return ms - times[lo] <= times[hi] - ms ? times[lo] : times[hi];
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
 * `|entry - sl| + (|entry| + |sl|) * feeRate`, so:
 *
 *     qty = risk / (|entry - sl| + (|entry| + |sl|) * feeRate)
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
  const perUnit = dist + (Math.abs(entry) + Math.abs(sl)) * fee;
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
  return qty * (dist + (Math.abs(entry) + Math.abs(sl)) * fee);
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
 * (close >= open), -volume on down bars. Used when a chart loads or resets;
 * live updates adjust only the final cumulative point.
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
