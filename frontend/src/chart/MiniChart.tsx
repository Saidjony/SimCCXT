import { useEffect, useMemo, useRef } from "react";
import {
  BarSeries,
  ColorType,
  CrosshairMode,
  HistogramSeries,
  createChart,
} from "lightweight-charts";
import type { AutoscaleInfo, IChartApi, ISeriesApi, UTCTimestamp } from "lightweight-charts";
import type { CandleRow } from "../types/protocol";
import type { Drawing } from "../state/store";
import { chartCriteriaKey, getState, useSim } from "../state/store";
import { criterionLabel } from "../trade/criteria";
import { nearestCandleTime, priceAxisFormat } from "./overlays";
import { watchAdaptivePriceAxis } from "./adaptivePriceAxis";
import { ChartCriteriaPicker } from "./ChartCriteriaPicker";
import { DrawingEngine } from "./primitives/engine";
import { publishCrosshair, subscribeCrosshair } from "./crosshairSync";

import { TIMEFRAMES, timeframeOr, type Timeframe } from "../market/timeframes";
import { aggregateCandles } from "../market/aggregate";

/** Intervals the terminal offers: 5m (downloaded) + 1h and 1d (aggregated). */
export const CHART_TIMEFRAMES: readonly Timeframe[] = TIMEFRAMES;

interface Props {
  panel: string;
  label: string;
  timeframe: string;
  onTimeframeChange: (timeframe: string) => void;
  onToggleMaximize: () => void;
  maximized: boolean;
  mainTimeframe: string;
}

