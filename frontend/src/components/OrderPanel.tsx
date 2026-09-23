import { useState } from "react";
import { nextRequestId, sendSim } from "../websocket/useSimSocket";
import { store, useSim } from "../state/store";
import type { ClientMessage } from "../types/protocol";
import { calcQty } from "../chart/overlays";

function num(v: string): number | null {
  const n = Number(v);
  return v.trim() !== "" && Number.isFinite(n) && n > 0 ? n : null;
}

type EntryKind = "market" | "limit" | "stop";

const KIND_LABEL: Record<EntryKind, string> = {
  market: "Рынок",
  limit: "Лимит",
  stop: "Стоп",
};

/**
 * Deal ticket: capital + fixed risk, one entry form with a type dropdown.
 * Sizing is risk-based: qty = fixedRiskUsdt / |entry - stop|.
 * Without a stop price entry is blocked (no defined risk).
 */
export function OrderPanel(): JSX.Element {
  const symbol = useSim((s) => s.symbol);
  const lastPrice = useSim((s) => s.lastPrice);
  const initialCapitalUsdt = useSim((s) => s.initialCapitalUsdt);
  const fixedRiskUsdt = useSim((s) => s.fixedRiskUsdt);
  const inChallenge = useSim((s) => s.inChallenge);
  const base = symbol.split("/")[0] ?? "";

  const [kind, setKind] = useState<EntryKind>("market");
  const [price, setPrice] = useState("");
  const [stop, setStop] = useState("");
  const [tif, setTif] = useState("GTC");
  const [postOnly, setPostOnly] = useState(false);
  const error = useSim((s) => s.error);

  const send = (side: string): void => {
    const sl = num(stop);
    if (sl === null) return;
    let orderType = kind;
    let entry: number | null = null;
    const msg: ClientMessage = {
      type: "place_order", symbol, side, order_type: orderType,
      quantity: 0, time_in_force: tif, post_only: postOnly,
      request_id: nextRequestId(),
    };
    if (kind === "market") {
      entry = lastPrice > 0 ? lastPrice : null;
    } else if (kind === "limit") {
      const p = num(price);
      if (p === null) return;
      entry = p;
      msg.price = p;
    } else {
      const t = num(price);
      if (t === null) return;
      entry = t;
      orderType = "stop";
      msg.order_type = "stop";
      msg.stop_price = t;
    }
    if (entry === null) return;
    const qty = calcQty(fixedRiskUsdt, entry, sl);
    if (qty === null) return;
    msg.quantity = qty;
    sendSim(msg);
    // While in the challenge the board shows our risk: keep it in sync.
    if (inChallenge) {
      sendSim({ type: "challenge", join: true, risk_usdt: fixedRiskUsdt });
    }
  };

  const entryPreview = (): number | null => {
    if (kind === "market") return lastPrice > 0 ? lastPrice : null;
    return num(price);
  };
  const entry = entryPreview();
  const sl = num(stop);
  const qty = entry !== null && sl !== null ? calcQty(fixedRiskUsdt, entry, sl) : null;
  const ready = qty !== null;
  const preview =
    qty !== null
      ? `≈ ${qty.toPrecision(4)} ${base} · риск $${fixedRiskUsdt}`
      : "укажи стоп — без него вход запрещён";

  return (
    <div className="card">
      <h4>Сделка</h4>
      <div className="row">
        <span className="lbl">Капитал</span>
        <input
          value={String(initialCapitalUsdt)}
          onChange={(e) => {
            const v = Number(e.target.value);
            if (Number.isFinite(v) && v > 0) store.setInitialCapitalUsdt(v);
          }}
          style={{ width: 90 }}
          aria-label="капитал, USDT"
        />
        <span className="lbl">Риск</span>
        <input
          value={String(fixedRiskUsdt)}
          onChange={(e) => {
            const v = Number(e.target.value);
            if (Number.isFinite(v) && v > 0) store.setFixedRiskUsdt(v);
          }}
          style={{ width: 70 }}
          aria-label="риск, USDT"
        />
      </div>
      <div className="row" style={{ marginTop: 6 }}>
        <select
          value={kind}
          onChange={(e) => setKind(e.target.value as EntryKind)}
          style={{ width: 100 }}
          aria-label="тип входа"
        >
          {(Object.keys(KIND_LABEL) as EntryKind[]).map((k) => (
            <option key={k} value={k}>{KIND_LABEL[k]}</option>
          ))}
        </select>
        <select
          value={tif}
          onChange={(e) => setTif(e.target.value)}
          style={{ width: 76 }}
          aria-label="time in force"
        >
          {["GTC", "IOC", "FOK"].map((t) => (
            <option key={t} value={t}>{t}</option>
          ))}
        </select>
        <label>
          <input type="checkbox" checked={postOnly} onChange={(e) => setPostOnly(e.target.checked)} />
          {" post-only"}
        </label>
      </div>
      <div className="row" style={{ marginTop: 6 }}>
        {kind !== "market" && (
          <>
            <span className="lbl">{kind === "limit" ? "Цена" : "Триггер"}</span>
            <input
              value={price}
              onChange={(e) => setPrice(e.target.value)}
              placeholder={kind === "limit" ? "цена" : "триггер → рынок"}
              style={{ width: 100 }}
            />
          </>
        )}
        <span className="lbl">Стоп</span>
        <input
          value={stop}
          onChange={(e) => setStop(e.target.value)}
          placeholder="цена стопа"
          style={{ width: 100 }}
        />
      </div>
      <div className="row" style={{ marginTop: 6 }}>
        <button className="btn b-green" disabled={!ready} onClick={() => send("buy")}>BUY</button>
        <button className="btn b-red" disabled={!ready} onClick={() => send("sell")}>SELL</button>
      </div>
      <div className="status">{preview}</div>
      {error && <div className="err">{error}</div>}
    </div>
  );
}
