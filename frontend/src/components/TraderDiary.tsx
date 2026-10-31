import { useEffect, useMemo, useRef, useState } from "react";
import {
  BarSeries,
  ColorType,
  CrosshairMode,
  LineSeries,
  createChart,
  createSeriesMarkers,
} from "lightweight-charts";
import type {
  IChartApi,
  ISeriesApi,
  ISeriesMarkersPluginApi,
  SeriesMarker,
  Time,
  UTCTimestamp,
} from "lightweight-charts";
import type { CandleRow, DiaryData, DiaryPosition, DiaryTrade } from "../types/protocol";
import { formatTs } from "../types/protocol";
import type { ExchangeId } from "../types/protocol";
import { apiUrl } from "../websocket/connection";
import {
  chartRange,
  diaryMarkers,
  diarySymbols,
  fmtChartTime,
  fmtPnl,
  fmtPrice,
  markersOutsideCandles,
  overallR,
  safeStats,
  tradeRef,
} from "../trade/diary";
import { criterionLabel } from "../trade/criteria";
import { priceAxisFormat } from "../chart/overlays";
import { buildReview } from "../trade/review";
import { aggregateCandles } from "../market/aggregate";
import { loadBaseBars } from "../market/provider";
import { timeframeOr } from "../market/timeframes";
import type { ExitKind, ReviewRow } from "../trade/review";

/** How a trade ended, in the trader's terms. */
const KIND_LABEL: Record<ExitKind, string> = {
  stop: "по стопу",
  target: "по TP",
  manual: "вручную",
  open: "открыта",
};

/**
 * How far the price went, in the unit the trade actually has: R when the entry
 * carried a stop (so the risk is known), percent when it did not. Blank only
 * when the bars for this symbol are not loaded.
 */
function fmtExcursion(r: number | null, pct: number | null, sign: "+" | "−"): string {
  if (r !== null && Number.isFinite(r)) return `${sign}${r.toFixed(1)}R`;
  if (pct !== null && Number.isFinite(pct)) return `${sign}${pct.toFixed(2)}%`;
  return "—";
}

interface SortState {
  by: "time" | "r" | "pnl";
  dir: "asc" | "desc";
}

/**
 * Read-only diary of one ONLINE trader, addressed by `?trader=<nick>`.
 *
 * Deliberately isolated from the terminal: it opens NO WebSocket (the session
 * belongs to the original tab), only fetches the HTTP API on `ws_port + 1`, and
 * renders the engine's FIFO-reconstructed round trips with entry/exit markers
 * on a bar chart.
 */
