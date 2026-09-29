import { useState } from "react";
import { nextRequestId, sendSim } from "../websocket/useSimSocket";
import { store, useSim } from "../state/store";

function fmtR(r: number): string {
  const v = (Math.round(r * 100) / 100).toFixed(2);
  return (r > 0 ? "+" : "") + v + "R";
}

function fmtMoney(n: number): string {
  if (!Number.isFinite(n)) return "—";
  const v = n.toLocaleString("en-US", { maximumFractionDigits: 0 });
  return (n > 0 ? "+" : "") + v;
}

/**
 * Challenge: passwordless nick + opt-in shared leaderboard.
 * The server assigns a random nick per session; typing your own overrides it.
 * Join sends the current fixed risk so the board can show R multiples.
 */
export function ChallengeCard(): JSX.Element {
  const [collapsed, setCollapsed] = useState(false);
  const nick = useSim((s) => s.nick);
  const serverNick = useSim((s) => s.serverNick);
  const inChallenge = useSim((s) => s.inChallenge);
  const board = useSim((s) => s.board);
  const fixedRiskUsdt = useSim((s) => s.fixedRiskUsdt);
  const [draft, setDraft] = useState<string | null>(null);
  const shown = draft ?? nick;

  const applyNick = (): void => {
    const name = (draft ?? nick).trim();
    store.setNick(name);
    setDraft(null);
    sendSim({ type: "login", name, request_id: nextRequestId() });
  };

  const toggleJoin = (): void => {
    const join = !inChallenge;
    store.setInChallenge(join);
    sendSim({
      type: "challenge",
      join,
      risk_usdt: join ? fixedRiskUsdt : undefined,
      request_id: nextRequestId(),
    });
  };

  return (
    <div className="card">
      <div className="card-head challenge-card-head">
        <h4>
          <button
            className="challenge-toggle"
            onClick={() => setCollapsed((value) => !value)}
            aria-expanded={!collapsed}
            aria-controls="challenge-card-content"
          >
            <span className="chev">{collapsed ? "▸" : "▾"}</span>Челлендж
          </button>
        </h4>
        {serverNick && <span className="nick-badge">{serverNick}</span>}
        {board.length > 0 && <span className="challenge-count">{board.length}</span>}
      </div>
      <div id="challenge-card-content" hidden={collapsed}>
        <div className="coin-add">
          <input
            value={shown}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") applyNick();
            }}
            onBlur={() => {
              if (draft !== null && draft !== nick) applyNick();
              else setDraft(null);
            }}
            placeholder="твой ник (пусто = рандомный)"
            aria-label="ник для челленджа"
          />
        </div>
        <label className="crit-row" style={{ marginTop: 6 }}>
          <input type="checkbox" checked={inChallenge} onChange={toggleJoin} />
          <span>участвовать (видно всем)</span>
        </label>
        {board.length === 0 && (
          <div className="lbl" style={{ marginTop: 6 }}>пока пусто — участники появятся здесь</div>
        )}
        {board.length > 0 && (
          <table className="grid-table board-table">
            <thead>
              <tr><th>Ник</th><th>Риск $</th><th>PnL $</th><th>R</th></tr>
            </thead>
            <tbody>
              {board.map((e) => (
                <tr key={e.nick} className={e.nick === serverNick ? "me" : ""}>
                  <td>{e.nick}</td>
                  <td className="num">{e.risk}</td>
                  <td className={"num" + (e.pnl > 0 ? " pos" : e.pnl < 0 ? " neg" : "")}>
                    {fmtMoney(e.pnl)}
                  </td>
                  <td className={"num" + (e.r > 0 ? " pos" : e.r < 0 ? " neg" : "")}>
                    {fmtR(e.r)}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}
