import { useState } from "react";
import { nextRequestId, sendSim } from "../websocket/useSimSocket";
import { store, useSim } from "../state/store";
import type { ClientMessage } from "../types/protocol";
import { calcQty } from "../chart/overlays";

function num(v: string): number | null {
  const n = Number(v);
  return v.trim() !== "" && Number.isFinite(n) && n > 0 ? n : null;
}

/**
 * Manual trading, everything in USDT. Every button is one command to Rust;
 * execution, PnL and the clock stay in the engine.
 *
 * Sizing is risk-based: qty = fixedRiskUsdt / |entry - stop|.
 * Without a stop price entry is blocked (no defined risk).
 */
export function OrderPanel(): JSX.Element {
  const symbol = useSim((s) => s.symbol);
  const lastPrice = useSim((s) => s.lastPrice);
  const initialCapitalUsdt = useSim((s) => s.initialCapitalUsdt);
  const fixedRiskUsdt = useSim((s) => s.fixedRiskUsdt);
  const base = symbol.split("/")[0] ?? "";

  const [marketSl, setMarketSl] = useState("");
  const [limitPrice, setLimitPrice] = useState("");
  const [limitSl, setLimitSl] = useState("");
  const [trigPrice, setTrigPrice] = useState("");
  const [trigSl, setTrigSl] = useState("");
  const [tif, setTif] = useState("GTC");
  const [postOnly, setPostOnly] = useState(false);
  const lastOrder = useSim((s) => s.lastOrder);
  const error = useSim((s) => s.error);

  const send = (
    side: string,
    orderType: string,
    entry: number | null,
    sl: number | null,
    extra?: { price?: number; stopPrice?: number },
  ): void => {
    if (entry === null || sl === null) return;
    const qty = calcQty(fixedRiskUsdt, entry, sl);
    if (qty === null) return;
    // stop_price goes only to stop orders: a market order with stopPrice
    // would turn into a stop in the engine.
    const msg: ClientMessage = {
      type: "place_order", symbol, side, order_type: orderType,
      quantity: qty, time_in_force: tif, post_only: postOnly,
      request_id: nextRequestId(),
    };
    if (orderType === "limit" && extra?.price != null) msg.price = extra.price;
    if (orderType === "stop" && extra?.stopPrice != null) msg.stop_price = extra.stopPrice;
    sendSim(msg);
  };

  const preview = (entry: number | null, sl: number | null): string => {
    if (entry === null || sl === null) return "укажи стоп — без него вход запрещён";
    const qty = calcQty(fixedRiskUsdt, entry, sl);
    if (qty === null) return "стоп равен входу — объём бесконечный";
    return `≈ ${qty.toPrecision(4)} ${base} · риск $${fixedRiskUsdt}`;
  };

  const mSl = num(marketSl);
  const lPrice = num(limitPrice);
  const lSl = num(limitSl);
  const tPrice = num(trigPrice);
  const tSl = num(trigSl);
  const marketEntry = lastPrice > 0 ? lastPrice : null;

  const blocked = (v: number | null): boolean => v === null;

  return (
    <div className="card">
      <h4>Риск</h4>
      <div className="row">
        <span className="lbl">Капитал, USDT</span>
        <input
          value={String(initialCapitalUsdt)}
          onChange={(e) => {
            const v = Number(e.target.value);
            if (Number.isFinite(v) && v > 0) store.setInitialCapitalUsdt(v);
          }}
          style={{ width: 100 }}
        />
        <span className="lbl">Риск, USDT</span>
        <input
          value={String(fixedRiskUsdt)}
          onChange={(e) => {
            const v = Number(e.target.value);
            if (Number.isFinite(v) && v > 0) store.setFixedRiskUsdt(v);
          }}
          style={{ width: 80 }}
        />
      </div>
      <div className="lbl" style={{ marginTop: 6 }}>
        объём = ${fixedRiskUsdt} риск / |вход − стоп| · без стопа вход запрещён
      </div>

      <h4 style={{ marginTop: 12 }}>Вход по рынку</h4>
      <div className="row">
        <span className="lbl">Стоп</span>
        <input value={marketSl} onChange={(e) => setMarketSl(e.target.value)}
          placeholder="цена стопа" style={{ width: 100 }} />
        <button className="btn b-green" disabled={blocked(mSl)} onClick={() => send("buy", "market", marketEntry, mSl)}>BUY</button>
        <button className="btn b-red" disabled={blocked(mSl)} onClick={() => send("sell", "market", marketEntry, mSl)}>SELL</button>
      </div>
      <div className="status">{preview(marketEntry, mSl)}</div>

      <h4 style={{ marginTop: 12 }}>Вход лимиткой</h4>
      <div className="row">
        <span className="lbl">Цена</span>
        <input value={limitPrice} onChange={(e) => setLimitPrice(e.target.value)}
          placeholder="цена" style={{ width: 90 }} />
        <span className="lbl">Стоп</span>
        <input value={limitSl} onChange={(e) => setLimitSl(e.target.value)}
          placeholder="цена стопа" style={{ width: 90 }} />
      </div>
      <div className="row" style={{ marginTop: 6 }}>
        <button className="btn b-acc" disabled={blocked(lPrice) || blocked(lSl)}
          onClick={() => send("buy", "limit", lPrice, lSl, { price: lPrice ?? undefined })}>Buy</button>
        <button className="btn b-acc" disabled={blocked(lPrice) || blocked(lSl)}
          onClick={() => send("sell", "limit", lPrice, lSl, { price: lPrice ?? undefined })}>Sell</button>
        <select value={tif} onChange={(e) => setTif(e.target.value)} style={{ width: 76 }}>
          {["GTC", "IOC", "FOK"].map((t) => (
            <option key={t} value={t}>{t}</option>
          ))}
        </select>
        <label>
          <input type="checkbox" checked={postOnly} onChange={(e) => setPostOnly(e.target.checked)} />
          {" post-only"}
        </label>
      </div>
      <div className="status">{preview(lPrice, lSl)}</div>

      <h4 style={{ marginTop: 12 }}>Отложенный вход</h4>
      <div className="row">
        <span className="lbl">Триггер</span>
        <input value={trigPrice} onChange={(e) => setTrigPrice(e.target.value)}
          placeholder="триггер → рынок" style={{ width: 90 }} />
        <span className="lbl">Стоп</span>
        <input value={trigSl} onChange={(e) => setTrigSl(e.target.value)}
          placeholder="цена стопа" style={{ width: 90 }} />
      </div>
      <div className="row" style={{ marginTop: 6 }}>
        <button className="btn b-dim" disabled={blocked(tPrice) || blocked(tSl)}
          onClick={() => send("buy", "stop", tPrice, tSl, { stopPrice: tPrice ?? undefined })}>Buy</button>
        <button className="btn b-dim" disabled={blocked(tPrice) || blocked(tSl)}
          onClick={() => send("sell", "stop", tPrice, tSl, { stopPrice: tPrice ?? undefined })}>Sell</button>
      </div>
      <div className="status">{preview(tPrice, tSl)}</div>

      {lastOrder && (
        <div className="status">
          last: {lastOrder.side.toUpperCase()} {lastOrder.type} {lastOrder.status}
          {lastOrder.average ? " @ " + lastOrder.average.toFixed(2) : ""}
        </div>
      )}
      {error && <div className="err">{error}</div>}
    </div>
  );
}