export function MiniChart({
  panel,
  label,
  timeframe,
  onTimeframeChange,
  onToggleMaximize,
  maximized,
  mainTimeframe,
}: Props): JSX.Element {
  const symbol = useSim((s) => s.symbol);
  // A coin switch (and every replay reset) arrives as a full snapshot. The
  // pane must repaint from scratch: the new series usually shares the same
  // window, so a length/first-bar comparison would silently take the
  // incremental path and keep the PREVIOUS coin's bars on screen.
  const fullReset = useSim((s) => s.fullReset);
  const hostRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const applyingCrosshairRef = useRef(false);
  const barsRef = useRef<ISeriesApi<"Bar"> | null>(null);
  const refreshPriceAxisRef = useRef<(() => void) | null>(null);
  const volumeRef = useRef<ISeriesApi<"Histogram"> | null>(null);
  // Data source: the SHARED session's candle buffer, aggregated into this
  // pane's timeframe. This is what keeps every pane on the same clock and
  // price (main + minis read from one store instead of three sessions).
  const candlesBase = useSim((s) => s.candles);
  const aggRef = useRef<{ source: readonly CandleRow[]; rows: CandleRow[] } | null>(null);
  const candles = useMemo(() => paneBars(candlesBase, timeframe, aggRef), [candlesBase, timeframe]);
  const drawings = useSim((s) => s.drawings);
  const drawingsRef = useRef(drawings);
  drawingsRef.current = drawings;
  const candleTimes = useMemo(() => candles.map((candle) => candle[0]), [candles]);
  const candleTimesRef = useRef(candleTimes);
  candleTimesRef.current = candleTimes;
  const positionSizeUsdt = useSim((s) => s.positionSizeUsdt);
  /** Drawing primitives live on this chart's series, outside React's render. */
  const engineRef = useRef<DrawingEngine | null>(null);
  const positionSizeUsdtRef = useRef(positionSizeUsdt);
  positionSizeUsdtRef.current = positionSizeUsdt;

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
    const bars = chart.addSeries(BarSeries, {
      upColor: "#3d9a50",
      downColor: "#d46666",
      thinBars: false,
      autoscaleInfoProvider: (base: () => AutoscaleInfo | null) =>
        autoscaleForDrawings(chart, base(), drawingsRef.current, candleTimesRef.current),
    });
    const volume = chart.addSeries(HistogramSeries, {
      priceScaleId: "",
      priceFormat: { type: "volume" },
    });
    chart.priceScale("").applyOptions({ scaleMargins: { top: 0.85, bottom: 0 } });
    chartRef.current = chart;
    barsRef.current = bars;
    volumeRef.current = volume;
    const adaptivePriceAxis = watchAdaptivePriceAxis(
      chart,
      bars,
      el,
      () => getState().candles.at(-1)?.[4] ?? 0,
    );
    refreshPriceAxisRef.current = adaptivePriceAxis.refresh;
    // Mirror of the main chart's drawings: same primitives, read-only layer.
    const engine = new DrawingEngine({
      chart,
      series: bars,
      times: () => candleTimesRef.current,
      positionSizeUsdt: positionSizeUsdtRef.current,
      symbol,
    });
    engineRef.current = engine;
    engine.sync(drawingsRef.current, null);
    const observer = new ResizeObserver(() => {
      chart.applyOptions({ width: el.clientWidth, height: el.clientHeight });
    });
    observer.observe(el);
    const crosshairListener = subscribeCrosshair(({ position, source }) => {
      if (source === panel) return;
      const currentChart = chartRef.current;
      const currentBars = barsRef.current;
      if (!currentChart || !currentBars || !position || candleTimesRef.current.length === 0) {
        currentChart?.clearCrosshairPosition();
        return;
      }
      if (position.price === undefined) return;
      const timestamp = nearestCandleTime(candleTimesRef.current, position.timestamp);
      if (timestamp === null) return;
      applyingCrosshairRef.current = true;
      currentChart.setCrosshairPosition(
        position.price,
        Math.floor(timestamp / 1000) as UTCTimestamp,
        currentBars,
      );
      window.requestAnimationFrame(() => { applyingCrosshairRef.current = false; });
    });
    const onCrosshairMove = (param: import("lightweight-charts").MouseEventParams): void => {
      if (applyingCrosshairRef.current) return;
      const point = param.point;
      if (!point || param.time === undefined || !barsRef.current) {
        publishCrosshair(null, panel);
        return;
      }
      const price = barsRef.current.coordinateToPrice(point.y);
      const timestamp = typeof param.time === "number"
        ? param.time * 1000
        : typeof param.time === "string"
          ? Date.parse(param.time + (param.time.length === 10 ? "T00:00:00Z" : "Z"))
          : Date.UTC(param.time.year, param.time.month - 1, param.time.day);
      if (price !== null && Number.isFinite(timestamp)) publishCrosshair({ timestamp, price }, panel);
    };
    chart.subscribeCrosshairMove(onCrosshairMove);
    return () => {
      observer.disconnect();
      crosshairListener();
      adaptivePriceAxis.dispose();
      refreshPriceAxisRef.current = null;
      chart.unsubscribeCrosshairMove(onCrosshairMove);
      publishCrosshair(null, panel);
      engine.destroy();
      engineRef.current = null;
      // The series dies with the chart; force a full setData on the next mount
      // instead of updating a bar that no longer exists.
      renderedRef.current = { len: 0, first: 0, timeframe: "", symbol: "" };
      chart.remove();
      chartRef.current = null;
      barsRef.current = null;
      volumeRef.current = null;
    };
  }, []);

  useEffect(() => {
    barsRef.current?.applyOptions({
      autoscaleInfoProvider: (base: () => AutoscaleInfo | null) => {
        const chart = chartRef.current;
        return chart
          ? autoscaleForDrawings(chart, base(), drawings, candleTimesRef.current)
          : base();
      },
    });
  }, [drawings]);

  const renderedRef = useRef<{ len: number; first: number; timeframe: string; symbol: string }>({
    len: 0,
    first: 0,
    timeframe: "",
    symbol: "",
  });
  useEffect(() => {
    const bars = barsRef.current;
    const volume = volumeRef.current;
    if (!bars || !volume) return;
    const lastCandle = candles[candles.length - 1];
    if (!lastCandle) return;
    // Tick size follows the price magnitude (see priceAxisFormat): a cheap pair
    // must not be rounded to whole cents.
    bars.applyOptions({ priceFormat: { type: "price", ...priceAxisFormat(lastCandle[4]) } });
    const first = candles[0][0];
    const prev = renderedRef.current;
    const full =
      fullReset ||
      prev.symbol !== symbol ||
      prev.timeframe !== timeframe ||
      candles.length < prev.len ||
      prev.len === 0 ||
      first !== prev.first;
    if (full) {
      bars.setData(candles.map(([ts, open, high, low, close]) => ({
        time: Math.floor(ts / 1000) as UTCTimestamp,
        open,
        high,
        low,
        close,
      })));
      volume.setData(candles.map(([ts, open, high, low, close, amount]) => ({
        time: Math.floor(ts / 1000) as UTCTimestamp,
        value: amount,
        color: close >= open ? "rgba(61, 154, 80, 0.45)" : "rgba(212, 102, 102, 0.45)",
      })));
      chartRef.current?.priceScale("right").applyOptions({ autoScale: true });
    } else {
      // One bucket usually changes; two when a new bucket just opened. Update
      // from the previously last bucket so its finalized close is corrected.
      const from = Math.max(prev.len - 1, 0);
      for (let i = from; i < candles.length; i += 1) {
        const [ts, open, high, low, close, amount] = candles[i];
        bars.update({
          time: Math.floor(ts / 1000) as UTCTimestamp,
          open,
          high,
          low,
          close,
        });
        volume.update({
          time: Math.floor(ts / 1000) as UTCTimestamp,
          value: amount,
          color: close >= open ? "rgba(61, 154, 80, 0.45)" : "rgba(212, 102, 102, 0.45)",
        });
      }
    }
    renderedRef.current = { len: candles.length, first, timeframe, symbol };
    // Few aggregated bars (early replay on a higher pane timeframe): spread
    // them across the pane instead of pinning one bar at the right edge.
    if (candles.length > 0 && candles.length < 25) {
      chartRef.current?.timeScale().fitContent();
    } else {
      chartRef.current?.timeScale().scrollToRealTime();
    }
  }, [candles, timeframe, symbol, fullReset]);

  // Store drawings -> this pane's primitives (same engine as the main chart).
  useEffect(() => {
    engineRef.current?.sync(drawings, null);
  }, [drawings]);

  // The position header chip (size + base asset) on mirrored drawings.
  useEffect(() => {
    engineRef.current?.setContext({ positionSizeUsdt, symbol });
  }, [positionSizeUsdt, symbol]);

  const selectedCriteria = useSim((s) => s.chartCriteria[chartCriteriaKey(s.symbol, timeframe)] ?? []);

  return (
    <section className="chart-pane">
      <header className="chart-pane-head">
        <strong>{label}</strong>
        <span className="chart-pane-symbol">{symbol}</span>
        <select
          className="tf-select"
          aria-label={"Таймфрейм " + label}
          value={timeframe}
          onChange={(event) => onTimeframeChange(event.target.value)}
        >
          {CHART_TIMEFRAMES.map((item) => <option key={item} value={item}>{item}</option>)}
        </select>
        <ChartCriteriaPicker timeframe={timeframe} />
        <button
          className="btn b-dim chart-maximize"
          onClick={onToggleMaximize}
          aria-label={maximized ? "Вернуть сетку графиков" : "Развернуть график"}
          title={maximized ? "Вернуть сетку графиков" : "На весь экран"}
        >
          {maximized ? "↙" : "⛶"}
        </button>
      </header>
      <div className="chart-pane-body">
        <div ref={hostRef} className="mini-chart" />
        {selectedCriteria.length > 0 && (
          <div className="mini-chart-criteria">
            {selectedCriteria.slice(0, 6).map((id) => (
              <span key={id} className="chip">✓ {criterionLabel(id)}</span>
            ))}
            {selectedCriteria.length > 6 && (
              <span className="chip more">+{selectedCriteria.length - 6}</span>
            )}
          </div>
        )}
        {status && <div className="mini-chart-status">{status}</div>}
      </div>
    </section>
  );
}