export function TraderDiary({ nick }: { nick: string }): JSX.Element {
  const [diary, setDiary] = useState<DiaryData | null>(null);
  const [error, setError] = useState<string>("");
  const [loading, setLoading] = useState(true);
  const [candles, setCandles] = useState<CandleRow[]>([]);
  const [candleError, setCandleError] = useState<string>("");
  const [symbol, setSymbol] = useState<string>("");
  const [sort, setSort] = useState<SortState>({ by: "time", dir: "asc" });
  const [selectedRef, setSelectedRef] = useState<string>("");

  const trades: DiaryTrade[] = diary?.trades ?? [];
  const positions: DiaryPosition[] = diary?.positions ?? [];
  const symbols = useMemo(() => (diary ? diarySymbols(diary) : []), [diary]);
  const activeSymbol = symbol || diary?.symbol || symbols[0] || "";

  const visibleTrades = useMemo(
    () => (activeSymbol ? trades.filter((t) => t.symbol === activeSymbol) : trades),
    [trades, activeSymbol],
  );
  const sortedTrades = useMemo(() => {
    const rows = [...visibleTrades];
    const m = sort.dir === "asc" ? 1 : -1;
    if (sort.by === "r") rows.sort((a, b) => m * (a.r - b.r));
    else if (sort.by === "pnl") rows.sort((a, b) => m * (a.pnl - b.pnl));
    else rows.sort((a, b) => m * (a.entry_time_ms - b.entry_time_ms));
    return rows;
  }, [visibleTrades, sort]);

  // Per-trade post-mortem for the charted symbol: how far price went the
  // trader's way before it hit them, and whether the exit followed their rule.
  // Reuses the same logic as the review panel so both agree. `riskUsdt` keeps
  // the excursion in the same R as the server's PnL/R columns.
  const reviewByRef = useMemo(() => {
    const map = new Map<string, ReviewRow>();
    for (const row of buildReview(visibleTrades, () => candles, {
      riskUsdt: diary?.risk_usdt,
    })) {
      map.set(row.ref, row);
    }
    return map;
  }, [visibleTrades, candles, diary?.risk_usdt]);

  // Diary JSON (404 ⇒ the trader disconnected while we were loading).
  useEffect(() => {
    let alive = true;
    setLoading(true);
    setError("");
    fetch(`${apiUrl()}/api/trader/${encodeURIComponent(nick)}`)
      .then(async (res) => {
        if (res.status === 404) {
          // The API is online-only by design: 404 means "no live session",
          // not "the server is down" — say exactly that.
          setError(`Трейдер «${nick}» не в сети — дневник отвечает только для онлайн-сессий.`);
          return null;
        }
        if (!res.ok) {
          const body: unknown = await res.json().catch(() => null);
          const detail =
            body !== null && typeof body === "object" && typeof (body as { error?: unknown }).error === "string"
              ? (body as { error: string }).error
              : `HTTP ${res.status}`;
          setError(`Дневник недоступен: ${detail}`);
          return null;
        }
        return (await res.json()) as DiaryData;
      })
      .then((data) => {
        if (!alive || !data) return;
        if (data.error) setError(String(data.error));
        else setDiary(data);
      })
      .catch((e: unknown) => {
        if (!alive) return;
        setError(
          e instanceof Error
            ? `Дневник недоступен: ${e.message} (сервер оффлайн?)`
            : "Дневник недоступен",
        );
      })
      .finally(() => alive && setLoading(false));
    return () => {
      alive = false;
    };
  }, [nick]);

  // Candles for the chart window of the active symbol.
  const range = useMemo(
    () => (diary ? chartRange(trades, positions, activeSymbol, diary.range) : null),
    [diary, trades, positions, activeSymbol],
  );

  // The server may serve a window shorter than asked (its cache holds a recent
  // tail). Markers outside it cannot be drawn — say which ones, rather than
  // leaving the trader looking at a chart that looks like they never traded.
  const offscreen = useMemo(
    () => markersOutsideCandles(trades, positions, activeSymbol, candles),
    [trades, positions, activeSymbol, candles],
  );
  const offscreenNote = offscreen
    ? `Часть входов и выходов (${offscreen.outside} из ${offscreen.total}) лежит вне загруженных свечей — ` +
      `график покрывает ${fmtChartTime(offscreen.from)} … ${fmtChartTime(offscreen.to)}`
    : "";
  useEffect(() => {
    if (!diary || !activeSymbol) return;
    let alive = true;
    setCandleError("");
    setCandles([]);
    // Candles for the diary chart come from THIS browser, not from our server:
    // 5m from the venue, aggregated locally to the diary's timeframe. The
    // server only told us which symbol/timeframe/range the trader replayed.
    const timeframe = timeframeOr(diary.timeframe, "5m");
    const from = range?.start_ms ?? diary.range?.start_ms ?? 0;
    const to = range?.end_ms ?? diary.range?.end_ms ?? Date.now();
    const venue = (diary.exchange ?? "binance") as ExchangeId;
    const known: ExchangeId[] = ["binance", "bybit", "okx"];
    const target = known.includes(venue) ? venue : "binance";
    void loadBaseBars(target, activeSymbol, from, to)
      .then((base) => {
        if (!alive) return;
        const rows = aggregateCandles(base, timeframe);
        if (rows.length === 0) setCandleError(`нет свечей ${activeSymbol} за этот период`);
        else setCandles(rows);
      })
      .catch((e: unknown) => {
        if (alive) setCandleError(e instanceof Error ? e.message : "нет свечей");
      });
    return () => {
      alive = false;
    };
  }, [diary, activeSymbol, range]);

  if (loading) {
    return (
      <div className="diary-page">
        <div className="diary-note">Загрузка дневника «{nick}»…</div>
      </div>
    );
  }
  if (error || !diary) {
    return (
      <div className="diary-page">
        <a className="diary-back" href="/">
          ← к терминалу
        </a>
        <div className="diary-note err">{error || "Трейдер не найден"}</div>
      </div>
    );
  }

  const stats = safeStats(diary);
  const r = overallR(diary);

  return (
    <div className="diary-page">
      <div className="diary-head">
        <a className="diary-back" href="/">
          ← к терминалу
        </a>
        <span className="diary-nick">{diary.nick}</span>
        {diary.in_challenge && <span className="chip">в челлендже</span>}
        <span className="lbl">риск {diary.risk_usdt ?? "—"} USDT</span>
        <span className="lbl">капитал {diary.capital_usdt ?? "—"}</span>
        <span className="lbl">equity {fmtPrice(diary.equity ?? Number.NaN)}</span>
      </div>

      <div className="diary-stats">
        <Stat label="Сделок" value={String(stats.total_trades)} />
        <Stat label="Винрейт" value={`${(stats.win_rate * 100).toFixed(0)}%`} />
        <Stat label="PnL" value={fmtPnl(stats.total_pnl)} tone={stats.total_pnl} />
        <Stat label="R" value={(r > 0 ? "+" : "") + r.toFixed(2)} tone={r} />
        <Stat label="Ср. win" value={fmtPnl(stats.avg_win)} tone={stats.avg_win} />
        <Stat label="Ср. loss" value={fmtPnl(stats.avg_loss)} tone={stats.avg_loss} />
        <Stat label="Лучшая" value={fmtPnl(stats.best)} tone={stats.best} />
        <Stat label="Худшая" value={fmtPnl(stats.worst)} tone={stats.worst} />
        <Stat label="Ср. R" value={(stats.avg_r > 0 ? "+" : "") + stats.avg_r.toFixed(2)} tone={stats.avg_r} />
        <Stat label="Комиссии" value={fmtPnl(-Math.abs(stats.fees))} tone={-Math.abs(stats.fees)} />
      </div>

      {symbols.length > 1 && (
        <div className="diary-symbols">
          {symbols.map((s) => (
            <button
              key={s}
              type="button"
              className={"chip" + (s === activeSymbol ? " more" : "")}
              onClick={() => {
                setSymbol(s);
                setSelectedRef("");
              }}
            >
              {s}
            </button>
          ))}
        </div>
      )}

      <DiaryChart
        trades={trades}
        positions={positions}
        symbol={activeSymbol}
        candles={candles}
        note={candleError || offscreenNote}
        selectedRef={selectedRef}
      />

      {(diary.criteria && Object.keys(diary.criteria).length > 0 || Boolean(diary.drawings?.length)) && (
        <div className="diary-meta">
          <div className="lbl">Контекст графиков трейдера</div>
          {diary.criteria && Object.keys(diary.criteria).length > 0 && (
            <div className="diary-criteria">
              {Object.entries(diary.criteria).map(([key, ids]) => (
                <div key={key} className="diary-criteria-row">
                  <span className="lbl">{key}:</span>
                  {ids.map((id) => (
                    <span key={id} className="chip">✓ {criterionLabel(id)}</span>
                  ))}
                </div>
              ))}
            </div>
          )}
          {diary.drawings && diary.drawings.length > 0 && (
            <div className="diary-drawings">
              {diary.drawings.map((d, i) => (
                <span key={i} className="chip">
                  {String(d["kind"] ?? "?")}: {String(d["title"] ?? "").slice(0, 48)}
                </span>
              ))}
            </div>
          )}
        </div>
      )}

      {positions.length > 0 && (
        <table className="grid-table diary-table">
          <thead>
            <tr>
              <th>Открытая позиция</th>
              <th>Сторона</th>
              <th className="num">Кол-во</th>
              <th className="num">Вход</th>
              <th className="num">Марк</th>
              <th className="num">Unrealized</th>
            </tr>
          </thead>
          <tbody>
            {positions.map((p) => (
              <tr key={p.symbol + p.entry_time_ms}>
                <td>{p.symbol}</td>
                <td className={p.side === "long" ? "pos" : "neg"}>
                  {p.side === "long" ? "LONG" : "SHORT"}
                </td>
                <td className="num">{p.qty}</td>
                <td className="num">{fmtPrice(p.entry_price)}</td>
                <td className="num">{fmtPrice(p.mark_price)}</td>
                <td className={"num" + (p.unrealized > 0 ? " pos" : p.unrealized < 0 ? " neg" : "")}>
                  {fmtPnl(p.unrealized)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {sortedTrades.length === 0 ? (
        <div className="diary-note">Закрытых сделок пока нет.</div>
      ) : (
        <div className="diary-table-scroll">
          <table className="grid-table diary-table">
            <thead>
              <tr>
                <th
                  className="sortable"
                  onClick={() => setSort(toggleSort(sort, "time"))}
                >
                  Вход {sortLabel(sort, "time")}
                </th>
                <th>Выход</th>
                <th>Символ</th>
                <th>Сторона</th>
                <th className="num">Кол-во</th>
                <th className="num">Вход</th>
                <th className="num">Выход</th>
                <th
                  className="num sortable"
                  onClick={() => setSort(toggleSort(sort, "pnl"))}
                >
                  PnL {sortLabel(sort, "pnl")}
                </th>
                <th
                  className="num sortable"
                  onClick={() => setSort(toggleSort(sort, "r"))}
                >
                  R {sortLabel(sort, "r")}
                </th>
                <th
                  className="num"
                  title="Максимальный ход цены в вашу пользу, пока сделка была открыта, в R от вашего риска (без комиссии на выходе)"
                >
                  Было в +
                </th>
                <th
                  className="num"
                  title="Максимальный ход цены против вас до выхода, в R от вашего риска (без комиссии на выходе)"
                >
                  Было в −
                </th>
                <th title="Как закрылась сделка: по своему стопу, по TP или вручную">Итог</th>
                <th className="num">Комиссия</th>
              </tr>
            </thead>
            <tbody>
              {sortedTrades.map((t) => {
                const ref = tradeRef(t);
                const sel = ref === selectedRef;
                // Only the charted symbol has bars loaded, so other symbols show
                // "—" rather than a made-up excursion.
                const review = reviewByRef.get(ref);
                return (
                  <tr
                    key={ref}
                    className={sel ? "sel" : ""}
                    onClick={() => setSelectedRef(sel ? "" : ref)}
                  >
                    <td>{formatTs(t.entry_time_ms)}</td>
                    <td>{formatTs(t.exit_time_ms)}</td>
                    <td>{t.symbol}</td>
                    <td className={t.side === "long" ? "pos" : "neg"}>
                      {t.side === "long" ? "LONG" : "SHORT"}
                    </td>
                    <td className="num">{t.qty}</td>
                    <td className="num">{fmtPrice(t.entry_price)}</td>
                    <td className="num">{fmtPrice(t.exit_price)}</td>
                    <td className={"num" + (t.pnl > 0 ? " pos" : t.pnl < 0 ? " neg" : "")}>
                      {fmtPnl(t.pnl)}
                    </td>
                    <td className={"num" + (t.r > 0 ? " pos" : t.r < 0 ? " neg" : "")}>
                      {(t.r > 0 ? "+" : "") + t.r.toFixed(2)}
                    </td>
                    {/* What the price actually did while the trade was open —
                        in R when the entry had a stop (a defined risk), in
                        percent otherwise; only the charted symbol has bars. */}
                    <td className={"num" + ((review?.mfeR ?? 0) > 1 ? " pos" : "")}>
                      {fmtExcursion(review?.mfeR ?? null, review?.mfePct ?? null, "+")}
                    </td>
                    <td className={"num" + ((review?.maeR ?? 0) > 1 ? " neg" : "")}>
                      {fmtExcursion(review?.maeR ?? null, review?.maePct ?? null, "−")}
                    </td>
                    <td className={"exit-kind " + (review?.kind ?? "manual")}>
                      {review ? KIND_LABEL[review.kind] : "—"}
                    </td>
                    <td className="num">{fmtPnl(-Math.abs(t.fee))}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

function toggleSort(sort: SortState, by: SortState["by"]): SortState {
  if (sort.by !== by) return { by, dir: "desc" };
  return { by, dir: sort.dir === "desc" ? "asc" : "desc" };
}
function sortLabel(sort: SortState, by: SortState["by"]): string {
  if (sort.by !== by) return "";
  return sort.dir === "desc" ? "▼" : "▲";
}

function Stat({ label, value, tone }: { label: string; value: string; tone?: number }): JSX.Element {
  const cls = tone === undefined || tone === 0 ? "" : tone > 0 ? " pos" : " neg";
  return (
    <div className="diary-stat">
      <span className="lbl">{label}</span>
      <strong className={cls.trim()}>{value}</strong>
    </div>
  );
}

/**
 * Bars + per-trade entry→exit segments + entry/exit arrows. Own chart
 * instance, no crosshair sync, no drawings — a self-contained read-only view.
 */
function DiaryChart({
  trades,
  positions,
  symbol,
  candles,
  note,
  selectedRef,
}: {
  trades: DiaryTrade[];
  positions: DiaryPosition[];
  symbol: string;
  candles: CandleRow[];
  note: string;
  selectedRef: string;
}): JSX.Element {
  const hostRef = useRef<HTMLDivElement>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<ISeriesApi<"Bar"> | null>(null);
  const lineSeriesRef = useRef<ISeriesApi<"Line">[]>([]);
  /** Markers are a plugin in v5: created once, fed on every diary change. */
  const markersPluginRef = useRef<ISeriesMarkersPluginApi<Time> | null>(null);

  useEffect(() => {
    const el = hostRef.current;
    if (!el) return;
    const chart = createChart(el, {
      width: el.clientWidth,
      height: el.clientHeight,
      layout: {
        background: { type: ColorType.Solid, color: "#0b0e13" },
        textColor: "#c9d1d9",
        fontSize: 11,
      },
      grid: { vertLines: { color: "#1b2330" }, horzLines: { color: "#1b2330" } },
      crosshair: { mode: CrosshairMode.Normal },
      rightPriceScale: { borderColor: "#222c3b" },
      timeScale: { borderColor: "#222c3b", timeVisible: true, secondsVisible: false },
    });
    const series = chart.addSeries(BarSeries, {
      upColor: "#3d9a50",
      downColor: "#d46666",
      thinBars: false,
    });
    series.applyOptions({ priceFormat: { type: "price", ...priceAxisFormat(1) } });
    chartRef.current = chart;
    seriesRef.current = series;
    markersPluginRef.current = createSeriesMarkers(series, []);
    const observer = new ResizeObserver(() => {
      chart.applyOptions({ width: el.clientWidth, height: el.clientHeight });
    });
    observer.observe(el);
    return () => {
      observer.disconnect();
      chart.remove();
      chartRef.current = null;
      seriesRef.current = null;
      markersPluginRef.current = null;
      lineSeriesRef.current = [];
    };
  }, []);

  // Bars + markers + per-trade entry→exit line segments.
  useEffect(() => {
    const chart = chartRef.current;
    const series = seriesRef.current;
    if (!chart || !series) return;
    series.setData(
      candles.map((c) => ({
        time: Math.floor(c[0] / 1000) as UTCTimestamp,
        open: c[1],
        high: c[2],
        low: c[3],
        close: c[4],
      })),
    );
    // Tick size follows the price magnitude, so a 0.09 pair is not rounded to
    // whole cents on the review chart either.
    const lastCandle = candles[candles.length - 1];
    if (lastCandle) {
      series.applyOptions({ priceFormat: { type: "price", ...priceAxisFormat(lastCandle[4]) } });
    }

    // Drop previous segment lines.
    for (const ls of lineSeriesRef.current) {
      try {
        chart.removeSeries(ls);
      } catch {
        /* already removed */
      }
    }
    lineSeriesRef.current = [];

    const times = candles.map((c) => c[0]);

    const fitTo = (lo: number, hi: number) => {
      const pad = Math.max((hi - lo) * 0.15, 1_800_000);
      try {
        chart.timeScale().setVisibleRange({
          from: Math.floor((lo - pad) / 1000) as UTCTimestamp,
          to: Math.floor((hi + pad) / 1000) as UTCTimestamp,
        });
      } catch {
        chart.timeScale().fitContent();
      }
    };

    // A clicked row zooms to its own trade.
    const selected = selectedRef
      ? trades.find((t) => tradeRef(t) === selectedRef)
      : undefined;

    if (times.length > 0) {
      if (selected) {
        fitTo(selected.entry_time_ms, selected.exit_time_ms);
      } else {
        // Open on the trader's actions, not on the whole replay window: with 60
        // days of 5m bars on screen every entry/exit collapses into one pixel and
        // the chart looks empty. Fall back to the full series if they never
        // traded.
        const marks = diaryMarkers(trades, positions, symbol).map((m) => m.time_ms);
        if (marks.length > 0) fitTo(Math.min(...marks), Math.max(...marks));
        else chart.timeScale().fitContent();
      }
    }

    // Connect entry→exit ONLY for the selected trade. A segment per trade
    // spans the whole chart and reads as a trendline, drowning the very markers
    // the trader is looking for; on click it is the one thing that explains the
    // move, so show it then.
    const segments = selectedRef
      ? trades.filter(
          (t) =>
            t.symbol === symbol &&
            tradeRef(t) === selectedRef &&
            times.length > 0 &&
            t.entry_time_ms <= times[times.length - 1] &&
            t.exit_time_ms >= times[0],
        )
      : [];
    for (const t of segments) {
      const color = t.pnl >= 0 ? "#3d9a50" : "#d46666";
      const line = chart.addSeries(LineSeries, {
        color,
        lineWidth: 2,
        lineStyle: 2, // dashed: it is a connection, not a market series
        priceLineVisible: false,
        lastValueVisible: false,
        crosshairMarkerVisible: false,
      });
      line.setData([
        { time: Math.floor(t.entry_time_ms / 1000) as UTCTimestamp, value: t.entry_price },
        { time: Math.floor(t.exit_time_ms / 1000) as UTCTimestamp, value: t.exit_price },
      ]);
      lineSeriesRef.current.push(line);
    }

    const markers: SeriesMarker<Time>[] = [];
    for (const m of diaryMarkers(trades, positions, symbol)) {
      if (times.length === 0) break;
      if (m.time_ms < times[0] || m.time_ms > times[times.length - 1]) continue;
      const isSel = selectedRef === m.ref;
      const color = selectedRef && !isSel
        ? "#6d7686"
        : m.kind === "entry"
          ? m.side === "long" ? "#3d9a50" : "#d46666"
          : "#e0af68";
      markers.push({
        time: Math.floor(snap(times, m.time_ms) / 1000) as UTCTimestamp,
        position:
          m.kind === "entry"
            ? m.side === "long" ? "belowBar" : "aboveBar"
            : m.side === "long" ? "aboveBar" : "belowBar",
        color,
        shape: (m.kind === "entry") === (m.side === "long") ? "arrowUp" : "arrowDown",
        // Unselected markers stay short so a busy chart is still readable; the
        // selected one spells out what happened.
        text: isSel || !selectedRef ? m.text : "",
      });
    }
    markers.sort((a, b) => Number(a.time) - Number(b.time));
    try {
      markersPluginRef.current?.setMarkers(markers);
    } catch {
      /* timestamp outside the loaded series: skip silently */
    }

    // A selected row zooms to that trade (set above, before the markers).
  }, [candles, trades, positions, symbol, selectedRef]);

  return (
    <div className="diary-chart-wrap">
      <div className="diary-chart" ref={hostRef} />
      {candles.length === 0 ? (
        <div className="diary-note">{note || `Нет свечей для ${symbol || "символа"}.`}</div>
      ) : note ? (
        <div className="diary-note warn">{note}</div>
      ) : null}
    </div>
  );
}

/** Nearest bar time (markers must land on an existing candle). */
function snap(times: number[], ms: number): number {
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
