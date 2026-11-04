// Post-trade review of the trader's OWN session: what actually happened on the
// chart between entry and exit, and what it says about the decision.
//
// Pure data in / pure data out (no DOM, no fetch) so `node --test` covers it —
// see test/review.test.ts. The panel in components/TradeReview.tsx only renders
// what this module computes.
//
// The point is not to restate PnL (the diary already shows it) but to answer the
// two questions a trader actually asks afterwards: "how far did it go my way
// before it hit me?" (MFE/MAE) and "did I follow my own rules?" (verdict).

import type { CandleRow, DiaryTrade } from "../types/protocol";
import { tradeRef } from "./diary.ts";

/** How the trade ended, judged against the trader's own protective levels. */
export type ExitKind =
  | "stop" // filled at (or within a tick of) the stop — discipline kept
  | "target" // filled at the take-profit
  | "manual" // closed by hand before either level
  | "open"; // still open

/** Tolerance for "filled at my level": 0.1% of the entry, so normal slippage
 *  still counts as hitting the stop rather than as a manual exit. */
const LEVEL_TOLERANCE = 0.001;

/**
 * Where the price went between entry and exit.
 *
 * `mfe` (maximum favourable excursion) is how far it moved in the trade's favour,
 * `mae` the worst it went against it — both absolute price distances. Feeding
 * them the planned risk turns them into R, which is the only comparable unit:
 * "it was 2R in my favour and I still closed at −1R" is a completely different
 * lesson from "it never worked".
 */
export function excursion(
  candles: ReadonlyArray<CandleRow>,
  entryMs: number,
  exitMs: number,
  side: "long" | "short",
  entryPrice: number,
): { mfe: number; mae: number; bars: number } {
  const long = side === "long";
  let mfe = 0;
  let mae = 0;
  let bars = 0;
  for (const row of candles) {
    const ts = row[0];
    if (ts < entryMs || ts > exitMs) continue;
    const high = row[2];
    const low = row[3];
    bars++;
    // Favourable extreme and adverse extreme, per side.
    const up = long ? high - entryPrice : entryPrice - low;
    const down = long ? entryPrice - low : high - entryPrice;
    if (up > mfe) mfe = up;
    if (down > mae) mae = down;
  }
  return { mfe, mae, bars };
}

/**
 * Convert a price distance into R given the planned risk per unit
 * (`|entry − stop|`). Zero/absent risk ⇒ null rather than Infinity, so callers
 * can render "—".
 */
export function toR(distance: number, riskPerUnit: number): number | null {
  if (!Number.isFinite(distance) || !Number.isFinite(riskPerUnit) || !(riskPerUnit > 0)) {
    return null;
  }
  return distance / riskPerUnit;
}

/**
 * How a trade ended, given the protective levels the trader set.
 *
 * Without a stop the verdict is the finding: an unprotected entry has no
 * defined risk, so its R is meaningless and the size was a guess.
 */
export function classifyExit(
  trade: Pick<DiaryTrade, "entry_price" | "exit_price" | "exit_reason">,
  levels: { stop?: number | null; target?: number | null } = {},
  open = false,
): ExitKind {
  if (open) return "open";
  // The server knows which protective order fired — trust it; executions can
  // predate the current replay rule that caps stop-loss fills at their trigger.
  if (trade.exit_reason === "stop") return "stop";
  if (trade.exit_reason === "take") return "target";
  if (trade.exit_reason) return "manual";
  const { entry_price: entry, exit_price: exit } = trade;
  if (!Number.isFinite(entry) || !Number.isFinite(exit)) return "manual";
  const tol = Math.abs(entry) * LEVEL_TOLERANCE;
  const near = (level: number | null | undefined): boolean =>
    typeof level === "number" && Number.isFinite(level) && Math.abs(exit - level) <= tol;
  if (near(levels.stop)) return "stop";
  if (near(levels.target)) return "target";
  return "manual";
}

/** One row of the review: the trade plus what the chart did while it was open. */
export interface ReviewRow {
  ref: string;
  trade: DiaryTrade;
  kind: ExitKind;
  /** Excursion in R, when the trade had a defined risk. */
  mfeR: number | null;
  maeR: number | null;
  /** The same excursion as a percent of the entry — works even with no stop. */
  mfePct: number | null;
  maePct: number | null;
  /** Favourable move that was given back: MFE minus what was actually taken. */
  gaveBackR: number | null;
  /**
   * Consecutive stop-outs immediately BEFORE this trade on the same symbol —
   * the "прогрештор" a trader should notice before the next entry.
   */
  priorStopStreak: number;
  /** Process findings, worst first. */
  flags: string[];
}

