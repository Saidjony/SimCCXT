import { useEffect, useState } from "react";
import { useSimSocket } from "../websocket/useSimSocket";
import { useSim } from "../state/store";
import { ChartWorkspace } from "../chart/ChartWorkspace";
import { ReplayControls } from "../components/ReplayControls";
import { CoinList } from "../components/CoinList";
import { ChallengeCard } from "../components/ChallengeCard";
import { OrderPanel } from "../components/OrderPanel";
import { BottomTerminal } from "../components/Panels";
import type { ExchangeId } from "../types/protocol";
import { subscribeSim } from "../websocket/connection";
import { store } from "../state/store";

/** Connection + data-age badge: "connected · data 2s ago" vs "connecting…". */
function ConnBadge(): JSX.Element {
  const status = useSim((s) => s.status);
  const lastMsgAt = useSim((s) => s.lastMsgAt);
  const reconnects = useSim((s) => s.reconnects);
  const [, tick] = useState(0);
  useEffect(() => {
    const t = window.setInterval(() => tick((n) => n + 1), 5000);
    return () => window.clearInterval(t);
  }, []);
  const age = lastMsgAt > 0 ? Math.max(0, Math.round((Date.now() - lastMsgAt) / 1000)) : -1;
  return (
    <>
      <span className={status === "open" ? "dot on" : "dot"} />
      <span className="lbl">
        {status === "open" ? "connected" : status + "…"}
        {status === "open" && age >= 0 ? " · data " + age + "s ago" : ""}
        {reconnects > 0 ? " · retries " + reconnects : ""}
      </span>
    </>
  );
}

/** Small components, one job each. No business logic: Rust decides. */
export function TradingTerminal(): JSX.Element {
  useSimSocket();
  const error = useSim((s) => s.error);
  const serverNick = useSim((s) => s.serverNick);
  const symbol = useSim((s) => s.symbol);
  const timeframe = useSim((s) => s.timeframe);
  const exchange = useSim((s) => s.exchange);

  return (
    <div className="app">
      <header className="topbar">
        <span className="logo">simccxt</span>
        <label className="exchange-picker">
          <span>Биржа</span>
          <select
            aria-label="Биржа данных"
            value={exchange}
            onChange={(event) => {
              const next = event.target.value as ExchangeId;
              store.setExchange(next);
              subscribeSim(symbol, timeframe, next);
            }}
          >
            <option value="binance">Binance</option>
            <option value="bybit">Bybit</option>
            <option value="okx">OKX</option>
          </select>
        </label>
        <ConnBadge />
        {serverNick && <span className="nick-badge">вы: {serverNick}</span>}
        <ReplayControls />
        {error && <span className="err">{error}</span>}
      </header>
      <div className="toprow">
        <main className="main">
          <ChartWorkspace />
        </main>
        <aside className="side">
          <CoinList />
          <ChallengeCard />
          <OrderPanel />
        </aside>
      </div>
      <div className="bottom">
        <BottomTerminal />
      </div>
    </div>
  );
}
