import { useEffect, useRef, useState } from "react";
import {
  BarSeries,
  ColorType,
  CrosshairMode,
  HistogramSeries,
  LineStyle,
  createChart,
  createSeriesMarkers,
} from "lightweight-charts";
import type {
  IChartApi,
  ISeriesApi,
  ISeriesMarkersPluginApi,
  MouseEventParams,
  SeriesMarker,
  Time,
  UTCTimestamp,
} from "lightweight-charts";
import type { CandleRow } from "../types/protocol";
import { chartCriteriaKey, getState, store, useSim } from "../state/store";
import { sendSim, subscribeSim } from "../websocket/connection";
import type { Drawing, Tool } from "../state/store";
import { criterionLabel } from "../trade/criteria";
import { SPEED_DOWN_CODES, SPEED_UP_CODES, nextSpeed } from "../replay/speed";
import {
  fillChronology,
  formatPriceValue,
  invertTime,
  nearestCandleTime,
  positionDirection,
  priceAxisFormat,
  riskReward,
  snapTime,
} from "./overlays";
import { DrawBar } from "./DrawBar";
import { publishCrosshair, subscribeCrosshair } from "./crosshairSync";
import type { SharedCrosshair } from "./crosshairSync";
import type { BarPrice } from "lightweight-charts";
import { followLatest } from "./rangeSync";
import { watchAdaptivePriceAxis } from "./adaptivePriceAxis";
import { DrawingEngine } from "./primitives/engine";
import type { GhostShape } from "./primitives/ghost";
import { setMagnetMode } from "./primitives/core";
function toTime(ms: number): UTCTimestamp {
  return Math.floor(ms / 1000) as UTCTimestamp;
}

function lastAtOrBefore<T>(rows: T[], target: number, getTimestamp: (row: T) => number): number {
  let lo = 0;
  let hi = rows.length;
  while (lo < hi) {
    const mid = Math.floor((lo + hi) / 2);
    if (getTimestamp(rows[mid]) <= target) lo = mid + 1;
    else hi = mid;
  }
  return lo - 1;
}

function timeToMs(t: Time): number | null {
  if (typeof t === "number") return t * 1000;
  if (typeof t === "string") {
    const ms = Date.parse(t + (t.length === 10 ? "T00:00:00Z" : "Z"));
    return Number.isFinite(ms) ? ms : null;
  }
  return null;
}

interface HoverBar {
  text: string;
  up: boolean;
}

interface Pt {
  t: number;
  p: number;
}

interface RulerState {
  a: Pt;
  b: Pt;
  shown: boolean;
}

const RULER_FADE_MS = 4000;
const RULER_FADE_ANIM_MS = 800;

const TOOL_HINTS: Record<Exclude<Tool, "none">, string> = {
  ray: "клик — луч-уровень от этой точки · drag — двигать",
  vline: "клик — барьер: бары справа скроются, play покажет по одному",
  zone: "2 клика — углы прямоугольника · drag — двигать",
  position: "3 клика — вход, стоп, тейк · drag — двигать",
  ruler: "2 клика — замер-прямоугольник (гаснет через 4 с)",
  note: "1 клик — текстовая заметка в этой точке · drag — двигать",
  arrow: "2 клика — стрелка от точки А до Б",
  brush: "зажать и рисовать — свободный мазок (как в Paint)",
};

/**
 * OHLC bar chart. All interaction (crosshair, zoom, pan, drawings)
 * stays in the browser — nothing is sent to Rust.
 * Incremental updates: snapshot -> setData(), deltas -> update().
 *
 * Drawings, execution lines and the volume profile are lightweight-charts
 * primitives (see ./primitives): they are painted by the chart itself, so
 * there is no overlay DOM to keep in sync — pan, zoom and screenshot can never
 * drift them off the bars. This component only owns state and intent.
 */
