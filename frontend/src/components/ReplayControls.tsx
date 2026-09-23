import { sendSim } from "../websocket/useSimSocket";
import { useSim } from "../state/store";
import { formatTs } from "../types/protocol";

/** Replay transport: Rust owns the clock, the UI only sends commands. */
export function ReplayControls(): JSX.Element {
  const replay = useSim((s) => s.replay);
  const pct = replay.total > 0 ? Math.round((replay.fed / replay.total) * 100) : 0;

  return (
    <div className="card">
      <h4>Replay</h4>
      <div className="row">
        <button className="btn b-green" onClick={() => sendSim({ type: "replay", action: "play" })}>▶</button>
        <button className="btn b-red" onClick={() => sendSim({ type: "replay", action: "pause" })}>⏸</button>
        <button className="btn b-acc" onClick={() => sendSim({ type: "replay", action: "step", steps: 1 })}>+1</button>
        <button className="btn b-dim" onClick={() => sendSim({ type: "replay", action: "reset" })}>⏮</button>
      </div>
      <div className="row" style={{ marginTop: 8 }}>
        <span className="lbl">Speed</span>
        <select
          value={String(replay.speed)}
          onChange={(e) => sendSim({ type: "replay", action: "set_speed", speed: Number(e.target.value) })}
        >
          {[0.5, 1, 2, 4, 10].map((s) => (
            <option key={s} value={s}>{s}×</option>
          ))}
        </select>
        <span className="lbl">bar {replay.fed}/{replay.total} ({pct}%)</span>
      </div>
      <div className="status">
        {formatTs(replay.now_ms)} {replay.paused ? "⏸ paused" : "▶ playing"}
        {replay.done ? " ■ end of data" : ""}
      </div>
      <div className="progress"><div style={{ width: pct + "%" }} /></div>
    </div>
  );
}
