import { useRef, useState } from "react";
import { Chart } from "./Chart";
import { ChartCriteriaPicker } from "./ChartCriteriaPicker";
import { CHART_TIMEFRAMES, MiniChart } from "./MiniChart";
import { useSim } from "../state/store";
import { subscribeSim } from "../websocket/connection";

type PanelId = "top" | "hour" | "day";
type Timeframes = Record<Exclude<PanelId, "top">, string>;
const TIMEFRAME_KEY = "simccxt.chart-timeframes.v1";
const LAYOUT_KEY = "simccxt.chart-layout.v1";

interface Layout {
  upper: number;
  lowerLeft: number;
}

function loadLayout(): Layout {
  try {
    const raw = window.localStorage.getItem(LAYOUT_KEY);
    if (raw) {
      const value = JSON.parse(raw) as Partial<Layout>;
      return {
        upper: clamp(value.upper ?? 50, 25, 75),
        lowerLeft: clamp(value.lowerLeft ?? 50, 25, 75),
      };
    }
  } catch {
    // Use the balanced layout if preferences are unavailable.
  }
  return { upper: 50, lowerLeft: 50 };
}

function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, value));
}

function loadMiniTimeframes(): Timeframes {
  try {
    const raw = window.localStorage.getItem(TIMEFRAME_KEY);
    if (raw) {
      const value: unknown = JSON.parse(raw);
      if (value && typeof value === "object") {
        const saved = value as Record<string, unknown>;
        return {
          hour: CHART_TIMEFRAMES.includes(String(saved.hour)) ? String(saved.hour) : "1h",
          day: CHART_TIMEFRAMES.includes(String(saved.day)) ? String(saved.day) : "1d",
        };
      }
    }
  } catch {
    // Use the requested defaults when local storage is unavailable.
  }
  return { hour: "1h", day: "1d" };
}

