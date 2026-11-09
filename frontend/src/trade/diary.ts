// Pure view-model for the read-only trader diary page.
// No DOM, no network, no lightweight-charts: everything here is plain data so
// `node --test` can exercise it (test/diary.test.ts). The React page
// (components/TraderDiary.tsx) only renders what this module computes.

import type {
  DiaryData,
  DiaryPosition,
  DiaryStats,
  DiaryTrade,
} from "../types/protocol";

/** Nick from `?trader=...`, percent-decoded; null/empty → null (no diary). */
export function traderFromSearch(search: string): string | null {
  const raw = new URLSearchParams(search).get("trader");
  const nick = (raw ?? "").trim();
  return nick === "" ? null : nick;
}

/** A chart marker for one trade event, in the shape the page maps 1:1. */
export interface DiaryMarker {
  /** Bar timestamp in ms (already snapped to a candle by the caller). */
  time_ms: number;
  side: "long" | "short";
  /** "entry" marks the fill that opened the lot, "exit" the closing fill. */
  kind: "entry" | "exit";
  price: number;
  /** Label shown next to the arrow, e.g. "ВХОД 0.5 · 64120.00". */
  text: string;
  /** Owning trade key (`entry_order_id-exit_order_id`) so row→marker coloring works. */
  ref: string;
}

/** Round a price for display: 2 decimals above 1, else 6 significant. */
export function fmtPrice(p: number): string {
  if (!Number.isFinite(p)) return "—";
  return Math.abs(p) >= 1 ? p.toFixed(2) : p.toPrecision(6);
}

/**
 * Short UTC stamp for chart labels: "MM-DD HH:MM". Chart time axes are UTC,
 * so a local stamp here would disagree with the axis the trader reads.
 * (Mirrors `formatTs` in types/protocol.ts, which cannot be imported here:
 * that module pulls runtime imports node --test cannot resolve.)
 */
export function fmtChartTime(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "";
  const d = new Date(ms);
  const p = (n: number): string => (n < 10 ? "0" + n : "" + n);
  return (
    p(d.getUTCMonth() + 1) + "-" + p(d.getUTCDate()) + " " +
    p(d.getUTCHours()) + ":" + p(d.getUTCMinutes())
  );
}

/**
 * How long a trade was held, in trader-readable units: "45с", "12м 30с",
 * "3ч 05м", "2д 4ч". At most two units are shown; a zero remainder is
 * dropped ("5м", not "5м 0с"). Never negative — a broken/absent stamp reads
 * "—" instead of a made-up duration.
 */
export function fmtDuration(entryMs: number, exitMs: number): string {
  if (
    !Number.isFinite(entryMs) ||
    !Number.isFinite(exitMs) ||
    entryMs <= 0 ||
    exitMs < entryMs
  ) {
    return "—";
  }
  // Round to whole seconds first, so 59.7 s reads "1м" rather than "59с".
  const total = Math.round((exitMs - entryMs) / 1000);
  if (total < 1) return "<1с";
  const d = Math.floor(total / 86_400);
  const h = Math.floor((total % 86_400) / 3_600);
  const m = Math.floor((total % 3_600) / 60);
  const s = total % 60;
  if (d > 0) return h > 0 ? `${d}д ${h}ч` : `${d}д`;
  if (h > 0) return m > 0 ? `${h}ч ${m}м` : `${h}ч`;
  if (m > 0) return s > 0 ? `${m}м ${s}с` : `${m}м`;
  return `${s}с`;
}

/** Signed money with thousands separators (empty for non-finite). */
export function fmtPnl(n: number): string {
  if (!Number.isFinite(n)) return "—";
  // `-Math.abs(0)` is `-0`, which would render as "-0" (e.g. the fee column of
  // a fee-free session).
  const value = Object.is(n, -0) ? 0 : n;
  const v = value.toLocaleString("en-US", { maximumFractionDigits: 2 });
  return (value > 0 ? "+" : "") + v;
}

/**
 * Stable identity of one reconstructed trade inside a trader's history.
 *
 * Order ids restart at 1 on every page load, so `entry-exit` alone collides
 * between replay sessions (two different trades of the same symbol would share
 * a key and inherit each other's review row). The engine instance is part of
 * the ref; older server payloads without one still produce a usable key.
 */
/**
 * Stable key of one trade: ties a table row to its chart markers and to its
 * post-mortem row.
 *
 * The SYMBOL is part of the key. In local-engine mode every symbol has its own
 * order counter — SOL and XRP both start at 1 — so `engine-1-2` named two
 * different trades. Clicking XRP's first row then found SOL's first trade and
 * zoomed the chart to a time with no XRP candles: the trade "did not show".
 */
