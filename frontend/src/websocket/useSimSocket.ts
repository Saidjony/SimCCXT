import { useEffect } from "react";
import type { ServerMessage } from "../types/protocol";
import { getConnection, sendSim, subscribeSim } from "./connection";
import { getState, store } from "../state/store";
import { isLocalEngineActive, requestLocalSync } from "../replay/localEngine";

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
  // A fresh session (first visit, server restart, expired parked session) has
  // no fills of its own, while this tab's engine still owns the whole history.
  // Re-mirror it, otherwise the diary of a live trader goes blank mid-session.
  requestLocalSync(true);
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
        // Reload the series locally: the bars live in this browser (OPFS /
        // IndexedDB), and the replay runs in this tab's engine.
        //
        // The DATE WINDOW is deliberately NOT adopted from the server: it is a
        // browser preference (`simccxt.replay-window.v1`), and a session parked
        // before the user picked their dates would otherwise hand back its
        // default two-month window and move the replay. Only restore it when
        // nothing is stored locally.
        const stored = window.localStorage.getItem("simccxt.replay-window.v1");
        if (!stored && m.start_ms > 0 && m.reveal_start_ms > 0 && m.end_ms > m.start_ms) {
          store.setReplayWindow({
            start_ms: m.start_ms,
            reveal_start_ms: m.reveal_start_ms,
            end_ms: m.end_ms,
          });
        }
        subscribeSim(m.symbol, m.timeframe, m.exchange);
      } else {
        initSession();
      }
      break;
    case "loading":
      // The old server-side series loader is gone; market data is local now.
      break;
    case "snapshot":
      // The server no longer serves market data: this tab downloads and
      // aggregates its own bars (see market/provider.ts). A snapshot here
      // would only re-introduce server-held coin data, so it is ignored.
      break;
    case "series_ack":
      // The server recorded which series this tab is on (for the diary and the
      // board). No data rides along: the bars are local.
      break;
    // Market data and account state belong to this tab's local engine. The
    // server no longer streams candles, OI, ticks, replay state or order
    // updates: accepting them would overwrite local bars with whatever the
    // server happened to have (or nothing at all).
    case "candle":
    case "oi_update":
    case "market_update":
      break;
    case "account_update":
      // Account events are accepted only as a fallback when the local engine is
      // not up yet (first frames after a reconnect).
      if (!isLocalEngineActive()) store.applyAccount(m.account);
      break;
    case "order_update":
      if (!isLocalEngineActive()) store.applyOrder(m.order);
      break;
    case "trade":
      if (!isLocalEngineActive()) store.applyFill(m.fill);
      break;
    case "replay_state":
      // The clock is local: never let a server echo move it.
      break;
    case "heartbeat":
      // Liveness only (already timestamped by the connection); nothing to render.
      break;
    case "resync":
      // Nothing to catch up on over the wire: re-mirror the local account.
      requestLocalSync(true);
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
    case "market_lookup":
      // Quotes come straight from the venue now (connection.lookupMarketsSim).
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
