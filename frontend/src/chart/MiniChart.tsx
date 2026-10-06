import { useEffect, useMemo, useRef, useState } from "react";
import { ColorType, CrosshairMode, createChart } from "lightweight-charts";
import type { AutoscaleInfo, IChartApi, ISeriesApi, UTCTimestamp } from "lightweight-charts";
import type { CandleRow } from "../types/protocol";
import type { Drawing } from "../state/store";
import { chartCriteriaKey, useSim } from "../state/store";
import { criterionLabel } from "../trade/criteria";
import { nearestCandleTime, priceAxisFormat } from "./overlays";
import { ChartCriteriaPicker } from "./ChartCriteriaPicker";
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
  const candles = useMemo(
    () => aggregateCandles(candlesBase, timeframeMs(timeframe)),
    [candlesBase, timeframe],
  );
  const [viewTick, setViewTick] = useState(0);
  const drawings = useSim((s) => s.drawings);
  const drawingsRef = useRef(drawings);
  drawingsRef.current = drawings;
  const candleTimes = candles.map((candle) => candle[0]);
  const candleTimesRef = useRef(candleTimes);
  candleTimesRef.current = candleTimes;
  const positionSizeUsdt = useSim((s) => s.positionSizeUsdt);

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
    const bars = chart.addBarSeries({
      upColor: "#3d9a50",
      downColor: "#d46666",
      thinBars: false,
      autoscaleInfoProvider: (base: () => AutoscaleInfo | null) =>
        autoscaleForDrawings(chart, base(), drawingsRef.current, candleTimesRef.current),
    });
    const volume = chart.addHistogramSeries({
      priceScaleId: "",
      priceFormat: { type: "volume" },
    });
    chart.priceScale("").applyOptions({ scaleMargins: { top: 0.85, bottom: 0 } });
    chartRef.current = chart;
    barsRef.current = bars;
    volumeRef.current = volume;
    const observer = new ResizeObserver(() => {
      chart.applyOptions({ width: el.clientWidth, height: el.clientHeight });
      setViewTick((tick) => tick + 1);
    });
    observer.observe(el);
    const onRange = (): void => setViewTick((tick) => tick + 1);
    chart.timeScale().subscribeVisibleLogicalRangeChange(onRange);
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
    let pointerRaf = 0;
    const onPointerMove = (): void => {
      if (pointerRaf) return;
      pointerRaf = window.requestAnimationFrame(() => {
        pointerRaf = 0;
        setViewTick((tick) => tick + 1);
      });
    };
    el.addEventListener("pointermove", onPointerMove);
    return () => {
      observer.disconnect();
      chart.timeScale().unsubscribeVisibleLogicalRangeChange(onRange);
      crosshairListener();
      chart.unsubscribeCrosshairMove(onCrosshairMove);
      publishCrosshair(null, panel);
      el.removeEventListener("pointermove", onPointerMove);
      if (pointerRaf) window.cancelAnimationFrame(pointerRaf);
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
    // Tick size follows the price magnitude (see priceAxisFormat): a cheap pair
    // must not be rounded to whole cents.
    const lastCandle = candles[candles.length - 1];
    if (lastCandle) {
      bars.applyOptions({ priceFormat: { type: "price", ...priceAxisFormat(lastCandle[4]) } });
    }
    bars.setData(candles.map(([ts, open, high, low, close]) => ({
      time: Math.floor(ts / 1000) as UTCTimestamp,
      open,
      high,
      low,
      close,
    })));
    chartRef.current?.priceScale("right").applyOptions({ autoScale: true });
    volume.setData(candles.map(([ts, open, high, low, close, amount]) => ({
      time: Math.floor(ts / 1000) as UTCTimestamp,
      value: amount,
      color: close >= open ? "rgba(61, 154, 80, 0.45)" : "rgba(212, 102, 102, 0.45)",
    })));
    // Few aggregated bars (early replay on a higher pane timeframe): spread
    // them across the pane instead of pinning one bar at the right edge.
    if (candles.length > 0 && candles.length < 25) {
      chartRef.current?.timeScale().fitContent();
    } else {
      chartRef.current?.timeScale().scrollToRealTime();
    }
  }, [candles]);

  const [, tickSetViewTick] = useState(0);
  void tickSetViewTick;

  useEffect(() => {
    let disposed = false;
    const id = window.setInterval(() => {
      if (!disposed) setViewTick((t) => t + 1);
    }, 1000);
    return () => {
      disposed = true;
      window.clearInterval(id);
    };
  }, []);

  const selectedCriteria = useSim((s) => s.chartCriteria[chartCriteriaKey(s.symbol, timeframe)] ?? []);
  void viewTick;

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
        <div className="mini-drawing-layer">
          {drawings.map((drawing) => drawOnMiniChart(
            drawing,
            candleTimes,
            chartRef.current,
            barsRef.current,
            hostRef.current,
            positionSizeUsdt,
            symbol,
          ))}
        </div>
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
  const minValue = Math.min(base?.priceRange.minValue ?? Infinity, ...extraPrices);
  const maxValue = Math.max(base?.priceRange.maxValue ?? -Infinity, ...extraPrices);
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
  }
}