export function Chart(): JSX.Element {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const applyingCrosshairRef = useRef(false);
  /** Hover coalesced while a programmatic sync is in flight (flushed on reset). */
  const pendingPublishRef = useRef<{ position: SharedCrosshair | null; source: string } | null>(null);
  /** Timestamp of the last applied shared cursor (drop same-T replays). */
  const lastAppliedRef = useRef<number | null>(null);
  const barsRef = useRef<ISeriesApi<"Bar"> | null>(null);
  const refreshPriceAxisRef = useRef<(() => void) | null>(null);
  const volumeRef = useRef<ISeriesApi<"Histogram"> | null>(null);
  /** Cursor we put on the chart root for a hovered barrier (see the crosshair handler). */
  const cursorOverrideRef = useRef<{ element: HTMLElement; previous: string } | null>(null);
  const pendingRef = useRef<Pt[]>([]);
  const previewRef = useRef<Pt | null>(null);
  const candleTimesRef = useRef<number[]>([]);
  const rulerTimer = useRef(0);
  const dragRef = useRef<{
    id: string;
    orig: Drawing;
    offT: number;
    offP: number;
    /** True once the drawing actually moved (distinguishes drag from click). */
    moved: boolean;
  } | null>(null);
  /** Window listeners of the in-flight drawing drag (drop or unmount). */
  const dragCleanupRef = useRef<(() => void) | null>(null);
  /** True while a freehand stroke owns the chart (pan/zoom stay parked). */
  const strokeActiveRef = useRef(false);
  /** Last pan/zoom lock we applied (avoids reading options on every mousemove). */
  const interactionsLockedRef = useRef(false);
  /** Mouse-down anchor for manual click detection (pan-safe placement). */
  const [magnet, setMagnet] = useState(false);
  const brushPointsRef = useRef<Array<{ t: number; p: number }>>([]); // freehand stroke points
  const downRef = useRef<{ x: number; y: number; button: number } | null>(null);
  /** Set when a drawing was actually dragged: suppress the click that follows. */
  const suppressClickRef = useRef(false);
  const versionRef = useRef(0);
  const chartCandleCountRef = useRef(0);
  /** Bars already painted (rAF may batch several WS candles into one render). */
  const renderedRef = useRef({ len: 0, first: 0 });
  const [follow, setFollow] = useState(true);
  const followRef = useRef(true);
  const [hover, setHover] = useState<HoverBar | null>(null);
  /** Placed measurement (auto-fades) — painted by the ruler primitive. */
  const rulerRef = useRef<RulerState | null>(null);
  /** Drawing/execution/profile primitives live on the chart's own series. */
  const engineRef = useRef<DrawingEngine | null>(null);
  /** Fill markers as the v5 markers plugin (series.setMarkers is gone). */
  const markersRef = useRef<ISeriesMarkersPluginApi<Time> | null>(null);

  const version = useSim((s) => s.candlesVersion);
  const drawings = useSim((s) => s.drawings);
  const tool = useSim((s) => s.tool);
  const fills = useSim((s) => s.fills);
  const entryProtections = useSim((s) => s.entryProtections);
  const account = useSim((s) => s.account);
  const fullReset = useSim((s) => s.fullReset);
  const symbol = useSim((s) => s.symbol);
  // Identity of the execution layer: re-runs the primitive sync only when a
  // position/order/entry actually changes, not on every account tick.
  const executionKey = JSON.stringify({
    positions: (account?.positions ?? []).map((p) => [p.symbol, p.side, p.entryPrice, p.entryTimeMs]),
    orders: (account?.open ?? []).map((o) => [o.symbol, o.side, o.triggerPrice, o.remaining]),
  });
  const selectedId = useSim((s) => s.selectedId);
  const positionSizeUsdt = useSim((s) => s.positionSizeUsdt);
  const criteria = useSim((s) => s.chartCriteria[chartCriteriaKey(s.symbol, s.timeframe)] ?? []);
  /** Changes only when a NEW bar appears (the forming bar keeps its time). */
  const candleLen = useSim((s) => s.candles.length);
  const toolRef = useRef(tool);
  toolRef.current = tool;

  // Magnet: switch the crosshair itself between free and magnet snapping
  // (lightweight-charts does the bar/time snap natively), plus our flag for
  // the price→OHLC snap in the handlers below.
  useEffect(() => {
    setMagnetMode(magnet);
    chartRef.current?.applyOptions({
      crosshair: { mode: magnet ? CrosshairMode.Magnet : CrosshairMode.Normal },
    });
  }, [magnet]);

  /**
   * Pan/zoom are disabled ONLY while something actively owns the pointer:
   * dragging a drawing or painting a freehand stroke. Every entry/exit path
   * calls this, and the wrapper also calls it on the next mouse event, so the
   * chart can never be left with `handleScroll: false` (the "frozen chart"
   * users hit after a drag/stroke that ended outside the chart).
   */
  function syncChartInteractions(verify = false): void {
    const chart = chartRef.current;
    if (!chart) return;
    const locked = dragRef.current !== null || strokeActiveRef.current;
    if (!verify && locked === interactionsLockedRef.current) return;
    if (verify) {
      const scroll = chart.options().handleScroll;
      const lockedNow =
        typeof scroll === "boolean"
          ? !scroll
          : scroll.pressedMouseMove === false && scroll.mouseWheel === false;
      if (lockedNow === locked) {
        interactionsLockedRef.current = locked;
        return;
      }
    }
    interactionsLockedRef.current = locked;
    chart.applyOptions({ handleScroll: !locked, handleScale: !locked });
  }

  // Create the chart once.
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const chart = createChart(el, {
      width: el.clientWidth,
      height: el.clientHeight,
      layout: {
        background: { type: ColorType.Solid, color: "#0b0e13" },
        textColor: "#c9d1d9",
        fontSize: 12,
      },
      grid: {
        vertLines: { color: "#1b2330" },
        horzLines: { color: "#1b2330" },
      },
      crosshair: {
        mode: CrosshairMode.Normal,
        vertLine: { style: LineStyle.Dashed },
      },
      // One right-axis width for the chart (the layout takes the widest label,
      // floored by minimumWidth), so the price scale never jumps while data
      // arrives. No left gutter: the chart stretches to the left edge.
      rightPriceScale: { borderColor: "#222c3b", minimumWidth: 96 },
      leftPriceScale: { visible: false },
      timeScale: {
        borderColor: "#222c3b",
        timeVisible: true,
        secondsVisible: false,
        // Long replay windows can hold tens of thousands of 5m bars. When they
        // are zoomed out to less than a pixel per bar, conflate them instead
        // of drawing every one: pan/zoom stays smooth and the picture is
        // visually identical at that scale. Lazy (no precompute) so loading a
        // fresh window does not pay the extra upfront cost.
        enableConflation: true,
      },
    });
    const onSharedCrosshair: Parameters<typeof subscribeCrosshair>[0] = ({ position }) => {
      if (!position) {
        lastAppliedRef.current = null;
        chart.clearCrosshairPosition(); // one call hides the cursor
        return;
      }
      // ONE shared time cursor: snap to the main candle grid so the
      // crosshair always lands on a real bar.
      const sharedTimestamp = nearestCandleTime(candleTimesRef.current, position.timestamp);
      if (sharedTimestamp === null) {
        lastAppliedRef.current = null;
        chart.clearCrosshairPosition();
        return;
      }
      lastAppliedRef.current = sharedTimestamp;
      const time = toTime(sharedTimestamp);
      const candleRows = getState().candles;
      const candleIndex = lastAtOrBefore(candleRows, sharedTimestamp, (row) => row[0]);
      const candleClose = candleIndex >= 0 ? candleRows[candleIndex][4] : undefined;
      const crosshairPrice = position.price ?? candleClose;
      applyingCrosshairRef.current = true;
      if (crosshairPrice !== undefined) {
        chart.setCrosshairPosition(crosshairPrice, time, bars);
      } else {
        chart.clearCrosshairPosition();
      }
      window.requestAnimationFrame(() => {
        applyingCrosshairRef.current = false;
        // Flush the hover coalesced during the sync (otherwise the crosshair
        // freezes one step behind the cursor when it stops moving).
        const pending = pendingPublishRef.current;
        pendingPublishRef.current = null;
        const pendingTs = pending?.position?.timestamp ?? null;
        if (pending && pendingTs !== lastAppliedRef.current) {
          publishCrosshair(pending.position, pending.source);
        }
      });
    };
    const bars = chart.addSeries(BarSeries, {
      upColor: "#3d9a50",
      downColor: "#d46666",
      openVisible: true,
      thinBars: false,
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
    // Fill markers: the v5 markers plugin, created once for the series.
    const markers = createSeriesMarkers(bars, []);
    markersRef.current = markers;
    // Every drawing / execution line / profile bar on this chart is a chart
    // primitive, reconciled with the store by the engine.
    const state0 = getState();
    const engine = new DrawingEngine({
      chart,
      series: bars,
      times: () => candleTimesRef.current,
      positionSizeUsdt: state0.positionSizeUsdt,
      symbol: state0.symbol,
      trackHover: true,
    });
    engineRef.current = engine;
    engine.sync(state0.drawings, state0.selectedId);
    engine.setProfile(true, () => getState().candles);
    engine.setExecution({
      fills: state0.fills,
      positions: state0.account?.positions ?? [],
      orders: state0.account?.open ?? [],
      symbol: state0.symbol,
      protections: state0.entryProtections,
    });
    const crosshairListener = subscribeCrosshair(onSharedCrosshair);

    const ro = new ResizeObserver(() => {
      chart.applyOptions({ width: el.clientWidth, height: el.clientHeight });
    });
    ro.observe(el);

    // [ / ] (any keyboard layout: matched on e.code) walk the replay speed one
    // rung. The server owns the clock, so this only sends the command — same
    // path the dropdown takes.
    const stepSpeed = (dir: 1 | -1): void => {
      const speed = nextSpeed(getState().replay.speed, dir);
      store.setReplaySpeed(speed); // optimistic: chained presses read this back
      sendSim({ type: "replay", action: "set_speed", speed });
    };

    const onKey = (e: KeyboardEvent): void => {
      const target = e.target as HTMLElement | null;
      if (target?.closest("input, textarea, select, button, a, summary, [contenteditable='true'], [role='button']")) {
        return;
      }
      if (e.key === "Delete" || e.key === "Backspace") {
        const id = getState().selectedId;
        if (id) {
          e.preventDefault();
          store.removeDrawing(id);
        }
      } else if (e.key === "Escape") {
        store.setTool("none");
        store.selectDrawing(null);
        pendingRef.current = [];
        previewRef.current = null;
        engineRef.current?.setGhost(null);
        // Cancel any in-progress drag/stroke: stuck drag + parked pan/zoom
        // would otherwise leave the chart unresponsive.
        if (dragRef.current) endDrawingDrag();
        cancelBrushStroke();
        syncChartInteractions();
      } else if (!e.repeat && e.key === " ") {
        e.preventDefault();
        sendSim({ type: "replay", action: getState().replay.paused ? "play" : "pause" });
      } else if (!e.repeat && e.key === "ArrowRight") {
        e.preventDefault();
        sendSim({ type: "replay", action: "step", steps: 1 });
      } else if (!e.repeat && e.key === "Home") {
        e.preventDefault();
        sendSim({ type: "replay", action: "reset" });
      } else if (!e.repeat && SPEED_UP_CODES.has(e.code)) {
        e.preventDefault();
        stepSpeed(1);
      } else if (!e.repeat && SPEED_DOWN_CODES.has(e.code)) {
        e.preventDefault();
        stepSpeed(-1);
      }
    };
    window.addEventListener("keydown", onKey);

    // Safety net: if the window loses focus mid-drag/mid-stroke, finish
    // neither — but never leave a stuck drag or parked pan/zoom behind.
    const onBlur = (): void => {
      if (dragRef.current) endDrawingDrag();
      cancelBrushStroke();
      syncChartInteractions();
    };
    window.addEventListener("blur", onBlur);

    // A stroke can end anywhere (outside the chart, outside the window):
    // finish it from the window so pan/zoom always come back. Idempotent:
    // the wrapper's mouseup finds the stroke already finished and does nothing.
    const onStrokePointerUp = (event: PointerEvent): void => {
      if (event.isPrimary) finishBrushStroke();
    };
    const onStrokePointerCancel = (): void => cancelBrushStroke();
    const onVisibility = (): void => {
      if (!document.hidden) return;
      if (dragRef.current) endDrawingDrag();
      cancelBrushStroke();
    };
    window.addEventListener("pointerup", onStrokePointerUp, true);
    window.addEventListener("pointercancel", onStrokePointerCancel, true);
    document.addEventListener("visibilitychange", onVisibility);

    // Hover readout + ghost-preview mouse tracking (local, no roundtrip).
    // Stays live even during a programmatic sync: only the bus publish is
    // coalesced (see pendingPublishRef), never the readout itself.
    chart.subscribeCrosshairMove((param: MouseEventParams) => {
      const data = param.seriesData?.get(bars) as
        | { open?: number; high?: number; low?: number; close?: number }
        | undefined;
      if (data && typeof data.close === "number" && typeof data.open === "number") {
        const up = data.close >= data.open;
        setHover({
          text:
            "O " + formatPriceValue(data.open) + "  H " + formatPriceValue(data.high ?? 0) +
            "  L " + formatPriceValue(data.low ?? 0) + "  C " + formatPriceValue(data.close),
          up,
        });
      } else {
        setHover(null);
      }
      // `point` is local to the pane under the cursor: only the price pane can
      // turn a y into a price.
      const onPricePane = param.paneIndex === undefined || param.paneIndex === 0;
      let price = param.point && onPricePane ? bars.coordinateToPrice(param.point.y) : null;
      let timestamp = param.time === undefined ? null : timeToMs(param.time);
      // Magnet: snap crosshair to bar (time) and OHLC (price) on price pane
      if (magnet && onPricePane && timestamp !== null) {
        // snap time to nearest bar
        timestamp = snapTime(candleTimes(), timestamp);
        // snap price to nearest OHLC of that bar
        if (price !== null && data) {
          const candidates = [data.open, data.high, data.low, data.close].filter(
            (v): v is number => typeof v === "number"
          );
          if (candidates.length > 0) {
            let best = candidates[0];
            let bd = Math.abs(price - best);
            for (const c of candidates) {
              const d = Math.abs(price - c);
              if (d < bd) {
                bd = d;
                best = c;
              }
            }
            price = best as BarPrice;
          }
        }
      }
      const position = price !== null && timestamp !== null && Number.isFinite(timestamp)
        ? { timestamp, price }
        : null;
      // Magnet: snap crosshair visually to bar time + OHLC price on price pane.
      // The synthetic call never fires another crosshair event (skipEvent), so
      // it must NOT raise applyingCrosshairRef: doing that used to leave the
      // flag true forever and silently stop the shared crosshair for good.
      if (magnet && onPricePane && price !== null && timestamp !== null) {
        chart.setCrosshairPosition(price, toTime(timestamp), bars);
      }
      if (applyingCrosshairRef.current) {
        pendingPublishRef.current = { position, source: "main" };
      } else {
        publishCrosshair(position, "main");
      }
      // The official vertical-line plugin reports no cursor while we own the
      // drag, so the barrier announces itself here (the chart root is where
      // the library keeps its own cursor override).
      const hoveredId = engineRef.current?.hoveredId() ?? null;
      const overBarrier =
        hoveredId !== null &&
        getState().drawings.some((d) => d.id === hoveredId && d.kind === "vline");
      setBarrierCursor(overBarrier);
      // Live ghost while placing a multi-click tool: pushed straight into the
      // chart primitive, so a mousemove never re-renders React.
      const active = toolRef.current;
      if (
        onPricePane &&
        (active === "zone" || active === "position" || active === "ruler" || active === "arrow") &&
        pendingRef.current.length > 0 &&
        param.point &&
        param.time !== undefined
      ) {
        const price = barsRef.current?.coordinateToPrice(param.point.y) ?? null;
        const ms = timeToMs(param.time as Time);
        if (price !== null && ms !== null) {
          previewRef.current = (() => {
            const sc = snapClick(ms, price);
            return { t: sc.t, p: sc.p };
          })();
          engineRef.current?.setGhost(
            ghostShape(active, pendingRef.current, previewRef.current),
          );
        }
      } else if (previewRef.current) {
        previewRef.current = null;
        engineRef.current?.setGhost(null);
      }
    });

    // Placement clicks are handled manually on the wrapper (mouseup with
    // minimal movement): the chart's own click event swallows rapid clicks
    // (double-click suppression), which silently ate drawing points.

    // Seed with whatever is already in the store.
    candleTimesRef.current = getState().candles.map(([timestamp]) => timestamp);
    const seed = getState().candles;
    pushBars(chart, bars, volume, seed, true, 0);
    renderedRef.current = { len: seed.length, first: seed.length > 0 ? seed[0][0] : 0 };
    versionRef.current = getState().candlesVersion;

    return () => {
      window.clearTimeout(rulerTimer.current);
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("blur", onBlur);
      window.removeEventListener("pointerup", onStrokePointerUp, true);
      window.removeEventListener("pointercancel", onStrokePointerCancel, true);
      document.removeEventListener("visibilitychange", onVisibility);
      adaptivePriceAxis.dispose();
      refreshPriceAxisRef.current = null;
      crosshairListener();
      publishCrosshair(null, "main");
      ro.disconnect();
      dragCleanupRef.current?.();
      dragCleanupRef.current = null;
      engineRef.current?.destroy();
      engineRef.current = null;
      markersRef.current = null;
      chart.remove();
      chartRef.current = null;
      barsRef.current = null;
      volumeRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Clear a half-finished multi-click when the tool changes. Arming a tool
  // also parks hit testing: placement clicks must win over selection.
  useEffect(() => {
    pendingRef.current = [];
    previewRef.current = null;
    engineRef.current?.setGhost(null);
    engineRef.current?.setArmed(tool !== "none");
  }, [tool]);

  // Drawings and the selection that rides on them go straight to the engine.
  useEffect(() => {
    engineRef.current?.sync(drawings, selectedId);
  }, [drawings, selectedId]);

  // Position header (size + base asset) shown by the position primitive.
  useEffect(() => {
    engineRef.current?.setContext({ positionSizeUsdt, symbol });
  }, [positionSizeUsdt, symbol]);

  // Execution layer: closed-trade boxes, entry line + anchor, resting stops.
  useEffect(() => {
    engineRef.current?.setExecution({
      fills,
      positions: account?.positions ?? [],
      orders: account?.open ?? [],
      symbol,
      protections: entryProtections,
    });
  }, [fills, executionKey, symbol, entryProtections]);

  // Incremental data: full snapshot -> setData(), tick -> update().
  useEffect(() => {
    const chart = chartRef.current;
    const bars = barsRef.current;
    const volume = volumeRef.current;
    if (!chart || !bars || !volume) return;
    if (version === versionRef.current) return;
    versionRef.current = version;
    const s = getState();
    const full = s.fullReset;
    // Time axis: extend the cached array instead of rebuilding 10k entries
    // on every tick. Only a reset/symbol switch needs the full map.
    const times = candleTimesRef.current;
    if (
      full ||
      times.length === 0 ||
      s.candles.length < times.length ||
      s.candles[0][0] !== times[0]
    ) {
      candleTimesRef.current = s.candles.map(([timestamp]) => timestamp);
    } else {
      for (let i = times.length; i < s.candles.length; i += 1) times.push(s.candles[i][0]);
      if (times.length > 0 && s.candles.length > 0) {
        times[times.length - 1] = s.candles[s.candles.length - 1][0];
      }
    }
    const rendered = renderedRef.current;
    const paintFull =
      full ||
      rendered.len === 0 ||
      s.candles.length < rendered.len ||
      (s.candles.length > 0 && s.candles[0][0] !== rendered.first);
    pushBars(chart, bars, volume, s.candles, paintFull, rendered.len);
    renderedRef.current = {
      len: s.candles.length,
      first: s.candles.length > 0 ? s.candles[0][0] : 0,
    };
    store.consumedReset();
    if (full) chart.priceScale("right").applyOptions({ autoScale: true });
    followLatest(followRef.current, () => chart.timeScale().scrollToRealTime());
  }, [version]);

  // Account fills as chart markers (rendered from Rust data, UI-only).
  useEffect(() => {
    const markers = markersRef.current;
    if (!markers) return;
    const marksByTime = new Map<number, SeriesMarker<Time>>();
    const times = candleTimes();
    let signedQty = 0;
    const chronological = fills
      .filter((fill) => fill.symbol === symbol)
      .slice()
      .sort(fillChronology);
    for (const f of chronological) {
      const signedFill = f.side === "buy" ? f.amount : -f.amount;
      const priorQty = signedQty;
      signedQty += signedFill;
      if (!times.length || f.timestamp < times[0] || f.timestamp > times[times.length - 1]) continue;
      const markerTime = toTime(snapTime(times, f.timestamp));
      const priorDirection = Math.sign(priorQty);
      const nextDirection = Math.sign(signedQty);
      const action = priorDirection === 0
        ? `ВХОД ${nextDirection > 0 ? "LONG" : "SHORT"}`
        : Math.sign(signedFill) === priorDirection
          ? `ДОБОР ${priorDirection > 0 ? "LONG" : "SHORT"}`
          : Math.abs(signedQty) < 1e-10
            ? `ВЫХОД ${priorDirection > 0 ? "LONG" : "SHORT"}`
            : priorDirection !== nextDirection
              ? `РАЗВОРОТ ${nextDirection > 0 ? "LONG" : "SHORT"}`
              : `СОКРАЩЕНИЕ ${priorDirection > 0 ? "LONG" : "SHORT"}`;
      const key = Number(markerTime);
      const previous = marksByTime.get(key);
      const text = `${action} · ${f.price.toFixed(2)}`;
      marksByTime.set(key, previous
        ? { ...previous, text: `${previous.text} / ${text}` }
        : {
            time: markerTime,
            position: f.side === "buy" ? "belowBar" : "aboveBar",
            color: action.startsWith("ВЫХОД") ? "#e0af68" : f.side === "buy" ? "#3d9a50" : "#d46666",
            shape: f.side === "buy" ? "arrowUp" : "arrowDown",
            text,
          });
    }
    const marks = [...marksByTime.values()];
    marks.sort((a, b) => Number(a.time) - Number(b.time));
    try {
      markers.setMarkers(marks);
    } catch {
      /* timestamps outside the visible series: ignore */
    }
  }, [fills, symbol, candleLen]);

  return (
    <div
      className="chartwrap"
      onMouseDown={(e) => {
        if (e.button !== 0) return;
        // Repair any stale pan/zoom lock before deciding what this press means.
        syncChartInteractions(true);
        downRef.current = { x: e.clientX, y: e.clientY, button: e.button };
        // Freehand brush: start recording points on mouse down. Toolbar
        // buttons floating over the chart are not a canvas stroke.
        if (toolRef.current === "brush") {
          if ((e.target as HTMLElement).closest(".drawbar,.chart-tools,.coin-info")) return;
          const pt = pointFromEvent(e);
          if (!pt) return;
          strokeActiveRef.current = true;
          brushPointsRef.current = (() => {
            const sc = snapClick(pt.t, pt.p);
            return [{ t: sc.t, p: sc.p }];
          })();
          syncChartInteractions();
          return;
        }
        // A tool is armed: the click places a drawing, never drags one.
        if (toolRef.current !== "none") return;
        const id = engineRef.current?.hoveredId() ?? null;
        if (!id) return;
        const drawing = getState().drawings.find((item) => item.id === id);
        if (!drawing) return;
        store.selectDrawing(id);
        startDrawingDrag(drawing, e.clientX, e.clientY);
      }}
      onMouseMove={(e) => {
        // Mouse released outside the window: end the drag now instead of
        // waiting for a pointerup that may never arrive.
        if (dragRef.current && (e.buttons & 1) === 0) endDrawingDrag();
        // Self-heal any stale lock (a stroke/drag that ended invisibly).
        syncChartInteractions();
        if (dragRef.current) moveDrawingDrag(e.clientX, e.clientY);
        if (toolRef.current === "brush" && brushPointsRef.current.length > 0) {
          const pt = pointFromEvent(e);
          if (pt) {
            brushPointsRef.current.push((() => {
              const sc = snapClick(pt.t, pt.p);
              return { t: sc.t, p: sc.p };
            })());
            // Live ghost preview: show the stroke as it is being drawn.
            engineRef.current?.setGhost({ kind: "brush", points: brushPointsRef.current });
          }
        }
      }}
      onMouseUp={(e) => {
        const down = downRef.current;
        downRef.current = null;
        // Finish a freehand brush stroke FIRST: a stroke always moves > 6px,
        // so it must be saved before the pan/click suppression checks below.
        // The window-level pointerup may already have finished it (release
        // outside the chart); finishBrushStroke is idempotent.
        if (toolRef.current === "brush") {
          finishBrushStroke();
          suppressClickRef.current = false;
          return;
        }
        if (suppressClickRef.current) {
          suppressClickRef.current = false;
          return; // a drawing was dragged: not a placement click
        }
        if (!down || e.button !== 0) return;
        const moved = Math.hypot(e.clientX - down.x, e.clientY - down.y);
        if (moved > 6) return; // chart pan, not a click
        if ((e.target as HTMLElement).closest(".drawbar,.chart-tools,.coin-info")) return;
        const active = toolRef.current;
        if (active === "none") {
          restoreRuler();
          return;
        }
        const pt = pointFromEvent(e);
        if (!pt) return;
        const sc = snapClick(pt.t, pt.p);
        placeAt(sc.t, sc.p);
      }}
      onContextMenu={(e) => {
        // Right-click on a drawing deletes it — the hit comes from the chart's
        // own hit test, so the pixels you aim at are the pixels that count.
        const id = engineRef.current?.hoveredId();
        if (!id) return;
        e.preventDefault();
        e.stopPropagation();
        store.removeDrawing(id);
      }}
    >
      <div ref={containerRef} className="chart" />
      <DrawBar
        magnet={magnet}
        onToggleMagnet={() => setMagnet(!magnet)}
        onScreenshot={async () => {
          try {
            const canvas = await chartRef.current?.takeScreenshot?.(true, true);
            if (canvas) {
              const dataUrl = canvas.toDataURL("image/png");
              const a = document.createElement("a");
              a.href = dataUrl;
              a.download = `simccxt-${Date.now()}.png`;
              a.click();
            }
          } catch {}
        }}
      />
      <div className="coin-info">
        <div className="coin-sym">{symbol}</div>
        {hover && (
          <div className="coin-ohlc" style={{ color: hover.up ? "#3d9a50" : "#d46666" }}>
            {hover.text}
          </div>
        )}
        {criteria.length > 0 && (
          <div className="coin-chips">
            {criteria.slice(0, 8).map((id) => (
              <span key={id} className="chip">✓ {criterionLabel(id)}</span>
            ))}
            {criteria.length > 8 && <span className="chip more">+{criteria.length - 8}</span>}
          </div>
        )}
        {tool !== "none" && <div className="coin-status">{TOOL_HINTS[tool]}</div>}
      </div>
      <div className="chart-tools">
        <button
          className={follow ? "btn b-acc" : "btn b-dim"}
          onClick={() => {
            const v = !followRef.current;
            followRef.current = v;
            setFollow(v);
            const chart = chartRef.current;
            if (chart) followLatest(v, () => chart.timeScale().scrollToRealTime());
          }}
        >
          Follow: {follow ? "ON" : "OFF"}
        </button>
      </div>
    </div>
  );

  // ---- coordinate helpers ----

  function candleTimes(): number[] {
    // Cached per candlesVersion change (line 534/467): this runs on every
    // mousemove during drag/brush — a full map over ~17k candles per event
    // froze the page under a paint-style stroke.
    return candleTimesRef.current;
  }

  function snapClick(ms: number, price: number): { t: number; p: number } {
    // Time always snaps to the bar. Magnet: price snaps to the nearest
    // OHLC value of the bar under the cursor (TradingView-style magnet).
    const t = snapTime(candleTimes(), ms);
    if (!magnet) return { t, p: price };
    const candles = getState().candles;
    if (!candles.length) return { t, p: price };
    const times = candleTimes();
    // binary search for the bar index
    let lo = 0;
    let hi = times.length - 1;
    if (t <= times[0]) lo = hi = 0;
    else if (t >= times[times.length - 1]) lo = hi = times.length - 1;
    else {
      while (hi - lo > 1) {
        const mid = (lo + hi) >> 1;
        if (times[mid] <= t) lo = mid;
        else hi = mid;
      }
      lo = t - times[lo] <= times[hi] - t ? lo : hi;
    }
    const [, open, high, low, close] = candles[lo];
    const candidates = [open, high, low, close];
    let best = candidates[0];
    let bd = Math.abs(price - best);
    for (const c of candidates) {
      const d = Math.abs(price - c);
      if (d < bd) {
        bd = d;
        best = c;
      }
    }
    return { t, p: best };
  }

  /** Container-relative pixel -> bar time (exact inverse of timeToCoordinate). */
  function xToMs(x: number): number | null {
    const ts = chartRef.current?.timeScale();
    if (!ts) return null;
    const times = candleTimes();
    if (!times.length) return null;
    return invertTime(x, times, (ms) => {
      const v = ts.timeToCoordinate(toTime(ms));
      return typeof v === "number" ? v : null;
    });
  }

  /**
   * Pane-local y -> price, but only inside the price pane: extrapolating below
   * it would turn the same y into an unrelated price.
   */
  function yToPrice(y: number): number | null {
    const chart = chartRef.current;
    if (!chart || y >= chart.paneSize(0).height) return null;
    const v = barsRef.current?.coordinateToPrice(y);
    return typeof v === "number" ? v : null;
  }

  /**
   * The official vertical-line plugin reports no cursor while `draggable` is
   * off (the drag is ours), so the barrier announces `ew-resize` from here.
   * The library keeps its own override on the chart root and rewrites it on
   * every crosshair move, so this only has to re-state ours while it is over.
   */
  function setBarrierCursor(over: boolean): void {
    const root = containerRef.current?.firstElementChild as HTMLElement | null;
    if (!root) return;
    const held = cursorOverrideRef.current;
    if (over) {
      if (!held || held.element !== root) {
        cursorOverrideRef.current = { element: root, previous: root.style.cursor };
      }
      root.style.cursor = "ew-resize";
    } else if (held && held.element === root && root.style.cursor === "ew-resize") {
      cursorOverrideRef.current = null;
      root.style.cursor = held.previous;
    }
  }

  function pointFromEvent(e: React.MouseEvent): Pt | null {
    const rect = containerRef.current?.getBoundingClientRect();
    if (!rect) return null;
    const t = xToMs(e.clientX - rect.left);
    const p = yToPrice(e.clientY - rect.top);
    if (t === null || p === null) return null;
    return { t, p };
  }

  // ---- freehand brush: one stroke owns pan/zoom, then gives it back ----

  /** Finish the current stroke: save it, disarm the tool, restore pan/zoom. */
  function finishBrushStroke(): void {
    if (!strokeActiveRef.current) return;
    strokeActiveRef.current = false;
    const pts = brushPointsRef.current;
    brushPointsRef.current = [];
    engineRef.current?.setGhost(null);
    if (pts.length > 1) {
      store.addDrawing({ kind: "brush", points: pts, color: "#4a9ec4", title: "ручка" });
    }
    store.setTool("none");
    syncChartInteractions();
  }

  /** Abort the stroke (Escape / window blur) without saving it. */
  function cancelBrushStroke(): void {
    if (!strokeActiveRef.current) return;
    strokeActiveRef.current = false;
    brushPointsRef.current = [];
    engineRef.current?.setGhost(null);
    syncChartInteractions();
  }

  // ---- placement state machine ----

  function placeAt(t: number, price: number): void {
    const active = toolRef.current;
    if (active === "none") {
      restoreRuler();
      return;
    }
    const pend = pendingRef.current;
    try {
      if (active === "ray") {
        store.addDrawing({ kind: "ray", t0: t, price, color: "#4a9ec4", title: "луч " + price.toFixed(1) });
        store.setTool("none");
      } else if (active === "note") {
        const text = window.prompt("Текст заметки:", "");
        if (text && text.trim()) {
          store.addDrawing({ kind: "note", t0: t, price, color: "#e0af68", title: text.trim() });
        }
        store.setTool("none");
      } else if (active === "vline") {
        const dt = new Date(t);
        const hh = String(dt.getUTCHours()).padStart(2, "0");
        const mm = String(dt.getUTCMinutes()).padStart(2, "0");
        store.addDrawing({ kind: "vline", t, color: "#e0af68", title: "барьер " + hh + ":" + mm });
        store.setTool("none");
        replayFrom(t);
      } else {
        pend.push({ t, p: price });
        const need = active === "position" ? 3 : 2;
        if (pend.length < need) return;
        if (active === "zone") {
          const [a, b] = pend;
          store.addDrawing({
            kind: "zone", t1: a.t, p1: a.p, t2: b.t, p2: b.p,
            color: "#4a9ec4",
            title: "зона " + Math.min(a.p, b.p).toFixed(0) + "–" + Math.max(a.p, b.p).toFixed(0),
          });
        } else if (active === "position") {
          const [e, s, tp] = pend;
          const dir = positionDirection(e.p, s.p);
          const rr = riskReward(e.p, s.p, tp.p);
          store.addDrawing({
            kind: "position", t1: e.t, t2: tp.t, entry: e.p, sl: s.p, tp: tp.p,
            color: dir === "long" ? "#3d9a50" : "#d46666",
            title: (dir === "long" ? "LONG " : "SHORT ") + (rr === null ? "—" : rr.toFixed(2) + "R"),
          });
        } else if (active === "arrow") {
          const [a, b] = pend;
          store.addDrawing({
            kind: "arrow", t0: a.t, price: a.p, t1: b.t, price1: b.p,
            color: "#4a9ec4",
            title: "стрелка",
          });
        } else if (active === "ruler") {
          const [a, b] = pend;
          placeRuler(a, b);
        }
        pendingRef.current = [];
        previewRef.current = null;
        if (active !== "ruler") store.setTool("none");
      }
      // The click landed: no more ghost frame to draw.
      previewRef.current = null;
      engineRef.current?.setGhost(null);
    } catch (err) {
      // eslint-disable-next-line no-console
      console.error("[draw] failed", err);
    }
  }

  // ---- ruler (transient local measurement, auto-fades) ----

  function placeRuler(a: Pt, b: Pt): void {
    window.clearTimeout(rulerTimer.current);
    rulerRef.current = { a, b, shown: true };
    engineRef.current?.setRuler({ a, b });
    rulerTimer.current = window.setTimeout(() => {
      const r = rulerRef.current;
      if (!r) return;
      r.shown = false;
      engineRef.current?.fadeRuler(RULER_FADE_ANIM_MS);
    }, RULER_FADE_MS);
  }

  function restoreRuler(): void {
    const r = rulerRef.current;
    if (!r || r.shown) return;
    placeRuler(r.a, r.b);
  }

  // ---- replay barrier (vline): restart the replay clock at a bar ----

  /**
   * Point the Rust clock at `ms` and reload the chart from there.
   * Bars right of the barrier disappear; play reveals them one by one
   * (reset starts paused). Positions/fills are untouched — only time moves.
   */
  function replayFrom(ms: number): void {
    const s = getState();
    sendSim({ type: "replay", action: "reset", timestamp_ms: ms });
    subscribeSim(s.symbol, s.timeframe, s.exchange);
  }

  // ---- dragging existing drawings (hit comes from the chart's hit test) ----

  /** Move-anchor of a drawing (time, price point that follows the cursor). */
  function anchorOf(d: Drawing): Pt {
    switch (d.kind) {
      case "ray": return { t: d.t0, p: d.price };
      case "vline": return { t: d.t, p: 0 };
      case "zone": return { t: Math.min(d.t1, d.t2), p: Math.max(d.p1, d.p2) };
      case "position": return { t: Math.min(d.t1, d.t2), p: d.entry };
      case "note": return { t: d.t0, p: d.price };
      case "arrow": return { t: d.t0, p: d.price };
      case "brush": return d.points.length ? { t: d.points[0].t, p: d.points[0].p } : { t: 0, p: 0 };
    }
  }

  /**
   * Begin dragging the drawing under the cursor. Scroll/scale are parked for
   * the duration — the same trick the official vertical-line plugin uses — so
   * the chart does not pan underneath the drawing being moved.
   */
  function startDrawingDrag(drawing: Drawing, clientX: number, clientY: number): void {
    const rect = containerRef.current?.getBoundingClientRect();
    const chart = chartRef.current;
    if (!rect || !chart) return;
    const mt = xToMs(clientX - rect.left);
    const mp = yToPrice(clientY - rect.top);
    if (mt === null || mp === null) return;
    const anchor = anchorOf(drawing);
    dragRef.current = {
      id: drawing.id,
      orig: drawing,
      offT: anchor.t - mt,
      offP: anchor.p - mp,
      moved: false,
    };
    const onMove = (event: PointerEvent): void => {
      // If the button was already released (e.g. mouseup outside the window),
      // finish the drag here instead of getting stuck in drag mode forever.
      if (event.buttons === 0) {
        endDrawingDrag();
        return;
      }
      moveDrawingDrag(event.clientX, event.clientY);
    };
    const onUp = (): void => endDrawingDrag();
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp, { once: true });
    window.addEventListener("pointercancel", onUp, { once: true });
    dragCleanupRef.current = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onUp);
    };
    syncChartInteractions();
  }

  function moveDrawingDrag(clientX: number, clientY: number): void {
    const drag = dragRef.current;
    if (!drag) return;
    const rect = containerRef.current?.getBoundingClientRect();
    if (!rect) return;
    const mt = xToMs(clientX - rect.left);
    const mp = yToPrice(clientY - rect.top);
    if (mt === null || mp === null) return;
    const rows = candleTimes();
    // New anchor = cursor + grab offset, snapped to bars; shift = vs original.
    const anchor = anchorOf(drag.orig);
    const nt = snapTime(rows, mt + drag.offT);
    const np = mp + drag.offP;
    const dT = nt - anchor.t;
    const dP = np - anchor.p;
    if (dT === 0 && dP === 0) return;
    suppressClickRef.current = true; // a real drag: no placement click after it
    drag.moved = true;
    const o = drag.orig;
    if (o.kind === "ray") {
      store.updateDrawing(drag.id, (x) =>
        x.kind === "ray" ? { ...x, t0: snapTime(rows, o.t0 + dT), price: o.price + dP } : x);
    } else if (o.kind === "note") {
      store.updateDrawing(drag.id, (x) =>
        x.kind === "note" ? { ...x, t0: snapTime(rows, o.t0 + dT), price: o.price + dP } : x);
    } else if (o.kind === "vline") {
      store.updateDrawing(drag.id, (x) =>
        x.kind === "vline" ? { ...x, t: snapTime(rows, o.t + dT) } : x);
    } else if (o.kind === "zone") {
      store.updateDrawing(drag.id, (x) =>
        x.kind === "zone"
          ? { ...x, t1: snapTime(rows, o.t1 + dT), t2: snapTime(rows, o.t2 + dT),
              p1: o.p1 + dP, p2: o.p2 + dP }
          : x);
    } else if (o.kind === "position") {
      store.updateDrawing(drag.id, (x) =>
        x.kind === "position"
          ? { ...x, t1: snapTime(rows, o.t1 + dT), t2: snapTime(rows, o.t2 + dT),
              entry: o.entry + dP, sl: o.sl + dP, tp: o.tp + dP }
          : x);
    } else if (o.kind === "arrow") {
      store.updateDrawing(drag.id, (x) =>
        x.kind === "arrow"
          ? { ...x, t0: snapTime(rows, o.t0 + dT), t1: snapTime(rows, o.t1 + dT),
              price: o.price + dP, price1: o.price1 + dP }
          : x);
    } else if (o.kind === "brush") {
      store.updateDrawing(drag.id, (x) =>
        x.kind === "brush"
          ? { ...x, points: o.points.map((pt) => ({ t: snapTime(rows, pt.t + dT), p: pt.p + dP })) }
          : x);
    }
  }

  function endDrawingDrag(): void {
    dragCleanupRef.current?.();
    dragCleanupRef.current = null;
    const drag = dragRef.current;
    dragRef.current = null;
    suppressClickRef.current = false;
    syncChartInteractions();
    // A moved barrier re-points the replay clock (drag end only, not per move).
    if (drag && drag.moved) {
      const d = getState().drawings.find((x) => x.id === drag.id);
      if (d && d.kind === "vline") replayFrom(d.t);
    }
  }
}
/**
 * Which ghost shape the placement state machine should show: `pending` holds
 * the clicks already placed, `mouse` the live cursor point.
 */
