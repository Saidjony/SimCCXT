import { useEffect, useMemo, useState } from "react";
import { useSim } from "../state/store";
import { apiUrl } from "../websocket/connection";
import type { CandleRow, DiaryData, DiaryTrade } from "../types/protocol";
import { buildReview, summarize } from "../trade/review";
import type { ExitKind, ReviewRow } from "../trade/review";
import { fmtPnl, fmtPrice, safeStats } from "../trade/diary";

/**
 * Post-trade review of the CURRENT session.
 *
 * Reuses the diary endpoint (the session is online by definition, so its round
 * trips are already there) and adds the two things the diary cannot answer:
 * how far the price went the trader's way before it closed (MFE/MAE), and
 * whether the exit followed their own rule. Bars for each trade come from
 * /api/candles — the incremental cache means that is now cheap.
 */

const KIND_LABEL: Record<ExitKind, string> = {
  stop: "по стопу",
  target: "по TP",
  manual: "ручной выход",
  open: "открыта",
};

/** Trade notes, kept per browser so a review survives a reload. */
const NOTES_KEY = "simccxt.trade-notes.v1";

function loadNotes(): Record<string, string> {
  try {
    const raw = window.localStorage.getItem(NOTES_KEY);
    const parsed = raw ? JSON.parse(raw) : {};
    return parsed && typeof parsed === "object" ? (parsed as Record<string, string>) : {};
  } catch {
    return {};
  }
}

function saveNotes(notes: Record<string, string>): void {
  try {
    window.localStorage.setItem(NOTES_KEY, JSON.stringify(notes));
  } catch {
    /* storage full or blocked: the note is just not persisted */
  }
}

/** Bars around a trade, padded so the excursion has context either side. */
async function candlesAround(
  symbol: string,
  timeframe: string,
  exchange: string,
  trade: DiaryTrade,
): Promise<CandleRow[]> {
  const pad = Math.max((trade.exit_time_ms - trade.entry_time_ms) * 0.25, 3_600_000);
  const params = new URLSearchParams({
    symbol,
    timeframe,
    exchange,
    start_ms: String(Math.round(trade.entry_time_ms - pad)),
    end_ms: String(Math.round(trade.exit_time_ms + pad)),
  });
  try {
    const res = await fetch(`${apiUrl()}/api/candles?${params.toString()}`);
    if (!res.ok) return [];
    const data = (await res.json()) as { candles?: CandleRow[] };
    return data.candles ?? [];
  } catch {
    return [];
  }
}