function autoscaleForDrawings(
  chart: IChartApi,
  base: AutoscaleInfo | null,
  drawings: Drawing[],
  candleTimes: number[],
): AutoscaleInfo | null {
  const visible = chart.timeScale().getVisibleRange();
  if (
    !visible ||
    typeof visible.from !== "number" ||
    typeof visible.to !== "number" ||
    candleTimes.length === 0
  ) return base;
  const from = visible.from * 1000;
  const to = visible.to * 1000;
  const extraPrices: number[] = [];
  for (const drawing of drawings) {
    const [start, end] = drawingTimeBounds(drawing);
    const projectedStart = nearestCandleTime(candleTimes, start);
    const projectedEnd = nearestCandleTime(candleTimes, end);
    if (projectedStart === null || projectedEnd === null) continue;
    if (Math.max(projectedStart, projectedEnd) < from || Math.min(projectedStart, projectedEnd) > to) continue;
    extraPrices.push(...drawingPrices(drawing));
  }
  if (extraPrices.length === 0) return base;
  const minValue = Math.min(base?.priceRange?.minValue ?? Infinity, ...extraPrices);
  const maxValue = Math.max(base?.priceRange?.maxValue ?? -Infinity, ...extraPrices);
  const padding = Math.max((maxValue - minValue) * 0.04, Math.abs(maxValue) * 0.0001, 1);
  return {
    priceRange: { minValue: minValue - padding, maxValue: maxValue + padding },
    margins: base?.margins,
  };
}

function drawingTimeBounds(drawing: Drawing): [number, number] {
  switch (drawing.kind) {
    case "ray": return [drawing.t0, drawing.t0];
    case "vline": return [drawing.t, drawing.t];
    case "zone":
    case "position": return [drawing.t1, drawing.t2];
    case "note": return [drawing.t0, drawing.t0];
    case "arrow": return [drawing.t0, drawing.t1];
    case "brush": return drawing.points.length ? [drawing.points[0].t, drawing.points[drawing.points.length - 1].t] : [0, 0];
  }
}

function drawingPrices(drawing: Drawing): number[] {
  switch (drawing.kind) {
    case "ray": return [drawing.price];
    case "vline": return [];
    case "zone": return [drawing.p1, drawing.p2];
    case "position": return [drawing.entry, drawing.sl, drawing.tp];
    case "note": return [drawing.price];
    case "arrow": return [drawing.price, drawing.price1];
    case "brush": return drawing.points.map((pt) => pt.p);
  }
}

/**
 * The pane's bars: the SAME aggregation the replay engine and the main chart
 * use (`market/aggregate.ts`), so a 1h bar is identical everywhere.
 *
 * This pane used to carry its own aggregator. On an empty series it returned
 * the input unchanged, and the first render of every new series happens with
 * zero candles — so the shared buffer was handed back untouched under a label
 * that claimed hour-long buckets. One aggregator, one behaviour.
 */
function paneBars(
  base: readonly CandleRow[],
  timeframe: string,
  cacheRef: React.MutableRefObject<{ source: readonly CandleRow[]; rows: CandleRow[] } | null>,
): CandleRow[] {
  const cached = cacheRef.current;
  if (cached && cached.source === base) return cached.rows;
  const rows = aggregateCandles(base, timeframeOr(timeframe, "5m"));
  cacheRef.current = { source: base, rows };
  return rows;
}