function ghostShape(tool: Tool, pending: Pt[], mouse: Pt): GhostShape | null {
  if (pending.length === 0) return null;
  if (tool === "zone") return { kind: "zone", a: pending[0], b: mouse };
  if (tool === "arrow") return { kind: "arrow", a: pending[0], b: mouse };
  if (tool === "ruler") return { kind: "ruler", a: pending[0], b: mouse };
  if (tool === "position") {
    const entry = pending[0];
    if (pending.length === 1) return { kind: "position", entry, sl: mouse, tp: null };
    return { kind: "position", entry, sl: pending[1], tp: mouse };
  }
  return null;
}

function pushBars(
  chart: IChartApi,
  bars: ISeriesApi<"Bar">,
  volume: ISeriesApi<"Histogram">,
  rows: CandleRow[],
  full: boolean,
  from: number,
): void {
  if (!rows.length) {
    bars.setData([]);
    volume.setData([]);
    return;
  }
  // Tick size follows the price magnitude: a 0.09 DOGE needs five decimals or
  // the axis rounds to whole cents and the trader cannot read the price.
  const last = rows[rows.length - 1];
  bars.applyOptions({
    priceFormat: { type: "price", ...priceAxisFormat(last[4], bars.priceScale().getVisibleRange()) },
  });
  if (full) {
    bars.setData(
      rows.map((r) => ({ time: toTime(r[0]), open: r[1], high: r[2], low: r[3], close: r[4] })),
    );
    volume.setData(
      rows.map((r) => ({
        time: toTime(r[0]),
        value: r[5],
        color: r[4] >= r[1] ? "rgba(61,154,80,0.4)" : "rgba(212,102,102,0.4)",
      })),
    );
    chart.timeScale().fitContent();
  } else {
    // One message can append several bars (a stalled link, a fast replay):
    // repaint from the previously forming bar to the tail, not just the last.
    const start = Math.max(0, Math.min(from - 1, rows.length - 1));
    for (let i = start; i < rows.length; i += 1) {
      const r = rows[i];
      bars.update({ time: toTime(r[0]), open: r[1], high: r[2], low: r[3], close: r[4] });
      volume.update({
        time: toTime(r[0]),
        value: r[5],
        color: r[4] >= r[1] ? "rgba(61,154,80,0.4)" : "rgba(212,102,102,0.4)",
      });
    }
  }
}
