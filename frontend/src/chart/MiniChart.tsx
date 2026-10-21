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
import { chartCriteriaKey, useSim } from "../state/store";
import { criterionLabel } from "../trade/criteria";
import { nearestCandleTime, priceAxisFormat } from "./overlays";
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
  const volumeRef = useRef<ISeriesApi<"Histogram"> | null>(null);
  // Data source: the SHARED session's candle buffer, aggregated into this
  // pane's timeframe. This is what keeps every pane on the same clock and
  // price (main + minis read from one store instead of three sessions).
  const candlesBase = useSim((s) => s.candles);
  const aggRef = useRef<{
    rows: CandleRow[];
    baseLen: number;
    first: number;
    step: number;
  }>({ rows: [], baseLen: 0, first: 0, step: 0 });
  // Incremental aggregation: on a tick only the forming bucket changes, so
  // re-bucketing all 10k source bars per tick was pure waste.
  const candles = useMemo(() => {
    const step = timeframeMs(timeframe);
    const cache = aggRef.current;
    const base = candlesBase;
    const baseFirst = base.length > 0 ? base[0][0] : 0;
    if (
      step !== cache.step ||
      cache.baseLen === 0 ||
      base.length < cache.baseLen ||
      base.length === 0 ||
      baseFirst !== cache.first
    ) {
      const rows = aggregateCandles(base, step);
      aggRef.current = { rows, baseLen: base.length, first: baseFirst, step };
      return rows;
    }
    const bucketOf = (ts: number): number => Math.floor(ts / step) * step;
    if (base.length === cache.baseLen) {
      // Same source length: only the last source bar was updated. Rebuild the
      // bucket that contains it (at most `step / baseStep` bars).
      const bucket = bucketOf(base[base.length - 1][0]);
      const rows = cache.rows.slice();
      const last = rows[rows.length - 1];
      if (!last || last[0] !== bucket) {
        const full = aggregateCandles(base, step);
        aggRef.current = { rows: full, baseLen: base.length, first: baseFirst, step };
        return full;
      }
      let i = base.length - 1;
      while (i > 0 && bucketOf(base[i - 1][0]) === bucket) i -= 1;
      let high = base[i][2];
      let low = base[i][3];
      let amount = 0;
      for (let k = i; k < base.length; k += 1) {
        high = Math.max(high, base[k][2]);
        low = Math.min(low, base[k][3]);
        amount += base[k][5];
      }
      rows[rows.length - 1] = [
        bucket,
        base[i][1],
        high,
        low,
        base[base.length - 1][4],
        amount,
      ];
      cache.rows = rows;
      return rows;
    }
    // New source bars: extend the last bucket or open the next one. The
    // previously forming source bar may have been updated in the same rAF
    // batch, so recompute the cached last bucket before appending.
    const rows = cache.rows.slice();
    const prevRow = base[cache.baseLen - 1];
    const prevBucket = bucketOf(prevRow[0]);
    let back = cache.baseLen - 1;
    while (back > 0 && bucketOf(base[back - 1][0]) === prevBucket) back -= 1;
    let prevHigh = base[back][2];
    let prevLow = base[back][3];
    let prevAmount = 0;
    for (let k = back; k < cache.baseLen; k += 1) {
      prevHigh = Math.max(prevHigh, base[k][2]);
      prevLow = Math.min(prevLow, base[k][3]);
      prevAmount += base[k][5];
    }
    const prevLast = rows[rows.length - 1];
    if (prevLast && prevLast[0] === prevBucket) {
      rows[rows.length - 1] = [
        prevBucket,
        base[back][1],
        prevHigh,
        prevLow,
        prevRow[4],
        prevAmount,
      ];
    }
    for (let i = cache.baseLen; i < base.length; i += 1) {
      const row = base[i];
      const bucket = bucketOf(row[0]);
      const last = rows[rows.length - 1];
      if (last && last[0] === bucket) {
        rows[rows.length - 1] = [
          bucket,
          last[1],
          Math.max(last[2], row[2]),
          Math.min(last[3], row[3]),
          row[4],
          last[5] + row[5],
        ];
      } else {
        rows.push([bucket, row[1], row[2], row[3], row[4], row[5]]);
      }
    }
    cache.rows = rows;
    cache.baseLen = base.length;
    cache.first = baseFirst;
    return rows;
  }, [candlesBase, timeframe]);
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
      chart.unsubscribeCrosshairMove(onCrosshairMove);
      publishCrosshair(null, panel);
      engine.destroy();
      engineRef.current = null;
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

  const renderedRef = useRef<{ len: number; first: number; timeframe: string }>({
    len: 0,
    first: 0,
    timeframe: "",
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
    renderedRef.current = { len: candles.length, first, timeframe };
    // Few aggregated bars (early replay on a higher pane timeframe): spread
    // them across the pane instead of pinning one bar at the right edge.
    if (candles.length > 0 && candles.length < 25) {
      chartRef.current?.timeScale().fitContent();
    } else {
      chartRef.current?.timeScale().scrollToRealTime();
    }
  }, [candles, timeframe]);

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