export function ChartWorkspace(): JSX.Element {
  const workspaceRef = useRef<HTMLDivElement | null>(null);
  const symbol = useSim((s) => s.symbol);
  const exchange = useSim((s) => s.exchange);
  const timeframe = useSim((s) => s.timeframe);
  const [miniTimeframes, setMiniTimeframes] = useState(loadMiniTimeframes);
  const [layout, setLayout] = useState(loadLayout);
  const layoutRef = useRef(layout);
  const [maximized, setMaximized] = useState<PanelId | null>(null);

  const setMainTimeframe = (next: string): void => {
    subscribeSim(symbol, next, exchange);
  };
  const setMiniTimeframe = (panel: Exclude<PanelId, "top">, next: string): void => {
    const value = { ...miniTimeframes, [panel]: next };
    setMiniTimeframes(value);
    try {
      window.localStorage.setItem(TIMEFRAME_KEY, JSON.stringify(value));
    } catch {
      // Keep the selected timeframe for this page load.
    }
  };
  const toggleMaximize = (panel: PanelId): void => {
    setMaximized((current) => current === panel ? null : panel);
  };
  const visible = (panel: PanelId): boolean => maximized === null || maximized === panel;
  const saveLayout = (value: Layout): void => {
    try {
      window.localStorage.setItem(LAYOUT_KEY, JSON.stringify(value));
    } catch {
      // Keep the adjusted layout for this page load.
    }
  };
  const setSplit = (axis: keyof Layout, value: number, persist = true): void => {
    const next = { ...layout, [axis]: clamp(value, 25, 75) };
    layoutRef.current = next;
    setLayout(next);
    if (persist) saveLayout(next);
  };
  const startResize = (axis: keyof Layout, event: React.PointerEvent<HTMLDivElement>): void => {
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    const bounds = workspaceRef.current?.getBoundingClientRect();
    if (!bounds) return;
    const move = (pointer: PointerEvent): void => {
      const value = axis === "upper"
        ? ((pointer.clientY - bounds.top) / bounds.height) * 100
        : ((pointer.clientX - bounds.left) / bounds.width) * 100;
      setSplit(axis, value, false);
    };
    const stop = (): void => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      saveLayout(layoutRef.current);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop, { once: true });
  };

  return (
    <div
      ref={workspaceRef}
      className={"chart-workspace" + (maximized ? " chart-workspace-maximized" : "")}
      style={maximized ? undefined : {
        gridTemplateRows: `minmax(0, ${layout.upper}fr) 8px minmax(0, ${100 - layout.upper}fr)`,
      }}
    >
      {visible("top") && (
        <section className="chart-pane chart-pane-top">
          <header className="chart-pane-head">
            <strong>Основной график</strong>
            <span className="chart-pane-symbol">{symbol}</span>
            <select
              className="tf-select"
              aria-label="Таймфрейм верхнего графика"
              value={timeframe}
              onChange={(event) => setMainTimeframe(event.target.value)}
            >
              {CHART_TIMEFRAMES.map((item) => <option key={item} value={item}>{item}</option>)}
            </select>
            <ChartCriteriaPicker timeframe={timeframe} />
            <button
              className="btn b-dim chart-maximize"
              onClick={() => toggleMaximize("top")}
              aria-label={maximized === "top" ? "Вернуть сетку графиков" : "Развернуть график"}
              title={maximized === "top" ? "Вернуть сетку графиков" : "На весь экран"}
            >
              {maximized === "top" ? "↙" : "⛶"}
            </button>
          </header>
          <div className="chart-pane-body chart-pane-main">
            <Chart />
          </div>
        </section>
      )}
      {maximized === null && (
        <div
          className="chart-workspace-divider horizontal"
          role="separator"
          aria-label="Изменить высоту графиков"
          aria-orientation="horizontal"
          aria-valuemin={25}
          aria-valuemax={75}
          aria-valuenow={layout.upper}
          onPointerDown={(event) => startResize("upper", event)}
          onKeyDown={(event) => {
            if (event.key === "ArrowUp") setSplit("upper", layout.upper - 2);
            if (event.key === "ArrowDown") setSplit("upper", layout.upper + 2);
          }}
          tabIndex={0}
        />
      )}
      {maximized === null && (
        <div
          className="chart-workspace-bottom"
          style={{ gridTemplateColumns: `minmax(0, ${layout.lowerLeft}fr) 8px minmax(0, ${100 - layout.lowerLeft}fr)` }}
        >
          <MiniChart
            panel="hour"
            label="1 час"
            timeframe={miniTimeframes.hour}
            mainTimeframe={timeframe}
            onTimeframeChange={(next) => setMiniTimeframe("hour", next)}
            onToggleMaximize={() => toggleMaximize("hour")}
            maximized={false}
          />
          <div
            className="chart-workspace-divider vertical"
            role="separator"
            aria-label="Изменить ширину нижних графиков"
            aria-orientation="vertical"
            aria-valuemin={25}
            aria-valuemax={75}
            aria-valuenow={layout.lowerLeft}
            onPointerDown={(event) => startResize("lowerLeft", event)}
            onKeyDown={(event) => {
              if (event.key === "ArrowLeft") setSplit("lowerLeft", layout.lowerLeft - 2);
              if (event.key === "ArrowRight") setSplit("lowerLeft", layout.lowerLeft + 2);
            }}
            tabIndex={0}
          />
          <MiniChart
            panel="day"
            label="День"
            timeframe={miniTimeframes.day}
            mainTimeframe={timeframe}
            onTimeframeChange={(next) => setMiniTimeframe("day", next)}
            onToggleMaximize={() => toggleMaximize("day")}
            maximized={false}
          />
        </div>
      )}
      {maximized !== null && maximized !== "top" && visible(maximized) && (
        <MiniChart
          panel={maximized}
          label={maximized === "hour" ? "1 час" : "День"}
          timeframe={miniTimeframes[maximized]}
          mainTimeframe={timeframe}
          onTimeframeChange={(next) => setMiniTimeframe(maximized, next)}
          onToggleMaximize={() => toggleMaximize(maximized)}
          maximized
        />
      )}
    </div>
  );
}
