import { useState } from "react";
import { nextRequestId, sendSim } from "../websocket/useSimSocket";
import { useSim } from "../state/store";
import { formatMoney } from "../types/protocol";

/** Balances + equity + PnL, all computed in Rust. */
export function Account(): JSX.Element {
  const account = useSim((s) => s.account);
  if (!account) return <div><h4>Account</h4><div className="lbl">—</div></div>;
  const rows = Object.entries(account.balances);
  return (
    <div>
      <h4>Account</h4>
      <div className="status big">Equity {formatMoney(account.equity)}</div>
      <table className="grid-table narrow-table">
        <thead><tr><th>Asset</th><th>Free</th><th>Locked</th><th>Total</th></tr></thead>
        <tbody>
          {rows.map(([asset, b]) => (
            <tr key={asset}>
              <td>{asset}</td>
              <td className="num">{formatMoney(b.free)}</td>
              <td className="num">{formatMoney(b.locked)}</td>
              <td className="num">{formatMoney(b.total)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <div className="status">
        uPnL <PnL v={account.pnl.unrealized} /> · realized {formatMoney(account.pnl.realized)}
        {" · "}fees {formatMoney(account.pnl.fees)}
      </div>
    </div>
  );
}

export function PnL({ v }: { v: number }): JSX.Element {
  const cls = v > 0 ? "pos" : v < 0 ? "neg" : "";
  return <span className={cls}>{formatMoney(v)}</span>;
}

/** Open positions with one-click close (market order in Rust). Sized in USDT. */
export function Positions(): JSX.Element {
  const positions = useSim((s) => s.account?.positions ?? []);
  return (
    <div>
      <h4>Positions</h4>
      {positions.length === 0 && <div className="lbl">flat</div>}
      {positions.length > 0 && (
        <table className="grid-table">
          <thead><tr><th>Symbol</th><th>Side</th><th>Value, USDT</th><th>Entry</th><th>Mark</th><th>uPnL</th><th></th></tr></thead>
          <tbody>
            {positions.map((p) => (
              <tr key={p.symbol + p.side}>
                <td>{p.symbol}</td>
                <td><span className={"pill " + (p.side === "long" ? "pos" : "neg")}>{p.side.toUpperCase()}</span></td>
                <td className="num">{(p.qty * p.markPrice).toLocaleString("en-US", { maximumFractionDigits: 2 })}</td>
                <td className="num">{p.entryPrice.toFixed(2)}</td>
                <td className="num">{p.markPrice.toFixed(2)}</td>
                <td className="num"><PnL v={p.unrealizedPnl} /></td>
                <td className="num">
                  <button
                    className="btn b-dim btn-sm"
                    onClick={() => sendSim({ type: "close_position", symbol: p.symbol, request_id: nextRequestId() })}
                  >
                    Close
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

/** Resting orders with cancel. */
export function Orders(): JSX.Element {
  const orders = useSim((s) => s.account?.open ?? []);
  return (
    <div>
      <h4>Orders</h4>
      {orders.length === 0 && <div className="lbl">none open</div>}
      {orders.length > 0 && (
        <table className="grid-table">
          <thead><tr><th>Side</th><th>Type</th><th>Filled/Amount</th><th>Price</th><th>Status</th><th></th></tr></thead>
          <tbody>
            {orders.map((o) => (
              <tr key={o.id}>
                <td><span className={"pill " + (o.side === "buy" ? "pos" : "neg")}>{o.side.toUpperCase()}</span></td>
                <td>{o.type}</td>
                <td className="num">{o.amount - o.filled}/{o.amount}</td>
                <td className="num">{o.price ?? "mkt"}</td>
                <td><span className="pill dim">{o.status}</span></td>
                <td className="num">
                  <button
                    className="btn b-red btn-sm"
                    onClick={() => sendSim({
                      type: "cancel_order", order_id: o.id, symbol: o.symbol, request_id: nextRequestId(),
                    })}
                  >
                    ✕
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

/** Account's own fills (newest first), emitted by Rust execution. */
export function Trades(): JSX.Element {
  const fills = useSim((s) => s.fills);
  return (
    <div>
      <h4>Trades</h4>
      {fills.length === 0 && <div className="lbl">no fills yet</div>}
      {fills.length > 0 && (
        <table className="grid-table">
          <thead><tr><th>Side</th><th>Price</th><th>Qty</th><th>Fee</th></tr></thead>
          <tbody>
            {fills.map((f) => (
              <tr key={f.id}>
                <td><span className={"pill " + (f.side === "buy" ? "pos" : "neg")}>{f.side.toUpperCase()}</span></td>
                <td className="num">{f.price.toFixed(2)}</td>
                <td className="num">{f.amount}</td>
                <td className="num">{f.fee.toFixed(4)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

type TabId = "account" | "positions" | "orders" | "trades";

const TABS: { id: TabId; label: string }[] = [
  { id: "account", label: "Account" },
  { id: "positions", label: "Positions" },
  { id: "orders", label: "Orders" },
  { id: "trades", label: "Trades" },
];

/** Full-width bottom terminal: one wide table under tabs, no inner scroll. */
export function BottomTerminal(): JSX.Element {
  const [tab, setTab] = useState<TabId>("positions");
  const openCount = useSim((s) => s.account?.open.length ?? 0);
  const posCount = useSim((s) => s.account?.positions.length ?? 0);
  const fillCount = useSim((s) => s.fills.length);

  const badge = (id: TabId): number => {
    if (id === "positions") return posCount;
    if (id === "orders") return openCount;
    if (id === "trades") return fillCount;
    return 0;
  };

  return (
    <div className="card bottom-terminal">
      <div className="tabs">
        {TABS.map((t) => (
          <button
            key={t.id}
            className={"tab" + (tab === t.id ? " active" : "")}
            onClick={() => setTab(t.id)}
          >
            {t.label}
            {badge(t.id) > 0 && <span className="tab-badge">{badge(t.id)}</span>}
          </button>
        ))}
      </div>
      <div className="tab-body">
        {tab === "account" && <Account />}
        {tab === "positions" && <Positions />}
        {tab === "orders" && <Orders />}
        {tab === "trades" && <Trades />}
      </div>
    </div>
  );
}
