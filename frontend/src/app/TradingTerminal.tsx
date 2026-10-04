import { useEffect, useRef, useState } from "react";
import { useSimSocket } from "../websocket/useSimSocket";
import { useSim } from "../state/store";
import { ChartWorkspace } from "../chart/ChartWorkspace";
import { ReplayControls } from "../components/ReplayControls";
import { TopbarTools } from "./TopbarTools";
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

  // Resizable side rail width (px), persisted across reloads.
  const [sideWidth, setSideWidth] = useState<number>(() => {
    try {
      const saved = Number(localStorage.getItem("simccxt.side-width"));
      if (Number.isFinite(saved) && saved >= 280 && saved <= 720) return saved;
    } catch {
      /* storage unavailable */
    }
    return 336;
  });
  const sideRef = useRef<HTMLElement | null>(null);
  const startSideResize = (e: React.MouseEvent): void => {
    e.preventDefault();
    const startX = e.clientX;
    const startW = sideRef.current?.getBoundingClientRect().width ?? sideWidth;
    const onMove = (ev: MouseEvent): void => {
      const next = Math.min(Math.max(startW + (startX - ev.clientX), 280), 720);
      setSideWidth(Math.round(next));
    };
    const onUp = (): void => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      setSideWidth((w) => {
        try {
          localStorage.setItem("simccxt.side-width", String(w));
        } catch {
          /* ignore */
        }
        return w;
      });
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  };

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
        <TopbarTools />
        <ReplayControls />
        {error && <span className="err">{error}</span>}
      </header>
      <div className="toprow">
        <main className="main">
          <ChartWorkspace />
        </main>
        {/* Draggable width handle between the chart workspace and the side rail */}
        <div className="side-resizer" onMouseDown={startSideResize} />
        <aside
          className="side"
          ref={sideRef as React.MutableRefObject<HTMLElement | null>}
          style={{ width: sideWidth, flex: `0 0 ${sideWidth}px` }}
        >
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
