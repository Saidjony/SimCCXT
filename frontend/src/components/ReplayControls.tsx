import { useEffect, useState } from "react";
import { getState, store, useSim } from "../state/store";
import { sendSim, subscribeSim } from "../websocket/connection";
import type { FormEvent } from "react";
import { formatTs } from "../types/protocol";
import { Popover } from "./Popover";
import { REPLAY_SPEEDS } from "../replay/speed";

function toLocalInput(ms: number): string {
  const date = new Date(ms);
  date.setMinutes(date.getMinutes() - date.getTimezoneOffset());
  return date.toISOString().slice(0, 16);
}

/** Replay transport: Rust owns the clock, the UI only sends commands. */
export function ReplayControls(): JSX.Element {
  const replay = useSim((s) => s.replay);
  const replayWindow = useSim((s) => s.replayWindow);
  const timeframe = useSim((s) => s.timeframe);
  const [draft, setDraft] = useState({
    start: toLocalInput(replayWindow.start_ms),
    reveal: toLocalInput(replayWindow.reveal_start_ms),
    end: toLocalInput(replayWindow.end_ms),
  });
  const [rangeError, setRangeError] = useState("");
  // What the running replay actually uses: bars sit on fixed boundaries and
  // history can be short, so the reveal lands on a BAR, not on the typed
  // minute. Showing the effective value keeps the control honest.
  const replayNow = useSim((s) => s.replay.now_ms);
  const replayStep = useSim((s) => s.replay.step_ms) || 0;
  const effective = replayNow > 0
    ? " · по факту реплей с " + formatTs(replayNow + replayStep) + " UTC"
    : "";
  const pct = replay.total > 0 ? Math.round((replay.fed / replay.total) * 100) : 0;
  // A speed the ladder does not list (server default, or an older client) must
  // still be selectable, or the dropdown would render blank and hide what is
  // actually running.
  const speedOptions = REPLAY_SPEEDS.includes(replay.speed)
    ? REPLAY_SPEEDS
    : [...REPLAY_SPEEDS, replay.speed].sort((a, b) => a - b);

  useEffect(() => {
    setDraft({
      start: toLocalInput(replayWindow.start_ms),
      reveal: toLocalInput(replayWindow.reveal_start_ms),
      end: toLocalInput(replayWindow.end_ms),
    });
  }, [replayWindow.start_ms, replayWindow.reveal_start_ms, replayWindow.end_ms]);

  const applyWindow = (event: FormEvent<HTMLFormElement>): void => {
    event.preventDefault();
    const start = Date.parse(draft.start);
    const reveal = Date.parse(draft.reveal);
    const end = Date.parse(draft.end);
    if (![start, reveal, end].every(Number.isFinite) || start >= reveal || reveal > end) {
      setRangeError("Порядок дат: начало < начало реплея ≤ конец.");
      return;
    }
    if (end > Date.now()) {
      setRangeError("Конец диапазона не может быть в будущем.");
      return;
    }
    const nextRange = { start_ms: start, reveal_start_ms: reveal, end_ms: end };
    store.setReplayWindow(nextRange);
    setRangeError("");
    const current = getState();
    subscribeSim(current.symbol, current.timeframe, current.exchange);
  };

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
      <label className="replay-speed" title="Скорость реплея — также [ и ]">
        <span className="sr-only">Скорость реплея</span>
        <select
          value={String(replay.speed)}
          onChange={(e) => {
            const speed = Number(e.target.value);
            store.setReplaySpeed(speed);
            sendSim({ type: "replay", action: "set_speed", speed });
          }}
          aria-label="Скорость реплея"
        >
          {speedOptions.map((s) => (
            <option key={s} value={s}>{s}×</option>
          ))}
        </select>
      </label>
      <Popover
        label="Даты"
        ariaLabel="Настроить даты реплея"
        title="Диапазон дат для всех таймфреймов"
        panelClass="replay-window-menu"
        width={290}
      >
        <form onSubmit={applyWindow}>
          <strong>Даты истории · {timeframe}</strong>
          {effective && <small className="lbl">сейчас использует{effective}</small>}
          <label>
            Начало видимой истории
            <input
              type="datetime-local"
              value={draft.start}
              onChange={(event) => setDraft((value) => ({ ...value, start: event.target.value }))}
              required
            />
          </label>
          <label>
            Конец видимых / начало пошаговых
            <input
              type="datetime-local"
              value={draft.reveal}
              onChange={(event) => setDraft((value) => ({ ...value, reveal: event.target.value }))}
              required
            />
          </label>
          <label>
            Конец истории
            <input
              type="datetime-local"
              value={draft.end}
              onChange={(event) => setDraft((value) => ({ ...value, end: event.target.value }))}
              required
            />
          </label>
          <small className="lbl">Диапазон общий для всех графиков и загружается отдельно для их таймфреймов.</small>
          {rangeError && <span className="err">{rangeError}</span>}
          <button className="btn b-acc" type="submit">Применить ко всем графикам</button>
        </form>
      </Popover>
      <span className="replay-state" title={formatTs(replay.now_ms)}>
        {replay.paused ? "Пауза" : "Идёт"}{replay.done ? " · конец" : ""}
      </span>
    </div>
  );
}