export function TradeReview(): JSX.Element {
  const serverNick = useSim((s) => s.serverNick);
  const timeframe = useSim((s) => s.timeframe);
  const exchange = useSim((s) => s.exchange);
  const positions = useSim((s) => s.account?.positions ?? []);
  const [diary, setDiary] = useState<DiaryData | null>(null);
  const [bars, setBars] = useState<Record<string, CandleRow[]>>({});
  const [levels, setLevels] = useState<
    Record<string, { stop?: number | null; target?: number | null }>
  >({});
  const [notes, setNotes] = useState<Record<string, string>>(loadNotes);
  const [error, setError] = useState("");

  // The diary of the running session. Refetched whenever a fill lands.
  const fills = useSim((s) => s.fills);
  useEffect(() => {
    if (!serverNick) return;
    let alive = true;
    fetch(`${apiUrl()}/api/trader/${encodeURIComponent(serverNick)}`)
      .then((res) => res.json())
      .then((data: DiaryData) => {
        if (alive && !data.error) setDiary(data);
      })
      .catch(() => {
        if (alive) setError("не удалось загрузить сделки сессии");
      });
    return () => {
      alive = false;
    };
  }, [serverNick, fills.length]);

  const trades = useMemo(() => diary?.trades ?? [], [diary]);
  const openRefs = useMemo(
    () => new Set(positions.map((p) => `pos-${p.symbol}`)),
    [positions],
  );

  // Bars + the protective levels the trader set, per trade.
  useEffect(() => {
    if (!diary || trades.length === 0) return;
    let alive = true;
    (async () => {
      const nextBars: Record<string, CandleRow[]> = {};
      const nextLevels: Record<string, { stop?: number | null; target?: number | null }> = {};
      for (const trade of trades) {
        const ref = `${trade.entry_order_id}-${trade.exit_order_id}`;
        nextBars[ref] = await candlesAround(
          trade.symbol,
          diary.timeframe || "5m",
          diary.exchange || "binance",
          trade,
        );
        // The levels come from the server (what the trader actually set). No
        // level recorded means the entry went in unprotected — that IS the
        // finding, so it is passed through as absent rather than guessed.
        nextLevels[ref] = { stop: trade.stop_price ?? null, target: trade.take_price ?? null };
      }
      if (alive) {
        setBars(nextBars);
        setLevels(nextLevels);
      }
    })();
    return () => {
      alive = false;
    };
  }, [diary, trades]);

  const rows = useMemo(
    () => buildReview(trades, (trade) => bars[`${trade.entry_order_id}-${trade.exit_order_id}`] ?? [], { levels, openRefs }),
    [trades, bars, levels, openRefs],
  );
  const summary = useMemo(() => summarize(rows), [rows]);
  const stats = diary ? safeStats(diary) : null;

  const setNote = (ref: string, text: string): void => {
    const next = { ...notes, [ref]: text };
    setNotes(next);
    saveNotes(next);
  };

  if (!serverNick) return <div className="lbl">Войди под ником, чтобы разбирать сделки.</div>;

  return (
    <div className="card review-card">
      <h4>Разбор сессии</h4>
      {error && <div className="err">{error}</div>}
      {!diary && !error && <div className="lbl">загрузка…</div>}
      {diary && rows.length === 0 && (
        <div className="lbl">Закрытых сделок пока нет — разбор появится после первой.</div>
      )}

      {rows.length > 0 && stats && (
        <div className="review-summary">
          <span>сделок <strong>{summary.trades}</strong></span>
          <span>винрейт <strong>{(summary.winRate * 100).toFixed(0)}%</strong></span>
          <span>сумма <strong className={summary.totalR >= 0 ? "pos" : "neg"}>{summary.totalR.toFixed(2)}R</strong></span>
          <span>комиссии <strong>{fmtPnl(-summary.fees)}</strong></span>
          <span title="доля входов со стопом">со стопом <strong>{(summary.protectedShare * 100).toFixed(0)}%</strong></span>
          {summary.gaveBack > 0 && (
            <span className="neg" title="проигранные сделки, которые сначала были в плюсе">
              упущено <strong>{summary.gaveBack}</strong>
            </span>
          )}
          {summary.worstStopStreak > 1 && (
            <span className="neg" title="максимальная серия стопов подряд">
              серия стопов <strong>{summary.worstStopStreak}</strong>
            </span>
          )}
        </div>
      )}

      {rows.map((row) => (
        <ReviewLine
          key={row.ref}
          row={row}
          note={notes[row.ref] ?? ""}
          onNote={(text) => setNote(row.ref, text)}
        />
      ))}
    </div>
  );
}

function ReviewLine({
  row,
  note,
  onNote,
}: {
  row: ReviewRow;
  note: string;
  onNote: (text: string) => void;
}): JSX.Element {
  const { trade, kind } = row;
  return (
    <div className={"review-line " + kind}>
      <div className="review-line-head">
        <span className={"pill " + (trade.side === "long" ? "pos" : "neg")}>
          {trade.side.toUpperCase()}
        </span>
        <span className="review-kind">{KIND_LABEL[kind]}</span>
        <span className={"num " + (trade.pnl >= 0 ? "pos" : "neg")}>{fmtPnl(trade.pnl)}</span>
        <span className="lbl">{trade.r.toFixed(2)}R</span>
        {row.priorStopStreak > 1 && (
          <span className="lbl" title="подряд стопов перед этой сделкой">
            до этого {row.priorStopStreak} стопов подряд
          </span>
        )}
      </div>
      <div className="review-line-meta lbl">
        {trade.symbol} · {fmtPrice(trade.entry_price)} → {fmtPrice(trade.exit_price)}
        {row.mfeR !== null && (
          <>
            {" · "}было в плюсе {row.mfeR.toFixed(1)}R
            {row.maeR !== null && ` / в минусе ${row.maeR.toFixed(1)}R`}
          </>
        )}
      </div>
      {row.gaveBackR !== null && row.gaveBackR > 1 && (
        <div className="lbl neg">
          отдал {row.gaveBackR.toFixed(1)}R прибыли, которая была
        </div>
      )}
      {row.flags.map((flag) => (
        <div key={flag} className="review-flag">
          {flag}
        </div>
      ))}
      <textarea
        className="review-note"
        value={note}
        placeholder="что сделал не так / что повторить"
        rows={2}
        onChange={(event) => onNote(event.target.value)}
      />
    </div>
  );
}
