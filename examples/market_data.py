#!/usr/bin/env python3
"""
market_data.py
==============
Disk cache + internet download for simccxt research (Binance public klines).

Logic ("if the coin is not in the laptop's memory, fetch it from internet"):
  cache:  data/ohlcv/{SYMBOL}_{tf}.csv   (timestamp ms, open, high, low, close, volume)
  1. read the cache file if it exists;
  2. compute which bars of [start, end) are missing (bar-grid aligned);
  3. download ONLY the missing segments from Binance REST klines (paginated);
  4. merge + dedupe + sort, write the cache back, return the window.

No API key needed (public klines endpoint). stdlib only (urllib) + pandas.
"""

import json
import os
import time
import urllib.request

import pandas as pd

TF_MS = {"1m": 60_000, "5m": 300_000, "15m": 900_000, "1h": 3_600_000}
CACHE_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                         "..", "data", "ohlcv")

BINANCE = "https://api.binance.com/api/v3/klines"


def to_binance_symbol(symbol: str) -> str:
    """"BTC/USDT" -> "BTCUSDT" (Binance spot)."""
    base, _, quote = symbol.partition("/")
    if not base or not quote:
        raise ValueError(f"bad symbol {symbol!r}, expected BASE/QUOTE")
    return (base + quote).upper()


def cache_path(symbol: str, tf: str, cache_dir: str = CACHE_DIR) -> str:
    return os.path.join(cache_dir, f"{to_binance_symbol(symbol)}_{tf}.csv")


def fetch_klines(symbol: str, tf: str, start_ms: int, end_ms: int,
                 limit: int = 1000, pause: float = 0.15) -> list:
    """Download klines [start_ms, end_ms] from Binance (paginated)."""
    if tf not in TF_MS:
        raise ValueError(f"unsupported timeframe {tf!r} (need one of {sorted(TF_MS)})")
    sym = to_binance_symbol(symbol)
    step = TF_MS[tf]
    out, cursor = [], int(start_ms)
    while cursor <= int(end_ms):
        url = (f"{BINANCE}?symbol={sym}&interval={tf}"
               f"&startTime={cursor}&endTime={int(end_ms)}&limit={limit}")
        req = urllib.request.Request(url, headers={"User-Agent": "simccxt/0.1"})
        with urllib.request.urlopen(req, timeout=30) as r:
            rows = json.loads(r.read().decode())
        if not rows:
            break
        for k in rows:
            out.append([int(k[0]), float(k[1]), float(k[2]),
                        float(k[3]), float(k[4]), float(k[5])])
        last_open = int(rows[-1][0])
        if len(rows) < limit or last_open < cursor:
            break
        cursor = last_open + step
        time.sleep(pause)
    return out


def read_cache(symbol: str, tf: str, cache_dir: str = CACHE_DIR) -> pd.DataFrame:
    path = cache_path(symbol, tf, cache_dir)
    if not os.path.exists(path):
        return pd.DataFrame(columns=["ts_ms", "open", "high", "low", "close", "volume"])
    df = pd.read_csv(path)
    return df.sort_values("ts_ms").reset_index(drop=True)


def write_cache(df: pd.DataFrame, symbol: str, tf: str,
                cache_dir: str = CACHE_DIR) -> str:
    os.makedirs(cache_dir, exist_ok=True)
    path = cache_path(symbol, tf, cache_dir)
    df.sort_values("ts_ms").to_csv(path, index=False)
    return path


def load_or_download(symbol: str, tf: str, start_ms: int, end_ms: int,
                     cache_dir: str = CACHE_DIR) -> tuple[pd.DataFrame, dict]:
    """Return bars for [start_ms, end_ms); download only what's missing.

    Returns (window_df, stats) with stats = {cached, downloaded, source}.
    """
    step = TF_MS[tf]
    want = np_arange(start_ms, end_ms, step)
    cached = read_cache(symbol, tf, cache_dir)
    have = set(cached["ts_ms"].astype("int64").tolist()) if len(cached) else set()
    missing = [t for t in want if t not in have]

    downloaded: list = []
    if missing:
        # group consecutive missing bars into segments, fetch each as one range
        segs, s = [], missing[0]
        prev = missing[0]
        for t in missing[1:]:
            if t == prev + step:
                prev = t
            else:
                segs.append((s, prev))
                s, prev = t, t
        segs.append((s, prev))
        for a, b in segs:
            downloaded.extend(fetch_klines(symbol, tf, a, b))

    if downloaded:
        new = pd.DataFrame(downloaded,
                           columns=["ts_ms", "open", "high", "low", "close", "volume"])
        full = pd.concat([cached, new], ignore_index=True)
        full = full.drop_duplicates("ts_ms").sort_values("ts_ms").reset_index(drop=True)
        write_cache(full, symbol, tf, cache_dir)
    else:
        full = cached

    window = full[(full["ts_ms"] >= start_ms) & (full["ts_ms"] < end_ms)]
    window = window.reset_index(drop=True)
    stats = {"cached": len(cached), "downloaded": len(downloaded),
             "source": "binance" if (len(cached) or len(downloaded)) else "empty"}
    return window, stats


def np_arange(start: int, end: int, step: int) -> list:
    return list(range(int(start), int(end), int(step)))
