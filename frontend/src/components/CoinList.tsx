import { useState } from "react";
import { sendSim } from "../websocket/useSimSocket";
import { getState, normalizeSymbol, store, useSim } from "../state/store";
import type { CoinEntry } from "../state/store";

const TIMEFRAMES = ["1m", "5m", "15m", "1h"];

/** Outline/filled flag: the "pinned" marker for a watchlist coin. */
function FlagIcon({ filled }: { filled: boolean }): JSX.Element {
  return (
    <svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true">
      <path d="M3.5 1.5v13" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" fill="none" />
      <path
        d="M3.5 2.5h9.5l-2.2 2.8 2.2 2.8H3.5z"
        fill={filled ? "currentColor" : "none"}
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinejoin="round"
      />
    </svg>
  );
}

function CoinRow(props: {
  coin: CoinEntry;
  active: boolean;
  lastPrice: number;
  onSwitch: (sym: string) => void;
  onFlag: (sym: string) => void;
  onRemove: (sym: string) => void;
}): JSX.Element {
  const { coin, active, lastPrice } = props;
  return (
    <div
      className={
        "coin-row" + (active ? " active" : "") + (coin.flagged ? " flagged" : "")
      }
      onClick={() => props.onSwitch(coin.symbol)}
      title={"показать " + coin.symbol}
    >
      <span
        className={"coin-flag" + (coin.flagged ? " on" : "")}
        role="button"
        title={coin.flagged ? "снять флаг" : "поставить флаг"}
        onClick={(e) => {
          e.stopPropagation();
          props.onFlag(coin.symbol);
        }}
      >
        <FlagIcon filled={coin.flagged} />
      </span>
      <span className="coin-name">{coin.symbol}</span>
      {active && (
        <span className="coin-px">
          {lastPrice > 0
            ? lastPrice.toLocaleString("en-US", { maximumFractionDigits: 2 })
            : "—"}
        </span>
      )}
      <span
        className="coin-x"
        role="button"
        title="убрать из списка"
        onClick={(e) => {
          e.stopPropagation();
          props.onRemove(coin.symbol);
        }}
      >
        ×
      </span>
    </div>
  );
}

/**
 * Market card: the right-panel coin list (add / remove / flag) plus the
 * timeframe picker. Clicking a row switches the session's series in Rust.
 */
export function CoinList(): JSX.Element {
  const watchlist = useSim((s) => s.watchlist);
  const symbol = useSim((s) => s.symbol);
  const timeframe = useSim((s) => s.timeframe);
  const lastPrice = useSim((s) => s.lastPrice);
  const status = useSim((s) => s.status);
  const collapsed = useSim((s) => s.collapsedMarket);
  const [input, setInput] = useState("");
  const [formErr, setFormErr] = useState<string | null>(null);

  const switchTo = (sym: string): void => {
    if (sym === symbol) return;
    sendSim({ type: "subscribe", symbol: sym, timeframe });
  };

  const remove = (sym: string): void => {
    const wasActive = sym === symbol;
    store.removeCoin(sym);
    // Removing the coin on screen: jump to the first remaining one.
    if (wasActive) {
      const next = getState().watchlist[0];
      if (next) {
        sendSim({ type: "subscribe", symbol: next.symbol, timeframe: getState().timeframe });
      }
    }
  };

  const add = (): void => {
    const norm = normalizeSymbol(input);
    if (!norm) {
      setFormErr("формат: BASE/QUOTE, напр. DOGE/USDT");
      return;
    }
    if (store.addCoin(norm) === null) {
      setFormErr("такая монета уже в списке");
      return;
    }
    setFormErr(null);
    setInput("");
  };

  const flagged = watchlist.filter((c) => c.flagged);
  const rest = watchlist.filter((c) => !c.flagged);
  const row = (c: CoinEntry): JSX.Element => (
    <CoinRow
      key={c.symbol}
      coin={c}
      active={c.symbol === symbol}
      lastPrice={lastPrice}
      onSwitch={switchTo}
      onFlag={store.toggleFlag}
      onRemove={remove}
    />
  );

  return (
    <div className="card">
      <div
        className="card-head collapsible"
        onClick={() => store.setCollapsedMarket(!collapsed)}
        title={collapsed ? "развернуть" : "свернуть"}
      >
        <h4><span className="chev">{collapsed ? "▸" : "▾"}</span> Рынок</h4>
        <select
          className="tf-select"
          value={timeframe}
          onClick={(e) => e.stopPropagation()}
          onChange={(e) => sendSim({ type: "subscribe", symbol, timeframe: e.target.value })}
        >
          {TIMEFRAMES.map((t) => (
            <option key={t} value={t}>{t}</option>
          ))}
        </select>
      </div>
      {!collapsed && (
      <>
      <div className="coin-list">
        {flagged.length > 0 && (
          <>
            <div className="coin-group">флаг · {flagged.length}</div>
            {flagged.map(row)}
          </>
        )}
        {rest.length > 0 && flagged.length > 0 && (
          <div className="coin-group">все · {rest.length}</div>
        )}
        {rest.map(row)}
        {watchlist.length === 0 && (
          <div className="lbl" style={{ padding: "6px 8px" }}>
            список пуст — добавь монету ниже
          </div>
        )}
      </div>
      <div className="coin-add">
        <input
          value={input}
          placeholder="DOGE/USDT"
          aria-label="добавить монету"
          onChange={(e) => {
            setInput(e.target.value);
            setFormErr(null);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") add();
          }}
        />
        <button className="btn b-acc" onClick={add} title="добавить монету">
          +
        </button>
      </div>
      {formErr && <div className="err">{formErr}</div>}
      <div className="status">
        {symbol} · last{" "}
        {Number.isFinite(lastPrice) ? lastPrice.toLocaleString("en-US") : "—"}
        {" · "}
        {status === "open" ? "● live" : status}
      </div>
      </>
      )}
    </div>
  );
}
