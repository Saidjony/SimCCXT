#!/usr/bin/env python3
"""
realtime_simulator.py
=====================
Show simccxt behaving like a live exchange feed — data is read from a local
CSV on disk and delivered step-by-step via the simulator clock, instead of
being fetched from the internet like ccxt does.

The full OHLCV series is loaded into the Rust Exchange once; then:

    reset(start_time)          -> point the simulator clock at the first bar
    set_step_ms(1000)          -> one step = 1 simulated second
    resume()                   -> allow the clock to advance
    loop:
        now = ex.now_ms()
        bars = ex.fetch_ohlcv_live(symbol, timeframe)   # only bars <= now
        ex.step(1)
        (optional) time.sleep(...)                      # real-time pacing

Each call to `fetch_ohlcv_live` returns exactly what a live feed would have
delivered up to that moment — sourced from disk, deterministic.

Run:
    python3 examples/realtime_simulator.py
"""

import csv
import datetime as dt
import os
import sys
import time

import simccxt


def make_demo_csv(path: str, n: int = 20) -> str:
    """Write a small deterministic BTC/USDT 1s CSV to disk (stdlib only)."""
    start = dt.datetime(2026, 3, 1, tzinfo=dt.timezone.utc)
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["timestamp", "open", "high", "low", "close", "volume"])
        for i in range(n):
            close = 100_000.0 + i * 10.0
            ts = (start + dt.timedelta(seconds=i)).isoformat()
            w.writerow([ts, close - 2, close + 5, close - 5, close, 1.0])
    return path


def main() -> None:
    csv_path = make_demo_csv("/tmp/simccxt_demo_btc.csv")

    ex = simccxt.binance()
    ex.add_market("BTC/USDT", "spot")

    # 1) data comes FROM DISK, not the internet
    ex.load_candles_from_csv("BTC/USDT", "1s", csv_path)
    with open(csv_path, newline="") as f:
        n_bars = sum(1 for _ in f) - 1  # minus the header
    start = dt.datetime(2026, 3, 1, tzinfo=dt.timezone.utc)
    first_ms = int(start.timestamp() * 1000)
    print(f"[sim] loaded {n_bars} bars from disk ({csv_path})")

    # 2) simulator clock
    ex.reset(first_ms)
    ex.set_step_ms(1000)  # 1 step == 1 second of market time
    ex.resume()

    # 3) step-by-step live feed
    print("\nstep | now (ms)             | live bars | last close")
    print("-" * 62)
    count = 0
    while not ex.replay_done("BTC/USDT", "1s"):
        count += 1
        now = ex.now_ms()
        bars = ex.fetch_ohlcv_live("BTC/USDT", "1s")
        last = bars[-1] if bars else [0, 0, 0, 0, 0, 0]
        print(f"{count:>4} | {now:>13} | {len(bars):>8} | {last[4]:>10.1f}")
        ex.step(1)            # advance the simulator clock
        time.sleep(0.2)       # optional: pace it like realtime

    print("\nreplay finished. final now_ms =", ex.now_ms())
    print("simccxt delivered the data step-by-step from disk (deterministic).")


if __name__ == "__main__":
    sys.exit(main())