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

export const CHART_TIMEFRAMES = ["1m", "5m", "15m", "1h", "4h", "1d"];

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
  const candlesVersion = useSim((s) => s.candlesVersion);
  const fullReset = useSim((s) => s.fullReset);
  const aggregationRef = useRef<AggregationCache | null>(null);
  const candles = useMemo(
    () => aggregateCandlesIncremental(
      candlesBase,
      timeframeMs(timeframe),
      aggregationRef,
      fullReset,
    ),
    [candlesBase, timeframe, fullReset, symbol, mainTimeframe],
  );
  const chartDataRef = useRef<{
    series: ISeriesApi<"Bar">;
    symbol: string;
    mainTimeframe: string;
    timeframe: string;
    firstSourceCandle: CandleRow | undefined;
    firstTimestamp: number | undefined;
    candleCount: number;
  } | null>(null);
  const candleTimesRef = useRef<number[]>([]);
  const candleTimesStateRef = useRef({ version: -1, timeframe: "" });
  if (
    candleTimesStateRef.current.version !== candlesVersion
    || candleTimesStateRef.current.timeframe !== timeframe
  ) {
    const times = candleTimesRef.current;
    if (fullReset || candleTimesStateRef.current.timeframe !== timeframe || candles.length < times.length) {
      candleTimesRef.current = candles.map((candle) => candle[0]);
    } else if (candles.length > 0) {
      const index = candles.length - 1;
      const timestamp = candles[index][0];
      if (index === times.length) times.push(timestamp);
      else times[index] = timestamp;
    }
    candleTimesStateRef.current = { version: candlesVersion, timeframe };
  }
  const candleTimes = candleTimesRef.current;
  const drawings = useSim((s) => s.drawings);
  const drawingsRef = useRef(drawings);
  drawingsRef.current = drawings;
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
      chartDataRef.current = null;
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
  }, [drawings, candles]);

  useEffect(() => {
    const bars = barsRef.current;
    const volume = volumeRef.current;
    if (!bars || !volume) return;
    const previous = chartDataRef.current;
    const reset = fullReset
      || !previous
      // React StrictMode mounts, destroys and re-creates the chart. The data
      // ref survives that remount, so comparing only the rows would take the
      // incremental branch and `update()` the last bar on an EMPTY series —
      // the pane then shows a single candle until the next full reset.
      || previous.series !== bars
      || previous.symbol !== symbol
      || previous.mainTimeframe !== mainTimeframe
      || previous.timeframe !== timeframe
      || previous.firstSourceCandle !== candlesBase[0]
      || candles.length < previous.candleCount
      || (candles.length > 0 && previous.firstTimestamp !== candles[0][0]);
    // Keep axis precision consistent with the current vertical zoom.
    const lastCandle = candles[candles.length - 1];
    if (lastCandle) {
      bars.applyOptions({
        priceFormat: {
          type: "price",
          ...priceAxisFormat(lastCandle[4], bars.priceScale().getVisibleRange()),
        },
      });
    }
    if (reset) {
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
    } else if (lastCandle && previous) {
      const firstChanged = Math.max(0, previous.candleCount - 1);
      for (let index = firstChanged; index < candles.length; index += 1) {
        const [ts, open, high, low, close, amount] = candles[index];
        const time = Math.floor(ts / 1000) as UTCTimestamp;
        bars.update({ time, open, high, low, close });
        volume.update({
          time,
          value: amount,
          color: close >= open ? "rgba(61, 154, 80, 0.45)" : "rgba(212, 102, 102, 0.45)",
        });
      }
    }
    chartDataRef.current = {
      series: bars,
      symbol,
      mainTimeframe,
      timeframe,
      firstSourceCandle: candlesBase[0],
      firstTimestamp: candles[0]?.[0],
      candleCount: candles.length,
    };
    // Few aggregated bars (early replay on a higher pane timeframe): spread
    // them across the pane instead of pinning one bar at the right edge.
    if (candles.length > 0 && candles.length < 25) {
      chartRef.current?.timeScale().fitContent();
    } else {
      chartRef.current?.timeScale().scrollToRealTime();
    }
    refreshPriceAxisRef.current?.();
  }, [candles, candlesVersion, fullReset, mainTimeframe, symbol, timeframe]);

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