const FLAG_SEVERITY: Record<string, number> = {
  "без стопа — риск не определён": 3,
  "закрыл до стопа: правило не выдержано": 3,
  "упущенная прибыль": 2,
  "стоп исполнен с пробоем": 1,
  "держал дольше суток": 1,
};

export interface ReviewOptions {
  /** Protective levels per trade ref, as the trader set them (stop / TP). */
  levels?: Record<string, { stop?: number | null; target?: number | null }>;
  /** Open positions count as `open` and get no excursion verdict. */
  openRefs?: ReadonlySet<string>;
  /** Risk per trade in USDT, for the budget view. */
  riskUsdt?: number;
}

/**
 * Build the review for a set of trades, oldest first.
 *
 * `candlesFor(symbol)` supplies the bars around a trade; it is called once per
 * trade so the caller can fetch (or reuse) whatever it has.
 */
export function buildReview(
  trades: DiaryTrade[],
  candlesFor: (trade: DiaryTrade) => CandleRow[],
  options: ReviewOptions = {},
): ReviewRow[] {
  const levels = options.levels ?? {};
  const openRefs = options.openRefs ?? new Set<string>();
  const ordered = [...trades].sort((a, b) => a.entry_time_ms - b.entry_time_ms);

  // Stop-out streak per symbol, as we walk forward in time.
  const stopStreak = new Map<string, number>();
  const rows: ReviewRow[] = [];

  for (const trade of ordered) {
    const ref = tradeRef(trade);
    // Levels come from the caller when it tracked them, otherwise from the trade
    // itself (the server records the stop/TP the trader planned).
    const level = levels[ref] ?? {
      stop: trade.stop_price ?? null,
      target: trade.take_price ?? null,
    };
    const kind = classifyExit(trade, level, openRefs.has(ref));
    const priorStreak = stopStreak.get(trade.symbol) ?? 0;

    let mfeR: number | null = null;
    let maeR: number | null = null;
    let mfePct: number | null = null;
    let maePct: number | null = null;
    let gaveBackR: number | null = null;
    const flags: string[] = [];

    // Excursion is measured whenever bars exist; R needs a planned stop, % does
    // not. An unprotected trade therefore still shows "how far it went" — in
    // percent — instead of a blank cell.
    if (kind !== "open" && trade.entry_price > 0) {
      let { mfe, mae } = excursion(
        candlesFor(trade),
        trade.entry_time_ms,
        trade.exit_time_ms,
        trade.side === "short" ? "short" : "long",
        trade.entry_price,
      );
      // A protective order closes the position AT its level: the bar it fired
      // on can trade far past it, but the trader never lived through that
      // move. Without the cap a stop-out on a wide bar reads "было в −4.7R"
      // next to its actual −1R, and a take-profit looks like it ran much
      // further than the target it was closed at.
      const stopLevel = typeof level.stop === "number" ? level.stop : null;
      const targetLevel = typeof level.target === "number" ? level.target : null;
      if (kind === "stop" && stopLevel !== null) {
        mae = Math.min(mae, Math.abs(trade.entry_price - stopLevel));
      }
      if (kind === "target" && targetLevel !== null) {
        mfe = Math.min(mfe, Math.abs(targetLevel - trade.entry_price));
      }
      if (mfe > 0 || mae > 0) {
        mfePct = Number(((mfe / trade.entry_price) * 100).toFixed(3));
        maePct = Number(((mae / trade.entry_price) * 100).toFixed(3));
      }
      const hasStop = typeof level.stop === "number" && Number.isFinite(level.stop);
      if (hasStop) {
        const riskPerUnit = Math.abs(trade.entry_price - level.stop!);
        // One R everywhere: the trader's MONEY risk, not the bare stop
        // distance. Sizing is `risk_usdt / (stop distance + fees)`, so a stop
        // costs the whole budget and 1R is `risk_usdt` — the same denominator
        // as the R column and the challenge board. Using the stop distance
        // alone made the excursion a different unit: a take-profit exit showed
        // "было в +1.2R" next to "+0.47R" for the very same trade, the gap
        // being the round-trip fees (0.32R on a tight stop).
        //
        // Without a configured budget the per-trade stop distance × size is
        // the only available R, which keeps the panel usable on its own.
        const budget = options.riskUsdt;
        const moneyPerR =
          typeof budget === "number" && budget > 0 ? budget : riskPerUnit * trade.qty;
        if (moneyPerR > 0) {
          mfeR = Number(((mfe * trade.qty) / moneyPerR).toFixed(6));
          maeR = Number(((mae * trade.qty) / moneyPerR).toFixed(6));
          const realizedR = trade.pnl / moneyPerR;
          gaveBackR = Number((mfeR - realizedR).toFixed(6));
        }
      }
    }

    if (typeof level.stop !== "number" || !Number.isFinite(level.stop)) {
      flags.push("без стопа — риск не определён");
    }
    if (kind === "manual" && typeof level.stop === "number") {
      flags.push("закрыл до стопа: правило не выдержано");
    }
    // Keep the diagnostic for legacy sessions created before stop fills were
    // capped at their trigger; current replay stop fills should match the level.
    if (kind === "stop" && typeof level.stop === "number" && trade.entry_price > 0) {
      const slipPct = ((trade.exit_price - level.stop) / trade.entry_price) * 100;
      if (Math.abs(slipPct) > 0.1) {
        const sign = slipPct < 0 ? "−" : "+";
        flags.push(`стоп исполнен с пробоем: ${sign}${Math.abs(slipPct).toFixed(2)}% от уровня`);
      }
    }
    if (gaveBackR !== null && gaveBackR > 1 && trade.pnl < 0) {
      flags.push("упущенная прибыль");
    }
    const heldMs = trade.exit_time_ms - trade.entry_time_ms;
    if (kind !== "open" && heldMs > 86_400_000) {
      flags.push("держал дольше суток");
    }

    // Severity keys are prefixes (the slip flag carries a measured percentage).
    const severity = (flag: string): number => {
      const key = Object.keys(FLAG_SEVERITY).find((k) => flag.startsWith(k));
      return key ? FLAG_SEVERITY[key] : 0;
    };
    flags.sort((a, b) => severity(b) - severity(a));

    rows.push({
      ref,
      trade,
      kind,
      mfeR,
      maeR,
      mfePct,
      maePct,
      gaveBackR,
      priorStopStreak: priorStreak,
      flags,
    });

    stopStreak.set(
      trade.symbol,
      kind === "stop" ? priorStreak + 1 : kind === "open" ? priorStreak : 0,
    );
  }

  return rows;
}

