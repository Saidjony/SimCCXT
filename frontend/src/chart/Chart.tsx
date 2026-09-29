import { useEffect, useRef, useState } from "react";
import {
  ColorType,
  CrosshairMode,
  createChart,
} from "lightweight-charts";
import type {
  IChartApi,
  ISeriesApi,
  MouseEventParams,
  SeriesMarker,
  Time,
  UTCTimestamp,
} from "lightweight-charts";
import type { CandleRow } from "../types/protocol";
import { getState, store, useSim } from "../state/store";
import { sendSim } from "../websocket/connection";
import type { Drawing, Tool } from "../state/store";
import { criterionLabel } from "../trade/criteria";
import {
  countBars,
  cumulativeDelta,
  formatRuler,
  invertTime,
  positionBands,
  positionDirection,
  profileBarLayout,
  riskReward,
  rulerStats,
  snapTime,
  toBox,
  volumeProfile,
} from "./overlays";
import { DrawBar } from "./DrawBar";

function toTime(ms: number): UTCTimestamp {
  return Math.floor(ms / 1000) as UTCTimestamp;
}

function loadOiPaneHeight(): number {
  try {
    const saved = Number(window.localStorage.getItem("simccxt.oi-pane-height"));
    return Number.isFinite(saved) && saved >= 72 ? saved : 120;
  } catch {
    return 120;
  }
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
const VOLUME_PROFILE_LEVELS = 64;

const TOOL_HINTS: Record<Exclude<Tool, "none">, string> = {
  ray: "клик — луч-уровень от этой точки · drag — двигать",
  vline: "клик — барьер: бары справа скроются, play покажет по одному",
  zone: "2 клика — углы прямоугольника · drag — двигать",
  position: "3 клика — вход, стоп, тейк · drag — двигать",
  ruler: "2 клика — замер-прямоугольник (гаснет через 4 с)",
};

/**
 * OHLC bar chart. All interaction (crosshair, zoom, pan, drawings)
 * stays in the browser — nothing is sent to Rust.
 * Incremental updates: snapshot -> setData(), deltas -> update().
 * Every drawing is an HTML overlay: uniform drag, right-click delete,
 * live ghost preview while placing.
 */
export function Chart(): JSX.Element {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const barsRef = useRef<ISeriesApi<"Bar"> | null>(null);
  const volumeRef = useRef<ISeriesApi<"Histogram"> | null>(null);
  const oiWrapRef = useRef<HTMLDivElement | null>(null);
  const cvdWrapRef = useRef<HTMLDivElement | null>(null);
  const oiChartRef = useRef<IChartApi | null>(null);
  const cvdChartRef = useRef<IChartApi | null>(null);
  const oiSeriesRef = useRef<ISeriesApi<"Line"> | null>(null);
  const cvdSeriesRef = useRef<ISeriesApi<"Line"> | null>(null);
  /** Guard against ping-pong when syncing the two time scales. */
  const syncingRef = useRef(false);
  const scaleSyncRafRef = useRef(0);
  const pendingRef = useRef<Pt[]>([]);
  const previewRef = useRef<Pt | null>(null);
  const rafRef = useRef(0);
  const rulerTimer = useRef(0);
  const dragRef = useRef<{
    id: string;
    orig: Drawing;
    offT: number;
    offP: number;
  } | null>(null);
  /** Mouse-down anchor for manual click detection (pan-safe placement). */
  const downRef = useRef<{ x: number; y: number; button: number } | null>(null);
  /** Set when an overlay was actually dragged: suppress the click that follows. */
  const suppressClickRef = useRef(false);
  const versionRef = useRef(0);
  const [follow, setFollow] = useState(true);
  const [oiPaneHeight, setOiPaneHeight] = useState(loadOiPaneHeight);
  const oiPaneHeightRef = useRef(oiPaneHeight);
  const followRef = useRef(true);
  const [hover, setHover] = useState<HoverBar | null>(null);
  const [ruler, setRuler] = useState<RulerState | null>(null);
  const rulerRef = useRef<RulerState | null>(null);
  rulerRef.current = ruler;
  // Bumped on pan/zoom/data/mousemove so HTML overlays reposition during render.
  const [viewTick, setViewTick] = useState(0);

  const version = useSim((s) => s.candlesVersion);
  const drawings = useSim((s) => s.drawings);
  const tool = useSim((s) => s.tool);
  const fills = useSim((s) => s.fills);
  const symbol = useSim((s) => s.symbol);
  const selectedId = useSim((s) => s.selectedId);
  const positionSizeUsdt = useSim((s) => s.positionSizeUsdt);
  const criteria = useSim((s) => s.chartCriteria[s.timeframe] ?? []);
  const oiVisible = useSim((s) => s.oiVisible);
  const cvdVisible = useSim((s) => s.cvdVisible);
  const oiPeriod = useSim((s) => s.oiPeriod);
  const toolRef = useRef(tool);
  toolRef.current = tool;
  const marketData = getState();
  const oiLabel = marketData.oi.length === 0
    ? "OI · нет данных"
    : "OI · " + (oiPeriod || "история");

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
      crosshair: { mode: CrosshairMode.Normal },
      rightPriceScale: { borderColor: "#222c3b" },
      timeScale: { borderColor: "#222c3b", timeVisible: true, secondsVisible: false },
    });
    const bars = chart.addBarSeries({
      upColor: "#3d9a50",
      downColor: "#d46666",
      openVisible: true,
      thinBars: false,
    });
    const volume = chart.addHistogramSeries({
      priceScaleId: "",
      priceFormat: { type: "volume" },
    });
    chart.priceScale("").applyOptions({ scaleMargins: { top: 0.85, bottom: 0 } });
    chartRef.current = chart;
    barsRef.current = bars;
    volumeRef.current = volume;

    const ro = new ResizeObserver(() => {
      chart.applyOptions({ width: el.clientWidth, height: el.clientHeight });
      setViewTick((n) => n + 1);
    });
    ro.observe(el);
    let priceScaleRaf = 0;
    const onPriceScaleMove = (event: PointerEvent): void => {
      const x = event.clientX - el.getBoundingClientRect().left;
      if (x < el.clientWidth - 72 || priceScaleRaf) return;
      priceScaleRaf = window.requestAnimationFrame(() => {
        priceScaleRaf = 0;
        setViewTick((n) => n + 1);
      });
    };
    el.addEventListener("pointermove", onPriceScaleMove);
    const bump = (): void => setViewTick((n) => n + 1);
    chart.timeScale().subscribeVisibleLogicalRangeChange(bump);

    const onKey = (e: KeyboardEvent): void => {
      const tag = (e.target as HTMLElement | null)?.tagName ?? "";
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;
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
      }
    };
    window.addEventListener("keydown", onKey);

    // Hover readout + ghost-preview mouse tracking (local, no roundtrip).
    chart.subscribeCrosshairMove((param: MouseEventParams) => {
      const data = param.seriesData?.get(bars) as
        | { open?: number; high?: number; low?: number; close?: number }
        | undefined;
      if (data && typeof data.close === "number" && typeof data.open === "number") {
        const up = data.close >= data.open;
        setHover({
          text:
            "O " + data.open.toFixed(1) + "  H " + (data.high ?? 0).toFixed(1) +
            "  L " + (data.low ?? 0).toFixed(1) + "  C " + data.close.toFixed(1),
          up,
        });
      } else {
        setHover(null);
      }
      // Ghost preview while placing multi-click tools.
      const active = toolRef.current;
      if (
        (active === "zone" || active === "position" || active === "ruler") &&
        pendingRef.current.length > 0 &&
        param.point &&
        param.time !== undefined
      ) {
        const price = barsRef.current?.coordinateToPrice(param.point.y) ?? null;
        const ms = timeToMs(param.time as Time);
        if (price !== null && ms !== null) {
          previewRef.current = { t: snapClick(ms), p: price };
          if (!rafRef.current) {
            rafRef.current = window.requestAnimationFrame(() => {
              rafRef.current = 0;
              setViewTick((n) => n + 1);
            });
          }
        }
      } else if (previewRef.current) {
        previewRef.current = null;
      }
    });

    // Placement clicks are handled manually on the wrapper (mouseup with
    // minimal movement): the chart's own click event swallows rapid clicks
    // (double-click suppression), which silently ate drawing points.

    // Seed with whatever is already in the store.
    pushBars(chart, bars, volume, getState().candles, true);
    versionRef.current = getState().candlesVersion;

    return () => {
      window.clearTimeout(rulerTimer.current);
      if (rafRef.current) window.cancelAnimationFrame(rafRef.current);
      if (priceScaleRaf) window.cancelAnimationFrame(priceScaleRaf);
      window.removeEventListener("keydown", onKey);
      el.removeEventListener("pointermove", onPriceScaleMove);
      ro.disconnect();
      chart.timeScale().unsubscribeVisibleLogicalRangeChange(bump);
      if (scaleSyncRafRef.current) {
        window.cancelAnimationFrame(scaleSyncRafRef.current);
        scaleSyncRafRef.current = 0;
      }
      chart.remove();
      chartRef.current = null;
      barsRef.current = null;
      volumeRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Clear a half-finished multi-click when the tool changes.
  useEffect(() => {
    pendingRef.current = [];
    previewRef.current = null;
  }, [tool]);

  // Incremental data: full snapshot -> setData(), tick -> update().
  useEffect(() => {
    const chart = chartRef.current;
    const bars = barsRef.current;
    const volume = volumeRef.current;
    if (!chart || !bars || !volume) return;
    if (version === versionRef.current) return;
    versionRef.current = version;
    const s = getState();
    syncingRef.current = true;
    try {
      pushBars(chart, bars, volume, s.candles, s.fullReset);
      store.consumedReset();
      if (s.fullReset) chart.priceScale("right").applyOptions({ autoScale: true });
      if (followRef.current) chart.timeScale().scrollToRealTime();
      // OI has its own time axis; CVD remains aligned to the candle range.
      if (s.fullReset && oiSeriesRef.current && oiChartRef.current) {
        pushOi(oiSeriesRef.current, s.oi);
        oiChartRef.current.timeScale().fitContent();
      }
      if (cvdSeriesRef.current) {
        pushCvd(cvdSeriesRef.current, s.candles);
      }
    } finally {
      syncingRef.current = false;
    }
    if (cvdChartRef.current) {
      syncCvdToMain();
      scheduleCvdSync();
    }
    setViewTick((n) => n + 1);
  }, [version]);

  // Account fills as chart markers (rendered from Rust data, UI-only).
  useEffect(() => {
    const bars = barsRef.current;
    if (!bars) return;
    const marks: SeriesMarker<Time>[] = [];
    for (const f of fills.slice(0, 200)) {
      marks.push({
        time: toTime(f.timestamp),
        position: f.side === "buy" ? "belowBar" : "aboveBar",
        color: f.side === "buy" ? "#3d9a50" : "#d46666",
        shape: f.side === "buy" ? "arrowUp" : "arrowDown",
        text: (f.side === "buy" ? "B " : "S ") + f.price.toFixed(0),
      });
    }
    marks.sort((a, b) => Number(a.time) - Number(b.time));
    try {
      bars.setMarkers(marks);
    } catch {
      /* timestamps outside the visible series: ignore */
    }
  }, [fills]);

  // OI gets its own fitted time axis so its shorter history is always visible.
  // CVD remains tied to the main chart's candle range.
  useEffect(() => {
    const observers: ResizeObserver[] = [];
    const main = chartRef.current;
    let cvdRangeListener: (() => void) | null = null;
    let mainRangeListener: (() => void) | null = null;

    if (oiVisible && oiWrapRef.current) {
      const el = oiWrapRef.current;
      const chart = createChart(el, indicatorChartOptions(el, false));
      const line = chart.addLineSeries({
        color: "#e0af68",
        lineWidth: 2,
        priceLineVisible: false,
        crosshairMarkerVisible: true,
      });
      oiChartRef.current = chart;
      oiSeriesRef.current = line;
      pushOi(line, getState().oi);
      chart.timeScale().fitContent();
      const observer = new ResizeObserver(() => {
        chart.applyOptions({ width: el.clientWidth, height: el.clientHeight });
      });
      observer.observe(el);
      observers.push(observer);
    } else {
      oiChartRef.current = null;
      oiSeriesRef.current = null;
    }

    if (cvdVisible && cvdWrapRef.current) {
      const el = cvdWrapRef.current;
      const chart = createChart(el, indicatorChartOptions(el, true));
      const line = chart.addLineSeries({
        color: "#4a9ec4",
        lineWidth: 1,
        priceScaleId: "left",
        priceLineVisible: false,
        crosshairMarkerVisible: true,
      });
      cvdChartRef.current = chart;
      cvdSeriesRef.current = line;
      pushCvd(line, getState().candles);
      syncCvdToMain();
      const observer = new ResizeObserver(() => {
        chart.applyOptions({ width: el.clientWidth, height: el.clientHeight });
        scheduleCvdSync();
      });
      observer.observe(el);
      observers.push(observer);
      mainRangeListener = (): void => {
        if (syncingRef.current) return;
        syncCvdToMain();
      };
      cvdRangeListener = (): void => {
        if (syncingRef.current || !main) return;
        const range = chart.timeScale().getVisibleLogicalRange();
        if (!range) return;
        syncingRef.current = true;
        try {
          main.timeScale().setVisibleLogicalRange(range);
        } catch {
          // Main and CVD share candle timestamps; a transient empty range is ignored.
        } finally {
          syncingRef.current = false;
        }
      };
      main?.timeScale().subscribeVisibleLogicalRangeChange(mainRangeListener);
      chart.timeScale().subscribeVisibleLogicalRangeChange(cvdRangeListener);
    } else {
      cvdChartRef.current = null;
      cvdSeriesRef.current = null;
      main?.priceScale("left").applyOptions({ visible: false, minimumWidth: 0 });
      main?.priceScale("right").applyOptions({ minimumWidth: 0 });
    }

    return () => {
      if (mainRangeListener) main?.timeScale().unsubscribeVisibleLogicalRangeChange(mainRangeListener);
      if (cvdRangeListener && cvdChartRef.current) {
        cvdChartRef.current.timeScale().unsubscribeVisibleLogicalRangeChange(cvdRangeListener);
      }
      observers.forEach((observer) => observer.disconnect());
      oiChartRef.current?.remove();
      cvdChartRef.current?.remove();
      oiChartRef.current = null;
      cvdChartRef.current = null;
      oiSeriesRef.current = null;
      cvdSeriesRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [oiVisible, cvdVisible]);

  // Toggle a bottom-pane series without rebuilding the chart.
  return (
    <div
      className="chartwrap"
      onMouseDown={(e) => {
        if (e.button !== 0) return;
        downRef.current = { x: e.clientX, y: e.clientY, button: e.button };
      }}
      onMouseUp={(e) => {
        const down = downRef.current;
        downRef.current = null;
        if (suppressClickRef.current) {
          suppressClickRef.current = false;
          return; // overlay drag just ended: not a placement click
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
        placeAt(snapClick(pt.t), pt.p);
      }}
    >
      <div ref={containerRef} className="chart" />
      {oiVisible && (
        <div
          ref={oiWrapRef}
          className="oi-wrap"
          style={{ flexBasis: cvdVisible ? oiPaneHeight / 2 : oiPaneHeight, height: cvdVisible ? oiPaneHeight / 2 : oiPaneHeight }}
        >
          <div
            className="oi-resize-handle"
            role="separator"
            aria-label="Изменить высоту панели индикаторов"
            aria-orientation="horizontal"
            aria-valuemin={72}
            aria-valuemax={Math.round((containerRef.current?.parentElement?.clientHeight ?? 600) * 0.45)}
            aria-valuenow={Math.round(oiPaneHeight)}
            onPointerDown={startOiPaneResize}
            onKeyDown={(event) => {
              if (event.key === "ArrowUp") updateOiPaneHeight(oiPaneHeight + 8);
              if (event.key === "ArrowDown") updateOiPaneHeight(oiPaneHeight - 8);
            }}
            tabIndex={0}
            title="Перетащите, чтобы изменить высоту индикаторов"
          />
          <div className="oi-label">
            {oiLabel}
          </div>
        </div>
      )}
      {cvdVisible && (
        <div
          ref={cvdWrapRef}
          className="oi-wrap cvd-wrap"
          style={{ flexBasis: oiVisible ? oiPaneHeight / 2 : oiPaneHeight, height: oiVisible ? oiPaneHeight / 2 : oiPaneHeight }}
        >
          {!oiVisible && (
            <div
              className="oi-resize-handle"
              role="separator"
              aria-label="Изменить высоту панели индикаторов"
              aria-orientation="horizontal"
              aria-valuemin={72}
              aria-valuemax={Math.round((containerRef.current?.parentElement?.clientHeight ?? 600) * 0.45)}
              aria-valuenow={Math.round(oiPaneHeight)}
              onPointerDown={startOiPaneResize}
              onKeyDown={(event) => {
                if (event.key === "ArrowUp") updateOiPaneHeight(oiPaneHeight + 8);
                if (event.key === "ArrowDown") updateOiPaneHeight(oiPaneHeight - 8);
              }}
              tabIndex={0}
              title="Перетащите, чтобы изменить высоту индикаторов"
            />
          )}
          <div className="oi-label">CVD</div>
        </div>
      )}
      <DrawBar />
      {renderOverlays()}
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
          className={oiVisible ? "btn b-acc" : "btn b-dim"}
          onClick={() => store.setOiVisible(!oiVisible)}
          title="Открытый интерес — панель под графиком"
        >
          OI
        </button>
        <button
          className={cvdVisible ? "btn b-acc" : "btn b-dim"}
          onClick={() => store.setCvdVisible(!cvdVisible)}
          title="Cumulative volume delta — линия в нижней панели"
        >
          CVD
        </button>
        <button
          className={follow ? "btn b-acc" : "btn b-dim"}
          onClick={() => {
            const v = !followRef.current;
            followRef.current = v;
            setFollow(v);
            if (v) chartRef.current?.timeScale().scrollToRealTime();
          }}
        >
          Follow: {follow ? "ON" : "OFF"}
        </button>
      </div>
    </div>
  );

  // ---- coordinate helpers ----

  function candleTimes(): number[] {
    return getState().candles.map((r) => r[0]);
  }

  function snapClick(ms: number): number {
    return snapTime(candleTimes(), ms);
  }

  function xFor(ms: number): number | null {
    const v = chartRef.current?.timeScale().timeToCoordinate(toTime(ms));
    return typeof v === "number" ? v : null;
  }

  function yFor(price: number): number | null {
    const v = barsRef.current?.priceToCoordinate(price);
    return typeof v === "number" ? v : null;
  }

  /** Container-relative pixel -> bar time (exact inverse of timeToCoordinate). */
  function xToMs(x: number): number | null {
    const ts = chartRef.current?.timeScale();
    if (!ts) return null;
    const rows = getState().candles;
    if (!rows.length) return null;
    const times = rows.map((r) => r[0]);
    return invertTime(x, times, (ms) => {
      const v = ts.timeToCoordinate(toTime(ms));
      return typeof v === "number" ? v : null;
    });
  }

  function yToPrice(y: number): number | null {
    const v = barsRef.current?.coordinateToPrice(y);
    return typeof v === "number" ? v : null;
  }

  function startOiPaneResize(event: React.PointerEvent<HTMLDivElement>): void {
    event.preventDefault();
    const startY = event.clientY;
    const initialHeight = oiPaneHeight;
    const chartHeight = containerRef.current?.parentElement?.clientHeight ?? 600;
    const move = (pointer: PointerEvent): void => {
      updateOiPaneHeight(initialHeight + startY - pointer.clientY, chartHeight, false);
    };
    const stop = (): void => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      persistOiPaneHeight(oiPaneHeightRef.current);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop, { once: true });
  }

  function updateOiPaneHeight(requested: number, chartHeight?: number, persist = true): void {
    const maxHeight = (chartHeight ?? containerRef.current?.parentElement?.clientHeight ?? 600) * 0.45;
    const height = Math.max(72, Math.min(maxHeight, requested));
    oiPaneHeightRef.current = height;
    setOiPaneHeight(height);
    if (persist) persistOiPaneHeight(height);
  }

  function persistOiPaneHeight(height: number): void {
    try {
      window.localStorage.setItem("simccxt.oi-pane-height", String(height));
    } catch {
      // Keep the resized panel for this page load.
    }
  }

  function alignPriceScales(main: IChartApi, indicators: IChartApi): void {
    const leftWidth = Math.max(
      main.priceScale("left").width(),
      indicators.priceScale("left").width(),
    );
    const rightWidth = Math.max(
      main.priceScale("right").width(),
      indicators.priceScale("right").width(),
    );
    const mainLeft = main.priceScale("left");
    const indicatorLeft = indicators.priceScale("left");
    const mainRight = main.priceScale("right");
    const indicatorRight = indicators.priceScale("right");
    if (
      !mainLeft.options().visible ||
      mainLeft.options().minimumWidth !== leftWidth ||
      mainLeft.options().ticksVisible ||
      mainLeft.options().borderVisible
    ) {
      mainLeft.applyOptions({
        visible: true,
        ticksVisible: false,
        borderVisible: false,
        minimumWidth: leftWidth,
      });
    }
    if (indicatorLeft.options().minimumWidth !== leftWidth) {
      indicatorLeft.applyOptions({ minimumWidth: leftWidth });
    }
    if (mainRight.options().minimumWidth !== rightWidth) {
      mainRight.applyOptions({ minimumWidth: rightWidth });
    }
    if (indicatorRight.options().minimumWidth !== rightWidth) {
      indicatorRight.applyOptions({ minimumWidth: rightWidth });
    }
  }

  function scheduleCvdSync(): void {
    if (scaleSyncRafRef.current) {
      window.cancelAnimationFrame(scaleSyncRafRef.current);
    }
    scaleSyncRafRef.current = window.requestAnimationFrame(() => {
      scaleSyncRafRef.current = window.requestAnimationFrame(() => {
        scaleSyncRafRef.current = 0;
        syncCvdToMain();
        setViewTick((n) => n + 1);
      });
    });
  }

  /** Keep the CVD candle index aligned with the main chart. */
  function syncCvdToMain(): void {
    const main = chartRef.current;
    const cvd = cvdChartRef.current;
    if (!main || !cvd) return;
    const r = main.timeScale().getVisibleLogicalRange();
    if (!r) return;
    syncingRef.current = true;
    try {
      alignPriceScales(main, cvd);
      const alignedRange = main.timeScale().getVisibleLogicalRange();
      if (alignedRange) cvd.timeScale().setVisibleLogicalRange(alignedRange);
    } catch {
      /* The CVD pane can briefly be empty while a snapshot is applied. */
    } finally {
      syncingRef.current = false;
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
        } else if (active === "ruler") {
          const [a, b] = pend;
          placeRuler(a, b);
        }
        pendingRef.current = [];
        previewRef.current = null;
        if (active !== "ruler") store.setTool("none");
      }
    } catch (err) {
      // eslint-disable-next-line no-console
      console.error("[draw] failed", err);
    }
  }

  // ---- ruler (transient local measurement, auto-fades) ----

  function placeRuler(a: Pt, b: Pt): void {
    window.clearTimeout(rulerTimer.current);
    setRuler({ a, b, shown: true });
    rulerTimer.current = window.setTimeout(() => {
      setRuler((r) => (r ? { ...r, shown: false } : r));
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
    sendSim({ type: "subscribe", symbol: s.symbol, timeframe: s.timeframe, exchange: s.exchange });
  }

  // ---- drag existing drawings ----

  /** Move-anchor of a drawing (time, price point that follows the cursor). */
  function anchorOf(d: Drawing): Pt {
    switch (d.kind) {
      case "ray": return { t: d.t0, p: d.price };
      case "vline": return { t: d.t, p: 0 };
      case "zone": return { t: Math.min(d.t1, d.t2), p: Math.max(d.p1, d.p2) };
      case "position": return { t: Math.min(d.t1, d.t2), p: d.entry };
    }
  }

  function onOverlayDown(e: React.PointerEvent, id: string): void {
    if (e.button === 2) return; // right button = delete (contextmenu)
    e.stopPropagation();
    const d = getState().drawings.find((x) => x.id === id);
    if (!d) return;
    store.selectDrawing(id);
    const rect = containerRef.current?.getBoundingClientRect();
    if (!rect) return;
    const x = e.clientX - rect.left;
    const y = e.clientY - rect.top;
    const mt = xToMs(x);
    const mp = yToPrice(y);
    if (mt === null || mp === null) return;
    const anchor = anchorOf(d);
    dragRef.current = { id, orig: d, offT: anchor.t - mt, offP: anchor.p - mp };
    e.currentTarget.setPointerCapture(e.pointerId);
  }

  function onOverlayMove(e: React.PointerEvent): void {
    const drag = dragRef.current;
    if (!drag) return;
    suppressClickRef.current = true; // a real drag: no placement click after it
    const rect = containerRef.current?.getBoundingClientRect();
    if (!rect) return;
    const mt = xToMs(e.clientX - rect.left);
    const mp = yToPrice(e.clientY - rect.top);
    if (mt === null || mp === null) return;
    const rows = candleTimes();
    // New anchor = cursor + grab offset, snapped to bars; shift = vs original.
    const anchor = anchorOf(drag.orig);
    const nt = snapTime(rows, mt + drag.offT);
    const np = mp + drag.offP;
    const dT = nt - anchor.t;
    const dP = np - anchor.p;
    if (dT === 0 && dP === 0) return;
    const o = drag.orig;
    if (o.kind === "ray") {
      store.updateDrawing(drag.id, (x) =>
        x.kind === "ray" ? { ...x, t0: snapTime(rows, o.t0 + dT), price: o.price + dP } : x);
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
    }
  }

  function onOverlayUp(e: React.PointerEvent): void {
    void e;
    const drag = dragRef.current;
    dragRef.current = null;
    // A moved barrier re-points the replay clock (drag end only, not per move).
    if (drag && suppressClickRef.current) {
      const d = getState().drawings.find((x) => x.id === drag.id);
      if (d && d.kind === "vline") replayFrom(d.t);
    }
  }

  // ---- HTML overlays (all drawings + ruler + ghost preview) ----

  function overlayCommon(id: string): {
    onPointerDown: (e: React.PointerEvent) => void;
    onPointerMove: (e: React.PointerEvent) => void;
    onPointerUp: (e: React.PointerEvent) => void;
    onContextMenu: (e: React.MouseEvent) => void;
  } {
    return {
      onPointerDown: (e) => onOverlayDown(e, id),
      onPointerMove: onOverlayMove,
      onPointerUp: onOverlayUp,
      onContextMenu: (e) => {
        e.preventDefault();
        e.stopPropagation();
        store.removeDrawing(id);
      },
    };
  }

  function renderOverlays(): JSX.Element {
    void viewTick; // reposition on pan/zoom/data/mousemove (bumped by subscriptions)
    const items: JSX.Element[] = [];
    for (const d of drawings) {
      const el = drawingOverlay(d, false);
      if (el) items.push(el);
    }
    const vp = profileOverlay();
    if (vp) items.push(vp);
    // Ghost preview while placing multi-click tools.
    const ghost = ghostOverlay();
    if (ghost) items.push(ghost);
    const r = rulerRef.current;
    if (r && r.shown) {
      const el = rulerOverlay(r.a, r.b, "ruler");
      if (el) items.push(el);
    }
    return <>{items}</>;
  }

  /**
   * Fixed-range volume profile of the VISIBLE bars (right side of the chart,
   * POC flagged with its price). Recomputed on every pan/zoom/data tick via
   * viewTick, so it always matches the bars on screen.
   */
  function profileOverlay(): JSX.Element | null {
    const rows = getState().candles;
    if (rows.length < 2) return null;
    const range = chartRef.current?.timeScale().getVisibleLogicalRange();
    const { rows: levels, poc } = volumeProfile(
      rows,
      range ? range.from : 0,
      range ? range.to : rows.length - 1,
      VOLUME_PROFILE_LEVELS,
    );
    if (!levels.length || poc === null) return null;
    const maxV = levels.reduce((m, r) => Math.max(m, r.volume), 0);
    if (!(maxV > 0)) return null;
    const layout = profileBarLayout(
      levels.map((level) => level.price),
      (price) => barsRef.current?.priceToCoordinate(price) ?? null,
      containerRef.current?.clientHeight,
    );
    return (
      <div
        key="vp"
        className="vp-profile"
        style={{ right: chartRef.current?.priceScale("right").width() ?? 66 }}
      >
        {levels.map((r, i) => {
          const barPosition = layout[i];
          if (!barPosition) return null;
          return (
            <div
              key={i}
              className={"vp-row" + (r.poc ? " poc" : "")}
              style={{ top: barPosition.top, height: barPosition.height }}
            >
              <div className="vp-bar" style={{ width: ((r.volume / maxV) * 100).toFixed(1) + "%" }} />
              {r.poc && (
                <div className="vp-poc">
                  {r.price.toLocaleString("en-US", { maximumFractionDigits: 1 })}
                </div>
              )}
            </div>
          );
        })}
      </div>
    );
  }

  function drawingOverlay(d: Drawing, ghost: boolean): JSX.Element | null {
    const sel = !ghost && d.id === selectedId ? " selected" : "";
    const handlers = ghost ? {} : overlayCommon(d.id);
    if (d.kind === "ray") {
      const x = xFor(d.t0);
      const y = yFor(d.price);
      const w = containerRef.current?.clientWidth ?? 0;
      if (x === null || y === null) return null;
      return (
        <div
          key={d.id + (ghost ? "-g" : "")}
          className={"ov-ray" + sel + (ghost ? " ghost" : "")}
          style={{ left: x, top: y - 2, width: Math.max(8, w - x) }}
          {...handlers}
        >
          <div className="ov-ray-dot" />
        </div>
      );
    }
    if (d.kind === "vline") {
      const x = xFor(d.t);
      if (x === null) return null;
      return (
        <div
          key={d.id + (ghost ? "-g" : "")}
          className={"ov-vline" + sel + (ghost ? " ghost" : "")}
          style={{ left: x - 2 }}
          {...handlers}
        />
      );
    }
    if (d.kind === "zone") {
      const box = cornersBox(d.t1, d.p1, d.t2, d.p2);
      if (!box) return null;
      return (
        <div
          key={d.id + (ghost ? "-g" : "")}
          className={"ov-zone" + sel + (ghost ? " ghost" : "")}
          style={{
            left: box.left, top: box.top, width: box.width, height: box.height,
            borderColor: d.color,
          }}
          {...handlers}
        />
      );
    }
    return positionOverlay(d, ghost);
  }

  function cornersBox(t1: number, p1: number, t2: number, p2: number): { left: number; top: number; width: number; height: number } | null {
    const x1 = xFor(t1);
    const x2 = xFor(t2);
    const y1 = yFor(p1);
    const y2 = yFor(p2);
    if (x1 === null || x2 === null || y1 === null || y2 === null) return null;
    const b = toBox(x1, y1, x2, y2);
    if (b.width < 2 || b.height < 2) return null;
    return b;
  }

  function positionOverlay(d: Extract<Drawing, { kind: "position" }>, ghost: boolean): JSX.Element | null {
    const top = Math.max(d.entry, d.sl, d.tp);
    const bottom = Math.min(d.entry, d.sl, d.tp);
    const box = cornersBox(d.t1, top, d.t2, bottom);
    if (!box) return null;
    const bands = positionBands(top, bottom, d.entry, d.sl, d.tp);
    const dir = positionDirection(d.entry, d.sl);
    const rr = riskReward(d.entry, d.sl, d.tp);
    const qty = positionSizeUsdt / d.entry;
    const base = symbol.split("/")[0] ?? "";
    const sel = !ghost && d.id === selectedId ? " selected" : "";
    const handlers = ghost ? {} : overlayCommon(d.id);
    return (
      <div
        key={d.id + (ghost ? "-g" : "")}
        className={"ov-pos " + dir + sel + (ghost ? " ghost" : "")}
        style={{ left: box.left, top: box.top, width: box.width, height: box.height }}
        {...handlers}
      >
        {bands.map((b, i) => (
          <div key={i} className={"ov-band " + b.cls} style={{
            top: (b.from * 100).toFixed(2) + "%",
            height: Math.max(0, (b.to - b.from) * 100).toFixed(2) + "%",
          }} />
        ))}
        <div className="ov-pos-label">
          {dir === "long" ? "LONG" : "SHORT"} {rr === null ? "—" : rr.toFixed(2) + "R"}
          {" · "}{qty.toPrecision(4)} {base} ≈ ${positionSizeUsdt}
        </div>
      </div>
    );
  }

  function rulerOverlay(a: Pt, b: Pt, key: string): JSX.Element | null {
    const box = cornersBox(a.t, a.p, b.t, b.p);
    if (!box) return null;
    const times = candleTimes();
    const stats = rulerStats(a, b, 60000);
    const bars = countBars(times, a.t, b.t);
    const text = formatRuler({ ...stats, bars });
    return (
      <div key={key}>
        <div className="ov-ruler-box" style={{
          left: box.left, top: box.top, width: box.width, height: box.height,
        }} />
        <div className="ov-ruler" style={{ left: box.left + box.width / 2, top: box.top }}>
          {text}
        </div>
      </div>
    );
  }

  /** Live ghost of the in-progress drawing + current mouse point. */
  function ghostOverlay(): JSX.Element | null {
    const active = toolRef.current;
    const pend = pendingRef.current;
    const mouse = previewRef.current;
    if (active === "none" || pend.length === 0 || !mouse) return null;
    if (active === "zone") {
      const box = cornersBox(pend[0].t, pend[0].p, mouse.t, mouse.p);
      if (!box) return null;
      return (
        <div key="ghost" className="ov-zone ghost" style={{
          left: box.left, top: box.top, width: box.width, height: box.height,
          borderColor: "#4a9ec4",
        }} />
      );
    }
    if (active === "ruler") {
      return rulerOverlay(pend[0], mouse, "ghost-ruler");
    }
    if (active === "position") {
      if (pend.length === 1) {
        // Entry set, choosing SL: ghost entry->mouse band.
        const top = Math.max(pend[0].p, mouse.p);
        const bottom = Math.min(pend[0].p, mouse.p);
        const box = cornersBox(pend[0].t, top, mouse.t, bottom);
        if (!box) return null;
        return (
          <div key="ghost" className="ov-pos ghost" style={{
            left: box.left, top: box.top, width: box.width, height: box.height,
          }}>
            <div className="ov-band sl" style={{ top: "0%", height: "100%" }} />
          </div>
        );
      }
      const [e, s] = pend;
      const dir = positionDirection(e.p, s.p);
      const ghostPos: Extract<Drawing, { kind: "position" }> = {
        id: "ghost", kind: "position",
        t1: e.t, t2: mouse.t, entry: e.p, sl: s.p, tp: mouse.p,
        color: dir === "long" ? "#3d9a50" : "#d46666", title: "",
      };
      return positionOverlay(ghostPos, true);
    }
    return null;
  }
}

function indicatorChartOptions(el: HTMLDivElement, cvd: boolean) {
  return {
    width: el.clientWidth,
    height: el.clientHeight,
    layout: {
      background: { type: ColorType.Solid, color: "#0b0e13" },
      textColor: "#7d8590",
      fontSize: 11,
    },
    grid: {
      vertLines: { color: "#141b26" },
      horzLines: { color: "#141b26" },
    },
    crosshair: { mode: CrosshairMode.Normal },
    rightPriceScale: { borderColor: "#222c3b" },
    leftPriceScale: { visible: cvd, borderColor: "#222c3b" },
    timeScale: { borderColor: "#222c3b", timeVisible: true, secondsVisible: false },
  };
}

function pushOi(line: ISeriesApi<"Line">, samples: Array<[number, number]>): void {
  line.setData(
    samples
      .filter(([time, value]) => Number.isFinite(time) && Number.isFinite(value))
      .sort((a, b) => a[0] - b[0])
      .map(([ms, value]) => ({ time: toTime(ms), value })),
  );
}

function pushCvd(line: ISeriesApi<"Line">, rows: CandleRow[]): void {
  const cvd = cumulativeDelta(rows);
  line.setData(rows.map((r, i) => ({ time: toTime(r[0]), value: cvd[i] })));
}

function pushBars(
  chart: IChartApi,
  bars: ISeriesApi<"Bar">,
  volume: ISeriesApi<"Histogram">,
  rows: CandleRow[],
  full: boolean,
): void {
  if (!rows.length) return;
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
    const last = rows[rows.length - 1];
    bars.update({ time: toTime(last[0]), open: last[1], high: last[2], low: last[3], close: last[4] });
    volume.update({
      time: toTime(last[0]),
      value: last[5],
      color: last[4] >= last[1] ? "rgba(61,154,80,0.4)" : "rgba(212,102,102,0.4)",
    });
  }
}