function timeframeMs(timeframe: string): number {
  switch (timeframe) {
    case "1m": return 60_000;
    case "5m": return 300_000;
    case "15m": return 900_000;
    case "1h": return 3_600_000;
    case "4h": return 14_400_000;
    case "1d": return 86_400_000;
    default: return 60_000;
  }
}

/**
 * Aggregate the shared source bars (main timeframe) into the pane's timeframe
 * buckets. Buckets align to target-step boundaries; the latest bucket is
 * always the "now" one, so its close equals the main chart's last close —
 * keeping all panes' last price identical.
 */
interface AggregationCache {
  source: CandleRow[];
  stepMs: number;
  aggregated: CandleRow[];
}

function aggregateCandlesIncremental(
  rows: CandleRow[],
  stepMs: number,
  cacheRef: React.MutableRefObject<AggregationCache | null>,
  forceReset: boolean,
): CandleRow[] {
  const cached = cacheRef.current;
  if (cached?.source === rows && cached.stepMs === stepMs) return cached.aggregated;
  const stablePrefix = cached
    && cached.stepMs === stepMs
    && rows.length >= cached.source.length
    && rows[0] === cached.source[0]
    && (cached.source.length < 2
      || rows[cached.source.length - 2] === cached.source[cached.source.length - 2])
    && (rows.length === cached.source.length
      || rows[cached.source.length - 1] === cached.source[cached.source.length - 1]);
  if (!forceReset && cached && stablePrefix) {
    const aggregated = cached.aggregated;
    if (rows.length > 0) {
      const firstChangedIndex = Math.max(0, cached.source.length - 1);
      const firstBucket = Math.floor(rows[firstChangedIndex][0] / stepMs) * stepMs;
      const outputIndex = lowerBoundTime(aggregated, firstBucket);
      aggregated.length = outputIndex;
      let sourceIndex = lowerBoundTime(rows, firstBucket);
      while (sourceIndex < rows.length) {
        const bucket = Math.floor(rows[sourceIndex][0] / stepMs) * stepMs;
        aggregated.push(aggregateBucket(rows, sourceIndex, bucket, stepMs));
        sourceIndex = lowerBoundTime(rows, bucket + stepMs);
      }
    }
    cacheRef.current = { source: rows, stepMs, aggregated };
    return aggregated;
  }

  const aggregated = aggregateCandles(rows, stepMs);
  cacheRef.current = { source: rows, stepMs, aggregated };
  return aggregated;
}

function lowerBoundTime(rows: CandleRow[], timestamp: number): number {
  let low = 0;
  let high = rows.length;
  while (low < high) {
    const middle = Math.floor((low + high) / 2);
    if (rows[middle][0] < timestamp) low = middle + 1;
    else high = middle;
  }
  return low;
}

function aggregateBucket(rows: CandleRow[], start: number, bucket: number, stepMs: number): CandleRow {
  const first = rows[start];
  let high = first[2];
  let low = first[3];
  let close = first[4];
  let volume = first[5];
  for (let i = start + 1; i < rows.length && Math.floor(rows[i][0] / stepMs) * stepMs === bucket; i += 1) {
    const row = rows[i];
    high = Math.max(high, row[2]);
    low = Math.min(low, row[3]);
    close = row[4];
    volume += row[5];
  }
  return [bucket, first[1], high, low, close, volume];
}

function aggregateCandles(rows: CandleRow[], stepMs: number): CandleRow[] {
  if (stepMs <= 0 || rows.length === 0) return rows;
  const out: CandleRow[] = [];
  let bucketStart = -1;
  let o = 0;
  let h = 0;
  let l = 0;
  let c = 0;
  let v = 0;
  for (const row of rows) {
    const ts = row[0];
    const bucket = Math.floor(ts / stepMs) * stepMs;
    if (bucket !== bucketStart) {
      if (bucketStart >= 0) out.push([bucketStart, o, h, l, c, v]);
      bucketStart = bucket;
      o = row[1];
      h = row[2];
      l = row[3];
      c = row[4];
      v = row[5];
    } else {
      h = Math.max(h, row[2]);
      l = Math.min(l, row[3]);
      c = row[4];
      v += row[5];
    }
  }
  if (bucketStart >= 0) out.push([bucketStart, o, h, l, c, v]);
  return out;
}
