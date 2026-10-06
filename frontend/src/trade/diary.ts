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

/** Signed money with thousands separators (empty for non-finite). */
export function fmtPnl(n: number): string {
  if (!Number.isFinite(n)) return "—";
  const v = n.toLocaleString("en-US", { maximumFractionDigits: 2 });
  return (n > 0 ? "+" : "") + v;
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
    const ref = `${t.entry_order_id}-${t.exit_order_id}`;
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

/** Stats with safe defaults so the page never divides by zero or NaNs. */
export function safeStats(diary: DiaryData): DiaryStats {
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
  const risk = diary.risk_usdt ?? 0;
  const pnl = safeStats(diary).total_pnl;
  if (!(risk > 0)) return 0;
  return pnl / risk;
}
