import { useEffect } from "react";
import { TradingTerminal } from "./TradingTerminal";
import { subscribeSim } from "../websocket/connection";
import { getState } from "../state/store";

export function App(): JSX.Element {
  useEffect(() => {
    const ts = ["1m","5m","15m","1h","4h","1d"];
    const list = getState().watchlist;
    const ex = getState().exchange;
    list.forEach((c) => {
      ts.forEach((t) => subscribeSim(c.symbol, t, ex));
    });
  }, []);
  return <TradingTerminal />;
}
