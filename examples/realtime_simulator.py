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

import os
import sys
import time

import numpy as np
import pandas as pd

import simccxt


def make_demo_csv(path: str, n: int = 20) -> str:
    """Write a small deterministic BTC/USDT 1s CSV to disk."""
    ts = pd.date_range("2026-03-01", periods=n, freq="s", tz="UTC")
    close = 100_000.0 + np.arange(n) * 10.0
    pd.DataFrame(
        {
            "timestamp": ts,
            "open": close - 2,
            "high": np.maximum(close - 2, close + 5),
            "low": close - 5,
            "close": close,
            "volume": 1.0,
        }
    ).to_csv(path, index=False)
    return path


def main() -> None:
    csv_path = make_demo_csv("/tmp/simccxt_demo_btc.csv")
    df = pd.read_csv(csv_path)

    ex = simccxt.binance()
    ex.add_market("BTC/USDT", "spot")

    # 1) data comes FROM DISK, not the internet
    ex.load_candles_from_csv("BTC/USDT", "1s", csv_path)
    ts_utc = pd.to_datetime(df["timestamp"], utc=True)
    first_ms = int(ts_utc.iloc[0].timestamp() * 1000)
    print(f"[sim] loaded {len(df)} bars from disk ({csv_path})")

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