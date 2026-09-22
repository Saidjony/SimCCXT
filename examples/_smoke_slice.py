#!/usr/bin/env python3
"""Vertical-slice smoke test for the simccxt exchange simulator.

Rust engine is the source of truth. This exercises the full loop:
  historical data -> Rust replay (step feeds bars into broker) ->
  manual BUY -> Rust execution -> position + PnL -> account snapshot.

Run:  .venv/bin/python examples/_smoke_slice.py
"""
import os
import sys
import tempfile
import simccxt

def main():
    # 1. Feed historical OHLCV (synthetic but deterministic).
    ex = simccxt.binance()
    ex.add_market("BTC/USDT", "spot")
    ex.deposit("USDT", 100_000.0)
    ex.deposit("BTC", 0.0)

    rows = []
    ts = 1_700_000_000_000
    price = 50_000.0
    for i in range(20):
        o, c = price, price + (i % 5 - 2) * 100.0
        rows.append([ts + i * 60_000, o, max(o, c) * 1.001, min(o, c) * 0.999, c, 1.0 + i])
        price = c
    ex.load_candles("BTC/USDT", "1m", rows)

    start = rows[0][0]
    ex.reset(start)
    ex.resume()
    ex.set_step_ms(60_000)

    # 2. Replay a few bars -> Rust feeds bar closes into the broker.
    ex.step()
    ex.step()
    ex.step()
    print("now_ms =", ex.now_ms())
    print("market_snapshot:", ex.market_snapshot("BTC/USDT").get("last"))

    # 3. Manual BUY at the current replay price.
    order = ex.create_order("BTC/USDT", "market", "buy", 0.5, None)
    print("BUY order status:", order.get("status"), "avg=", order.get("average"))

    # 4. Account snapshot: position + balances + pnl.
    shot = ex.account_snapshot()
    print("positions:", shot["positions"])
    print("balances :", shot["balances"])
    print("pnl      :", shot["pnl"])
    print("equity   :", round(shot["equity"], 4))

    # 5. SELL to close -> PnL realizes into quote.
    order2 = ex.create_order("BTC/USDT", "market", "sell", 0.5, None)
    print("SELL order status:", order2.get("status"))
    shot2 = ex.account_snapshot()
    print("after close positions:", shot2["positions"])
    print("after close equity   :", round(shot2["equity"], 4))

    # 6. Fills (account's own executed trades) come from Rust, newest first.
    fills = ex.fetch_fills(None, None)
    print("fills:", [(f["side"], round(f["price"])) for f in fills])

    # 7. A take-profit SELL triggers when price rises to the TP level.
    ex.deposit("BTC", 0.5)
    ex.create_order("BTC/USDT", "market", "buy", 0.5, None)
    last = ex.market_snapshot("BTC/USDT").get("last") or 50_000.0
    tp = last * 1.001           # just above current — crossed on the next uptick
    ex.create_order("BTC/USDT", "take_profit", "sell", 0.5, None, {"stopPrice": tp})
    before = ex.account_snapshot()
    ex.run()
    after = ex.account_snapshot()
    triggered = len(after["positions"]) == 0 and len(before["positions"]) >= 1
    print("TP triggered (position flattened):", triggered)

    ok = len(shot["positions"]) >= 1 and shot["equity"] > 0 and len(fills) >= 2 and triggered
    print("\nSLICE SMOKE:", "OK" if ok else "CHECK")
    sys.exit(0 if ok else 1)

if __name__ == "__main__":
    main()