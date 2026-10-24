import { useState } from "react";
import { nextRequestId, sendSim } from "../websocket/useSimSocket";
import { useSim } from "../state/store";
import { formatMoney, formatTs } from "../types/protocol";
import { criterionLabel } from "../trade/criteria";

/** Balances + equity + PnL, all computed in Rust. */
export function Account(): JSX.Element {
  const account = useSim((s) => s.account);
  if (!account) return <div><h4>Account</h4><div className="lbl">—</div></div>;
  const rows = Object.entries(account.balances);
  const symbol = useSim((s) => s.symbol);
  return (
    <div>
      <h4>Account</h4>
      <div className="account-summary">
        <span>Equity <strong>{formatMoney(account.equity)}</strong></span>
        <span>Realized <strong><PnL v={account.pnl.realized} /></strong></span>
        <span>Unrealized <strong><PnL v={account.pnl.unrealized} /></strong></span>
        <span>Fees <strong>{formatMoney(account.pnl.fees)}</strong></span>
        <span>Updated <strong>{formatTs(account.time)}</strong></span>
      </div>
      <table className="grid-table narrow-table">
        <thead><tr><th>Asset</th><th className="num">Free</th><th className="num">Locked</th><th className="num">Total</th></tr></thead>
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
      <CoinCriteriaSummary symbol={symbol} />
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
          <thead><tr><th>Symbol</th><th>Side</th><th className="num">Qty</th><th className="num">Notional</th><th className="num">Entry</th><th className="num">Mark</th><th className="num">uPnL</th><th className="num">Realized</th><th className="num">Lev.</th><th className="num">Liq. price</th><th className="num"></th></tr></thead>
          <tbody>
            {positions.map((p) => (
              <tr key={p.symbol + p.side}>
                <td>{p.symbol}</td>
                <td><span className={"pill " + (p.side === "long" ? "pos" : "neg")}>{p.side.toUpperCase()}</span></td>
                <td className="num">{p.qty.toPrecision(6)}</td>
                <td className="num">{(p.qty * p.markPrice).toLocaleString("en-US", { maximumFractionDigits: 2 })}</td>
                <td className="num">{p.entryPrice.toFixed(2)}</td>
                <td className="num">{p.markPrice.toFixed(2)}</td>
                <td className="num"><PnL v={p.unrealizedPnl} /></td>
                <td className="num"><PnL v={p.realizedPnl} /></td>
                <td className="num">{p.leverage.toFixed(2)}×</td>
                <td className="num">{p.liquidationPrice > 0 ? p.liquidationPrice.toFixed(2) : "—"}</td>
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
          <thead><tr><th>Symbol</th><th>Side</th><th>Type</th><th className="num">Filled</th><th className="num">Remaining</th><th className="num">Limit</th><th className="num">Trigger</th><th>TIF</th><th>Flags</th><th>Status</th><th>Time</th><th className="num"></th></tr></thead>
          <tbody>
            {orders.map((o) => (
              <tr key={o.id}>
                <td>{o.symbol}</td>
                <td><span className={"pill " + (o.side === "buy" ? "pos" : "neg")}>{o.side.toUpperCase()}</span></td>
                <td>{o.type}</td>
                <td className="num">{o.filled.toPrecision(5)} / {o.amount.toPrecision(5)}</td>
                <td className="num">{o.remaining.toPrecision(5)}</td>
                <td className="num">{o.price ?? "mkt"}</td>
                <td className="num">{o.triggerPrice?.toFixed(2) ?? "—"}</td>
                <td>{o.timeInForce}</td>
                <td>{o.reduceOnly ? "reduce-only " : ""}{o.postOnly ? "post-only" : ""}</td>
                <td><span className="pill dim">{o.status}</span></td>
                <td>{formatTs(o.timestamp)}</td>
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

/** Rows rendered in the bottom terminal. The store keeps 500; painting all
 *  of them on every fill made the table the slowest DOM in the app. */
const TRADES_RENDER_CAP = 150;

/** Account's own fills (newest first), emitted by Rust execution. */
export function Trades(): JSX.Element {
  const fills = useSim((s) => s.fills);
  const shown = fills.slice(0, TRADES_RENDER_CAP);
  return (
    <div>
      <h4>Trades</h4>
      <div className="lbl">Последние 500 исполнений сохраняются в этом браузере; счёт и позиции не восстанавливаются после разрыва сессии.</div>
      {fills.length === 0 && <div className="lbl">no fills yet</div>}
      {fills.length > 0 && (
        <table className="grid-table">
          <thead><tr><th>Time (UTC)</th><th>Symbol</th><th>Side</th><th>Type</th><th className="num">Price</th><th className="num">Qty</th><th className="num">Notional</th><th className="num">Fee</th><th>Liquidity</th><th>Order ID</th></tr></thead>
          <tbody>
            {shown.map((f) => (
              <tr key={f.id}>
                <td>{formatTs(f.timestamp)}</td>
                <td>{f.symbol}</td>
                <td><span className={"pill " + (f.side === "buy" ? "pos" : "neg")}>{f.side.toUpperCase()}</span></td>
                <td>{f.type}</td>
                <td className="num">{f.price.toFixed(2)}</td>
                <td className="num">{f.amount}</td>
                <td className="num">{formatMoney(f.cost)}</td>
                <td className="num">{formatMoney(f.fee)}</td>
                <td>{f.maker ? "Maker" : "Taker"}</td>
                <td>{f.orderId}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {fills.length > shown.length && (
        <div className="lbl">
          показаны последние {shown.length} из {fills.length} (полная история — в дневнике)
        </div>
      )}
    </div>
  );
}

function CoinCriteriaSummary({ symbol }: { symbol: string }): JSX.Element {
  const criteria = useSim((s) => s.chartCriteria);
  const groups = Object.entries(criteria)
    .filter(([key, ids]) => key.startsWith(`${symbol}::`) && ids.length > 0)
    .map(([key, ids]) => ({ timeframe: key.slice(symbol.length + 2), ids }));
  return (
    <section className="coin-criteria-summary">
      <h4>Критерии {symbol} · все таймфреймы</h4>
      {groups.length === 0 && <span className="lbl">Не выбраны</span>}
      {groups.map(({ timeframe, ids }) => (
        <div key={timeframe}>
          <strong>{timeframe}</strong>
          <span>{ids.map(criterionLabel).join(" · ")}</span>
        </div>
      ))}
    </section>
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
