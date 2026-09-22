import { sendSim } from "../websocket/useSimSocket";
import { useSim } from "../state/store";
import { formatTs } from "../types/protocol";

const SYMBOLS = ["BTC/USDT", "ETH/USDT", "SOL/USDT", "XRP/USDT"];
const TIMEFRAMES = ["1m", "5m", "15m", "1h"];

/** Market & playback: symbol/timeframe picker (asks Rust for a new series). */
export function Screener(): JSX.Element {
  const symbol = useSim((s) => s.symbol);
  const timeframe = useSim((s) => s.timeframe);
  const status = useSim((s) => s.status);
  const lastPrice = useSim((s) => s.lastPrice);

  const load = (sym: string, tf: string): void => {
    sendSim({ type: "subscribe", symbol: sym, timeframe: tf });
  };

  return (
    <div className="card">
      <h4>Market</h4>
      <div className="row">
        <span className="lbl">Symbol</span>
        <select value={symbol} onChange={(e) => load(e.target.value, timeframe)}>
          {SYMBOLS.map((s) => (
            <option key={s} value={s}>{s}</option>
          ))}
        </select>
        <span className="lbl">TF</span>
        <select value={timeframe} onChange={(e) => load(symbol, e.target.value)}>
          {TIMEFRAMES.map((t) => (
            <option key={t} value={t}>{t}</option>
          ))}
        </select>
      </div>
      <div className="status">
        last {Number.isFinite(lastPrice) ? lastPrice.toLocaleString("en-US") : "—"}
        {" · "}
        {status === "open" ? "● live" : status}
      </div>
    </div>
  );
}

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
