import { useEffect } from "react";
import type { ServerMessage } from "../types/protocol";
import { getConnection, sendSim, subscribeSim } from "./connection";
import { getState, store } from "../state/store";

export { nextRequestId, sendSim } from "./connection";

/** Fresh session: identity, challenge entry, series. Called after
 *  `session_state` says there is nothing to restore. */
function initSession(): void {
  // Restore identity: login (stored nick or empty = server random nick),
  // then rejoin the challenge with the stored risk/capital.
  sendSim({ type: "login", name: getState().nick });
  sendSim({
    type: "challenge",
    join: true,
    risk_usdt: getState().fixedRiskUsdt,
    capital_usdt: getState().initialCapitalUsdt,
  });
  subscribeSim(getState().symbol, getState().timeframe, getState().exchange);
}

function route(m: ServerMessage): void {
  const me = getState().clientId;
  const to = (m as { to?: number }).to;
  if (typeof to === "number" && me !== null && to !== me) return; // other client
  switch (m.type) {
    case "welcome":
      // Just the socket id; the session is bound by `attach` (sent on open)
      // and answered with `session_state`.
      store.setClientId(m.client_id);
      break;
    case "session_state":
      // The server kept this tab's session (reload/reconnect): adopt its
      // risk/capital/nick and let the snapshot that follows restore the
      // series, clock and account. Otherwise start a fresh session.
      // On a fresh session the server has not joined yet — keep the local
      // opt-in and let the `challenge` below establish it. A restored session
      // carries the authoritative flag.
      store.restoreChallenge(
        m.risk_usdt,
        m.capital_usdt,
        m.restored ? m.in_challenge : getState().inChallenge,
      );
      if (m.nick) store.applyLogin(m.nick);
      if (m.restored && m.has_series) {
        // Adopt the running session's window (the snapshot that follows
        // carries its candles and clock). Keep the local one when the server
        // has no resolved range yet.
        if (m.start_ms > 0 && m.reveal_start_ms > 0 && m.end_ms > m.start_ms) {
          store.setReplayWindow({
            start_ms: m.start_ms,
            reveal_start_ms: m.reveal_start_ms,
            end_ms: m.end_ms,
          });
        }
      } else {
        initSession();
      }
      break;
    case "loading":
      store.setSeriesLoading(`${m.symbol} ${m.timeframe}: загрузка истории…`);
      break;
    case "snapshot":
      store.setSeriesLoading(null);
      store.applySnapshot(m.symbol, m.exchange, m.timeframe, m.candles, m.account, m.replay, m.oi, m.oi_period, m.fills);
      break;
    case "candle":
      if (m.symbol === getState().symbol && (!m.exchange || m.exchange === getState().exchange)) {
        store.applyCandle(m.candle);
      }
      break;
    case "oi_update":
      store.applyOi(m.samples);
      break;
    case "market_update":
      store.applyMarket(m.last);
      break;
    case "account_update":
      store.applyAccount(m.account);
      break;
    case "order_update":
      store.applyOrder(m.order);
      break;
    case "trade":
      store.applyFill(m.fill);
      break;
    case "replay_state":
      store.applyReplay(m);
      break;
    case "heartbeat":
      // Liveness only (already timestamped by the connection); nothing to render.
      break;
    case "resync":
      // We lagged and missed candles: full snapshot again, then deltas resume.
      subscribeSim(getState().symbol, getState().timeframe, getState().exchange);
      break;
    case "login_ok":
      store.applyLogin(m.nick);
      break;
    case "challenge_ok":
      // Authoritative join state (server may reject a bad risk).
      if (!m.joined && getState().inChallenge) store.setInChallenge(false);
      break;
    case "challenge_board":
      // No `to`: shared by design, the route() filter above lets it through.
      store.applyBoard(m.board);
      break;
    case "preload_progress":
      store.advancePreload(
        m.done,
        m.total,
        m.failed,
        m.symbol ? `${m.symbol} · ${m.timeframe}` : "",
      );
      break;
    case "preload_done":
      store.finishPreload(m.done, m.total, m.failed);
      break;
    case "error":
      store.setSeriesLoading(null);
      store.setError(m.message);
      break;
    case "pong":
      break;
  }
}

/** Own the page-level WebSocket: connect, route messages, resubscribe. */
export function useSimSocket(): void {
  useEffect(() => {
    const c = getConnection();
    c.attach(route);
    return () => c.detach();
  }, []);
}