export function tradeRef(
  trade: Pick<DiaryTrade, "symbol" | "engine_id" | "entry_order_id" | "exit_order_id">,
): string {
  const engine = trade.engine_id ?? "";
  const pair = `${trade.entry_order_id}-${trade.exit_order_id}`;
  const base = engine === "" ? pair : `${engine}-${pair}`;
  return `${trade.symbol}|${base}`;
}

/**
 * Entry/exit markers for the trades of ONE symbol (the chart shows one
 * symbol at a time; the server may have trades across several).
 * Open positions get an "entry" marker too, so the still-held lot is
 * visible on the chart.
 */
export function diaryMarkers(
  trades: DiaryTrade[],
  positions: DiaryPosition[],
  symbol: string,
): DiaryMarker[] {
  const out: DiaryMarker[] = [];
  for (const t of trades) {
    if (t.symbol !== symbol) continue;
    const ref = tradeRef(t);
    out.push({
      time_ms: t.entry_time_ms,
      side: t.side,
      kind: "entry",
      price: t.entry_price,
      text: `ВХОД ${t.side === "long" ? "LONG" : "SHORT"} ${fmtPrice(t.entry_price)} · ${fmtChartTime(t.entry_time_ms)}`,
      ref,
    });
    out.push({
      time_ms: t.exit_time_ms,
      side: t.side,
      kind: "exit",
      price: t.exit_price,
      text: `ВЫХОД ${fmtPrice(t.exit_price)} · ${fmtChartTime(t.exit_time_ms)} · ${fmtPnl(t.pnl)}`,
      ref,
    });
  }
  for (const p of positions) {
    if (p.symbol !== symbol) continue;
    out.push({
      time_ms: p.entry_time_ms,
      side: p.side,
      kind: "entry",
      price: p.entry_price,
      text: `ВХОД ${p.side === "long" ? "LONG" : "SHORT"} ${fmtPrice(p.entry_price)} · ${fmtChartTime(p.entry_time_ms)}`,
      ref: `pos-${p.symbol}`,
    });
  }
  out.sort((a, b) => a.time_ms - b.time_ms || (a.kind === b.kind ? 0 : a.kind === "entry" ? -1 : 1));
  return out;
}

/**
 * Entry/exit markers the loaded candles cannot show, because their timestamp
 * falls outside the loaded span. The chart drops such markers silently
 * (lightweight-charts rejects times outside the series), so the page must say
 * it out loud instead of showing a chart that looks like the trader never
 * traded.
 *
 * Returns the count plus the covered span, or null when everything is inside.
 */
export function markersOutsideCandles(
  trades: DiaryTrade[],
  positions: DiaryPosition[],
  symbol: string,
  candles: ReadonlyArray<readonly number[]>,
): { outside: number; total: number; from: number; to: number } | null {
  const times = candles.map((c) => c[0]);
  if (times.length === 0) return null;
  const from = times[0];
  const to = times[times.length - 1];
  const marks = diaryMarkers(trades, positions, symbol);
  if (marks.length === 0) return null;
  const outside = marks.filter((m) => m.time_ms < from || m.time_ms > to).length;
  return outside > 0 ? { outside, total: marks.length, from, to } : null;
}

/** Symbols that have trades or an open position, in first-seen order. */
export function diarySymbols(diary: Pick<DiaryData, "trades" | "positions">): string[] {
  const seen: string[] = [];
  const push = (s: string): void => {
    if (s && !seen.includes(s)) seen.push(s);
  };
  for (const t of diary.trades ?? []) push(t.symbol);
  for (const p of diary.positions ?? []) push(p.symbol);
  return seen;
}

/**
 * Candle window for the diary chart. When the session's replay window is
 * available, the chart covers it entirely (more context than just the
 * trades). Without it, spans entries..exits with pad = 10% (min 1 hour).
 * Returns null when nothing to draw.
 */
