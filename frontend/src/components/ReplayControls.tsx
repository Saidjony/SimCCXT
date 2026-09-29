import { sendSim } from "../websocket/useSimSocket";
import { useSim } from "../state/store";
import { formatTs } from "../types/protocol";

/** Replay transport: Rust owns the clock, the UI only sends commands. */
export function ReplayControls(): JSX.Element {
  const replay = useSim((s) => s.replay);
  const pct = replay.total > 0 ? Math.round((replay.fed / replay.total) * 100) : 0;

  return (
    <div className="replay-compact" aria-label="Replay controls">
      <div className="replay-buttons">
        <button
          className={"btn " + (replay.paused ? "b-green" : "b-red")}
          onClick={() => sendSim({ type: "replay", action: replay.paused ? "play" : "pause" })}
          aria-label={replay.paused ? "Запустить реплей" : "Приостановить реплей"}
          title={replay.paused ? "Запустить (Space)" : "Пауза (Space)"}
        >{replay.paused ? "▶" : "⏸"}</button>
        <button
          className="btn b-acc"
          onClick={() => sendSim({ type: "replay", action: "step", steps: 1 })}
          aria-label="Следующий бар"
          title="Следующий бар (→)"
        >+1</button>
        <button
          className="btn b-dim"
          onClick={() => sendSim({ type: "replay", action: "reset" })}
          aria-label="Сбросить реплей"
          title="Сбросить реплей (Home)"
        >⏮</button>
      </div>
      <span className="replay-progress-label" title={`${replay.fed} из ${replay.total} баров`}>
        {replay.fed}/{replay.total} · {pct}%
      </span>
      <div className="progress replay-progress"><div style={{ width: pct + "%" }} /></div>
      <label className="replay-speed">
        <span className="sr-only">Скорость реплея</span>
        <select
          value={String(replay.speed)}
          onChange={(e) => sendSim({ type: "replay", action: "set_speed", speed: Number(e.target.value) })}
          aria-label="Скорость реплея"
        >
          {[0.5, 1, 2, 4, 10].map((s) => (
            <option key={s} value={s}>{s}×</option>
          ))}
        </select>
      </label>
      <span className="replay-state" title={formatTs(replay.now_ms)}>
        {replay.paused ? "Пауза" : "Идёт"}{replay.done ? " · конец" : ""}
      </span>
      <details className="shortcut-help">
        <summary aria-label="Показать горячие клавиши" title="Горячие клавиши">⌨</summary>
        <div className="shortcut-menu">
          <strong>Горячие клавиши</strong>
          <span><kbd>Space</kbd> запуск / пауза</span>
          <span><kbd>→</kbd> следующий бар</span>
          <span><kbd>Home</kbd> сброс реплея</span>
          <span><kbd>Esc</kbd> отменить рисунок</span>
          <span><kbd>Delete</kbd> удалить выбранный рисунок</span>
          <small>Работают, когда фокус не в поле ввода или кнопке.</small>
        </div>
      </details>
    </div>
  );
}
