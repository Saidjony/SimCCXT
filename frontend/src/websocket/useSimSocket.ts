import { useEffect } from "react";
import type { ServerMessage } from "../types/protocol";
import { getConnection, sendSim } from "./connection";
import { getState, store } from "../state/store";

export { nextRequestId, sendSim } from "./connection";

function route(m: ServerMessage): void {
  const me = getState().clientId;
  const to = (m as { to?: number }).to;
  if (typeof to === "number" && me !== null && to !== me) return; // other client
  switch (m.type) {
    case "welcome":
      store.setClientId(m.client_id);
      sendSim({ type: "subscribe", symbol: getState().symbol, timeframe: getState().timeframe });
      break;
    case "snapshot":
      store.applySnapshot(m.symbol, m.timeframe, m.candles, m.account, m.replay);
      break;
    case "candle":
      if (m.symbol === getState().symbol) store.applyCandle(m.candle);
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
      sendSim({ type: "subscribe", symbol: getState().symbol, timeframe: getState().timeframe });
      break;
    case "error":
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