export function chartRange(
  trades: DiaryTrade[],
  positions: DiaryPosition[],
  symbol: string,
  sessionRange?: { start_ms?: number; end_ms?: number } | null,
): { start_ms: number; end_ms: number } | null {
  const times: number[] = [];
  for (const t of trades) {
    if (t.symbol !== symbol) continue;
    times.push(t.entry_time_ms, t.exit_time_ms);
  }
  for (const p of positions) {
    if (p.symbol !== symbol) continue;
    times.push(p.entry_time_ms);
  }
  const real = times.filter((t) => Number.isFinite(t) && t > 0);
  if (sessionRange && sessionRange.start_ms && sessionRange.end_ms) {
    const start = sessionRange.start_ms;
    const end = sessionRange.end_ms;
    if (Number.isFinite(start) && Number.isFinite(end) && end > start) {
      // If trades happen outside the replay window, still cover them.
      const withTrades = real.length
        ? { start_ms: Math.min(start, Math.min(...real)), end_ms: Math.max(end, Math.max(...real)) }
        : { start_ms: start, end_ms: end };
      return withTrades;
    }
  }
  if (real.length === 0) return null;
  const lo = Math.min(...real);
  const hi = Math.max(...real);
  const pad = Math.max(Math.round((hi - lo) * 0.1), 3_600_000);
  return { start_ms: lo - pad, end_ms: hi + pad };
}

/** One contiguous download window for the diary chart. */
export interface ChartWindow {
  start_ms: number;
  end_ms: number;
}

/**
 * Candle windows the diary chart actually needs: one around every trade of
 * `symbol` (plus `padMs` of context), never the whole replay range. A diary
 * with two trades costs two small kline requests instead of the session's full
 * history — the difference between a fast page and 18 paginated downloads.
 *
 * Nearby trades are merged (a gap smaller than `padMs` would draw an empty
 * stretch anyway) and the list is capped at `maxWindows`, joining the closest
 * pairs first, so a busy trader cannot turn the page into a request storm.
 */
export function chartWindows(
  trades: DiaryTrade[],
  positions: DiaryPosition[],
  symbol: string,
  padMs = 3_600_000,
  maxWindows = 8,
): ChartWindow[] {
  const spans: ChartWindow[] = [];
  const add = (lo: number, hi: number): void => {
    if (!Number.isFinite(lo) || !Number.isFinite(hi)) return;
    if (lo <= 0 || hi < lo) return;
    spans.push({ start_ms: lo - padMs, end_ms: hi + padMs });
  };
  for (const t of trades) {
    if (t.symbol !== symbol) continue;
    add(Math.min(t.entry_time_ms, t.exit_time_ms), Math.max(t.entry_time_ms, t.exit_time_ms));
  }
  for (const p of positions) {
    if (p.symbol !== symbol) continue;
    add(p.entry_time_ms, p.entry_time_ms);
  }
  if (spans.length === 0) return [];
  spans.sort((a, b) => a.start_ms - b.start_ms);
  const merged: ChartWindow[] = [];
  for (const span of spans) {
    const last = merged[merged.length - 1];
    if (last && span.start_ms <= last.end_ms + padMs) {
      last.end_ms = Math.max(last.end_ms, span.end_ms);
    } else {
      merged.push({ ...span });
    }
  }
  while (merged.length > Math.max(1, maxWindows)) {
    let at = 1;
    let gap = Infinity;
    for (let i = 1; i < merged.length; i += 1) {
      const g = merged[i].start_ms - merged[i - 1].end_ms;
      if (g < gap) {
        gap = g;
        at = i;
      }
    }
    merged[at - 1].end_ms = Math.max(merged[at - 1].end_ms, merged[at].end_ms);
    merged.splice(at, 1);
  }
  return merged;
}

/** Stats with safe defaults so the page never divides by zero or NaNs. */export function safeStats(diary: DiaryData): DiaryStats {
  return (
    diary.stats ?? {
      total_trades: 0,
      win_rate: 0,
      wins: 0,
      losses: 0,
      realized_pnl: 0,
      total_pnl: 0,
      avg_r: 0,
      avg_win: 0,
      avg_loss: 0,
      best: 0,
      worst: 0,
      fees: 0,
    }
  );
}

/** Overall R of the diary: total PnL over the fixed risk (0 if risk unknown). */
export function overallR(diary: DiaryData): number {
  // The server sums each trade's R, measured against the risk that trade
  // actually ran (qty x |entry - stop|). A +6R trade must not be diluted into
  // +4R by a planned-vs-filled entry difference.
  const settled = diary.stats?.total_r;
  if (typeof settled === "number" && Number.isFinite(settled)) return settled;
  const trades = diary.trades ?? [];
  if (trades.length > 0) {
    return trades.reduce((sum, t) => sum + (Number.isFinite(t.r) ? t.r : 0), 0);
  }
  const risk = diary.risk_usdt ?? 0;
  const pnl = safeStats(diary).total_pnl;
  if (!(risk > 0)) return 0;
  return pnl / risk;
}