/** Session-level numbers worth putting above the table. */
export interface ReviewSummary {
  trades: number;
  wins: number;
  losses: number;
  winRate: number;
  totalR: number;
  avgR: number;
  fees: number;
  /** Loss-making trades that had been ≥1R in the profit: the expensive ones. */
  gaveBack: number;
  /** Worst consecutive stop-out run on one symbol. */
  worstStopStreak: number;
  /** Fraction of trades that entered with a defined risk. */
  protectedShare: number;
}

export function summarize(rows: ReviewRow[], riskUsdt?: number): ReviewSummary {
  const trades = rows.length;
  const closed = rows.filter((r) => r.kind !== "open");
  const wins = closed.filter((r) => r.trade.pnl > 0).length;
  const losses = closed.filter((r) => r.trade.pnl < 0).length;
  const totalR = closed.reduce((sum, r) => sum + r.trade.r, 0);
  const streaks = new Map<string, number>();
  let running = 0;
  for (const row of rows) {
    running = row.kind === "stop" ? running + 1 : row.kind === "open" ? running : 0;
    streaks.set(row.trade.symbol, Math.max(streaks.get(row.trade.symbol) ?? 0, running));
  }
  const protectedRows = rows.filter((r) => !r.flags.includes("без стопа — риск не определён"));
  return {
    trades,
    wins,
    losses,
    winRate: closed.length ? wins / closed.length : 0,
    totalR,
    avgR: closed.length ? totalR / closed.length : 0,
    fees: rows.reduce((sum, r) => sum + r.trade.fee, 0),
    gaveBack: rows.filter((r) => (r.gaveBackR ?? 0) > 1 && r.trade.pnl < 0).length,
    worstStopStreak: Math.max(0, ...streaks.values()),
    protectedShare: trades ? protectedRows.length / trades : 0,
  };
}