function drawingPrices(drawing: Drawing): number[] {
  switch (drawing.kind) {
    case "ray": return [drawing.price];
    case "vline": return [];
    case "zone": return [drawing.p1, drawing.p2];
    case "position": return [drawing.entry, drawing.sl, drawing.tp];
    case "note": return [drawing.price];
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

function drawOnMiniChart(
  drawing: Drawing,
  candleTimes: number[],
  chart: IChartApi | null,
  bars: ISeriesApi<"Bar"> | null,
  host: HTMLDivElement | null,
  positionSizeUsdt: number,
  symbol: string,
): JSX.Element | null {
  if (!chart || !bars || !host || candleTimes.length === 0) return null;
  const x = (timestamp: number): number | null => {
    const nearest = nearestCandleTime(candleTimes, timestamp);
    if (nearest === null) return null;
    const value = chart.timeScale().timeToCoordinate(
      Math.floor(nearest / 1000) as UTCTimestamp,
    );
    return typeof value === "number" ? value : null;
  };
  const y = (price: number): number | null => {
    const value = bars.priceToCoordinate(price);
    return typeof value === "number" ? value : null;
  };
  if (drawing.kind === "ray") {
    const left = x(drawing.t0);
    const top = y(drawing.price);
    if (left === null || top === null || left > host.clientWidth) return null;
    return <div key={drawing.id} className="mini-drawing mini-ray" style={{
      left, top, width: Math.max(0, host.clientWidth - left), borderColor: drawing.color,
    }} title={drawing.title} />;
  }
  if (drawing.kind === "note") {
    const left = x(drawing.t0);
    const top = y(drawing.price);
    if (left === null || top === null || left > host.clientWidth || top > host.clientHeight) return null;
    return <div key={drawing.id} className="mini-drawing mini-note" style={{
      left, top: top + 8, borderColor: drawing.color,
    }}>{drawing.title}</div>;
  }
  if (drawing.kind === "vline") {
    const left = x(drawing.t);
    if (left === null || left < 0 || left > host.clientWidth) return null;
    return <div key={drawing.id} className="mini-drawing mini-vline" style={{
      left, backgroundColor: drawing.color,
    }} title={drawing.title} />;
  }
  const left = x(drawing.kind === "zone" ? drawing.t1 : drawing.t1);
  const right = x(drawing.kind === "zone" ? drawing.t2 : drawing.t2);
  const prices = drawing.kind === "zone"
    ? [drawing.p1, drawing.p2]
    : [drawing.entry, drawing.sl, drawing.tp];
  const ys = prices.map(y);
  const validYs = ys.filter((value): value is number => value !== null);
  if (left === null || right === null || validYs.length !== ys.length) return null;
  const top = Math.min(...validYs);
  const bottom = Math.max(...validYs);
  const rangeLeft = Math.min(left, right);
  const rangeRight = Math.max(left, right);
  if (
    rangeRight < 0 ||
    rangeLeft > host.clientWidth ||
    top > host.clientHeight ||
    bottom < 0
  ) return null;
  const projectedWidth = rangeRight - rangeLeft;
  const width = Math.min(host.clientWidth, Math.max(6, projectedWidth));
  const rectLeft = Math.max(0, Math.min(host.clientWidth - width, rangeLeft - (width - projectedWidth) / 2));
  const height = Math.max(2, Math.min(host.clientHeight, bottom) - Math.max(0, top));
  if (drawing.kind === "zone") {
    return <div key={drawing.id} className="mini-drawing mini-zone" style={{
      left: rectLeft, top: Math.max(0, top), width, height,
      borderColor: drawing.color, backgroundColor: `${drawing.color}18`,
    }} title={drawing.title} />;
  }
  const long = drawing.entry > drawing.sl;
  const entryY = y(drawing.entry) ?? top;
  const qty = positionSizeUsdt / drawing.entry;
  const base = symbol.split("/")[0] ?? "";
  return (
    <div key={drawing.id} className={"mini-drawing mini-position " + (long ? "long" : "short")} style={{
      left: rectLeft, top: Math.max(0, top), width, height,
      borderColor: drawing.color,
    }} title={`${drawing.title} · ${qty.toPrecision(4)} ${base}`}>
      <span className="mini-position-entry" style={{ top: entryY - Math.max(0, top) }} />
    </div>
  );
}
