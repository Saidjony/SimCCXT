import { store, useSim } from "../state/store";
import { subscribeSim } from "../websocket/connection";
import { CHART_TIMEFRAMES } from "../chart/MiniChart";
import { timeframeOr } from "../market/timeframes";
import { Popover } from "../components/Popover";

/** Shortcut table shown by the ⌨ hint (kept in sync with Chart.tsx onKey). */
const HOTKEYS: Array<[string, string]> = [
  ["Space", "запуск / пауза реплея"],
  ["→", "следующий бар"],
  ["Home", "сброс реплея"],
  ["[ ]", "скорость реплея (0.5× … 10×), то же самое + и −"],
  ["Esc", "отменить рисунок"],
  ["Delete", "удалить выбранный рисунок"],
];

/**
 * Timeframe picker for the main chart. Selecting a value asks Rust to switch
 * the session's series (the server owns the series, the UI only commands it).
 */
function TimeframePicker(): JSX.Element {
  const timeframe = useSim((s) => s.timeframe);
  const symbol = useSim((s) => s.symbol);
  const exchange = useSim((s) => s.exchange);
  return (
    <label className="topbar-tf" title="Таймфрейм основного графика">
      <span className="topbar-tf-label">ТФ</span>
      <select
        className="tf-select"
        aria-label="Таймфрейм графика"
        value={timeframe}
        onChange={(event) => {
          // Record the choice (persisted) and switch this tab's local series.
          const next = timeframeOr(event.target.value, "5m");
          store.setTimeframe(next);
          subscribeSim(symbol, next, exchange);
        }}
      >
        {CHART_TIMEFRAMES.map((item) => (
          <option key={item} value={item}>{item}</option>
        ))}
      </select>
    </label>
  );
}

/** ⌨ hotkeys hint: opens the shortcut table next to the timeframe picker. */
function HotkeysHint(): JSX.Element {
  return (
    <Popover label="⌨" ariaLabel="Горячие клавиши" title="Горячие клавиши" panelClass="hotkeys-pop" width={250}>
      <strong>Горячие клавиши</strong>
      {HOTKEYS.map(([key, label]) => (
        <span key={key}><kbd>{key}</kbd>{label}</span>
      ))}
      <small>Работают, когда фокус не в поле ввода, кнопке или меню.</small>
    </Popover>
  );
}

/**
 * Background warm-up indicator: how many of the watchlist x timeframe series
 * Rust has already cached. Hidden until a warm-up is requested.
 */
function PreloadBadge(): JSX.Element | null {
  const preload = useSim((s) => s.preload);
  if (preload.total === 0) return null;
  const failed = preload.failed > 0;
  return (
    <span
      className={"preload-badge" + (preload.active ? " active" : "") + (failed ? " warn" : "")}
      title={
        preload.active
          ? `Фоновая загрузка: ${preload.done}/${preload.total}` +
            (preload.current ? ` · ${preload.current}` : "")
          : `Загружено ${preload.done}/${preload.total}` +
            (failed ? ` · ошибок: ${preload.failed}` : "")
      }
    >
      {preload.active ? "⏳" : failed ? "⚠" : "✓"} {preload.done}/{preload.total}
    </span>
  );
}

/** Topbar cluster next to the nick: timeframe picker, hotkeys, warm-up state. */
export function TopbarTools(): JSX.Element {
  return (
    <div className="topbar-tools">
      <TimeframePicker />
      <HotkeysHint />
      <PreloadBadge />
    </div>
  );
}
