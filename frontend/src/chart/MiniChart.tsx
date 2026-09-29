import { useEffect, useRef, useState } from "react";
import { ColorType, CrosshairMode, createChart } from "lightweight-charts";
import type { AutoscaleInfo, IChartApi, ISeriesApi, UTCTimestamp } from "lightweight-charts";
import type { CandleRow, ServerMessage } from "../types/protocol";
import { parseServerMessage } from "../types/protocol";
import type { Drawing } from "../state/store";
import { useSim } from "../state/store";
import { criterionLabel } from "../trade/criteria";
import { nearestCandleTime } from "./overlays";
import { wsUrl } from "../websocket/connection";
import { ChartCriteriaPicker } from "./ChartCriteriaPicker";

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
  const exchange = useSim((s) => s.exchange);
  const replayNow = useSim((s) => s.replay.now_ms);
  const hostRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const barsRef = useRef<ISeriesApi<"Bar"> | null>(null);
  const volumeRef = useRef<ISeriesApi<"Histogram"> | null>(null);
  const socketRef = useRef<WebSocket | null>(null);
  const replayNowRef = useRef(replayNow);
  const lastSyncedBucketRef = useRef<number | null>(null);
  const lastMainTimeframeRef = useRef(mainTimeframe);
  replayNowRef.current = replayNow;
  const [candles, setCandles] = useState<CandleRow[]>([]);
  const [status, setStatus] = useState("Загрузка…");
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
    chartRef.current?.timeScale().scrollToRealTime();
  }, [candles]);

  useEffect(() => {
    let socket: WebSocket | null = null;
    let retryTimer = 0;
    let disposed = false;
    let retryDelay = 1_000;
    setCandles([]);
    setStatus("Загрузка…");

    const connect = (): void => {
      if (disposed) return;
      socket = new WebSocket(wsUrl());
      socketRef.current = socket;
      socket.onopen = () => {
        if (disposed) {
          socket?.close();
          return;
        }
        retryDelay = 1_000;
        setStatus("Подключено");
      };
      socket.onmessage = (event: MessageEvent<string>) => {
        if (disposed) return;
        const message = parseServerMessage(String(event.data));
        if (!message) return;
        if (message.type === "welcome") {
          socket?.send(JSON.stringify({ type: "subscribe", symbol, timeframe, exchange }));
          const now = replayNowRef.current;
          lastSyncedBucketRef.current = Math.floor(now / timeframeMs(timeframe));
          lastMainTimeframeRef.current = mainTimeframe;
          if (now > 0) {
            socket?.send(JSON.stringify({ type: "replay", action: "reset", timestamp_ms: now }));
            socket?.send(JSON.stringify({ type: "replay", action: "pause" }));
            socket?.send(JSON.stringify({ type: "subscribe", symbol, timeframe, exchange }));
          }
        } else if (message.type === "snapshot") {
          if (message.symbol !== symbol || message.timeframe !== timeframe ||
              (message.type === "snapshot" && (message.exchange ?? "binance") !== exchange)) return;
          setCandles(message.candles);
          setStatus("");
        } else if (
          message.type === "candle" &&
          message.symbol === symbol &&
          message.timeframe === timeframe &&
          (message.exchange === undefined || message.exchange === exchange)
        ) {
          setCandles((previous) => applyCandle(previous, message));
        } else if (message.type === "error") {
          setStatus(message.message);
        }
      };
      socket.onclose = () => {
        if (disposed) return;
        setStatus("Переподключение…");
        retryTimer = window.setTimeout(connect, retryDelay);
        retryDelay = Math.min(retryDelay * 2, 10_000);
      };
      socket.onerror = () => socket?.close();
    };
    connect();
    return () => {
      disposed = true;
      window.clearTimeout(retryTimer);
      if (socket?.readyState === WebSocket.CONNECTING) {
        // Close after the handshake to avoid a spurious browser warning.
      } else {
        socket?.close();
      }
      socketRef.current = null;
    };
  }, [symbol, timeframe, exchange]);

  useEffect(() => {
    if (replayNow <= 0) return;
    const bucket = Math.floor(replayNow / timeframeMs(timeframe));
    const mainTimeframeChanged = lastMainTimeframeRef.current !== mainTimeframe;
    if (lastSyncedBucketRef.current === bucket && !mainTimeframeChanged) return;
    lastSyncedBucketRef.current = bucket;
    lastMainTimeframeRef.current = mainTimeframe;
    const socket = socketRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    socket.send(JSON.stringify({ type: "replay", action: "reset", timestamp_ms: replayNow }));
    socket.send(JSON.stringify({ type: "replay", action: "pause" }));
    socket.send(JSON.stringify({ type: "subscribe", symbol, timeframe, exchange }));
  }, [replayNow, symbol, timeframe, mainTimeframe, exchange]);

  const selectedCriteria = useSim((s) => s.chartCriteria[timeframe] ?? []);
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
  }
}

function drawingPrices(drawing: Drawing): number[] {
  switch (drawing.kind) {
    case "ray": return [drawing.price];
    case "vline": return [];
    case "zone": return [drawing.p1, drawing.p2];
    case "position": return [drawing.entry, drawing.sl, drawing.tp];
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

function applyCandle(rows: CandleRow[], message: Extract<ServerMessage, { type: "candle" }>): CandleRow[] {
  const candle = message.candle;
  const last = rows[rows.length - 1];
  if (!last || candle[0] > last[0]) return [...rows, candle];
  if (candle[0] === last[0]) return [...rows.slice(0, -1), candle];
  return rows;
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
