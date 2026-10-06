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
  const board = useSim((s) => s.board);
  const [draft, setDraft] = useState<string | null>(null);
  const shown = draft ?? nick;

  const applyNick = (): void => {
    const name = (draft ?? nick).trim();
    store.setNick(name);
    setDraft(null);
    sendSim({ type: "login", name, request_id: nextRequestId() });
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
        {/* "Видно всем" is the default: every session joins the shared board
            (see useSimSocket welcome handler), so there is no opt-in toggle.
        */}
        {board.length === 0 && (
          <div className="lbl" style={{ marginTop: 6 }}>пока пусто — участники появятся здесь</div>
        )}
        {board.length > 0 && (
          <div className="board-table-scroll">
          <table className="grid-table board-table">
            <thead>
              <tr><th>Ник</th><th className="num">Риск</th><th className="num">Капитал</th><th className="num">Equity</th><th className="num">Realized</th><th className="num">Unrealized</th><th className="num">PnL</th><th className="num">R</th><th className="num">Сделки</th><th>Рынок</th></tr>
            </thead>
            <tbody>
              {board.map((e) => (
                <tr key={e.nick} className={e.nick === serverNick ? "me" : ""}>
                  <td>
                    {e.nick === serverNick ? (
                      e.nick
                    ) : (
                      // Read-only diary of someone else's ONLINE session.
                      // Opens in a new tab so this terminal keeps its socket.
                      <a
                        className="diary-link"
                        href={`/?trader=${encodeURIComponent(e.nick)}`}
                        target="_blank"
                        rel="noopener"
                        title={`Дневник ${e.nick}`}
                      >
                        {e.nick}
                      </a>
                    )}
                  </td>
                  <td className="num">{e.risk}</td>
                  <td className="num">{fmtMoney(e.capital)}</td>
                  <td className="num">{fmtMoney(e.equity)}</td>
                  <td className={"num" + (e.realized > 0 ? " pos" : e.realized < 0 ? " neg" : "")}>{fmtMoney(e.realized)}</td>
                  <td className={"num" + (e.unrealized > 0 ? " pos" : e.unrealized < 0 ? " neg" : "")}>{fmtMoney(e.unrealized)}</td>
                  <td className={"num" + (e.pnl > 0 ? " pos" : e.pnl < 0 ? " neg" : "")}>
                    {fmtMoney(e.pnl)}
                  </td>
                  <td className={"num" + (e.r > 0 ? " pos" : e.r < 0 ? " neg" : "")}>
                    {fmtR(e.r)}
                  </td>
                  <td className="num">{e.trades}</td>
                  <td>{e.symbol}</td>
                </tr>
              ))}
            </tbody>
          </table>
          </div>
        )}
      </div>
    </div>
  );
}
