#!/usr/bin/env python3
"""
levels_finder.py
================
Находит уровни поддержки/сопротивления, сортирует их по цене, анализирует
временные интервалы и расстояния между соседними уровнями, строит графики.

Данные берутся через simccxt (виртуальный обмен): сначала генерируются
синтетические свечи, загружаются в Exchange, читаются обратно fetch_ohlcv.

Usage:
    ./venv/bin/python levels_finder.py
    ./venv/bin/python levels_finder.py --csv data/btc.csv
"""
from __future__ import annotations

import argparse
import os
import sys

import numpy as np
import pandas as pd

import simccxt  # native PyO3 binding


# ---------------------------------------------------------------------------
# 1. Генерация / загрузка данных через simccxt
# ---------------------------------------------------------------------------
def synthetic_candles(days: int = 2, seed: int = 7) -> pd.DataFrame:
    """Детерминированные синтетические 1-секундные свечи BTC/USDT."""
    n = days * 86_400
    rng = np.random.default_rng(seed)
    t = np.arange(n) / 86_400.0
    trend = 0.02 * t
    cycle = 0.04 * np.sin(2 * np.pi * t / 2.0) + 0.01 * np.sin(2 * np.pi * t * 3.0)
    micro = np.cumsum(rng.normal(0.0, 0.0005, n))
    logp = np.log(100_000.0) + trend + cycle + micro
    close = np.exp(logp)
    rng2 = np.random.default_rng(seed + 1)
    half = 0.0004 + 0.0012 * np.abs(rng2.normal(0.0, 1.0, n))
    open_ = np.concatenate(([close[0]], close[:-1]))
    high = np.maximum(open_, close) * (1.0 + half)
    low = np.minimum(open_, close) * (1.0 - half)
    volume = rng.lognormal(0.0, 0.5, n) * 0.5
    ts = pd.date_range("2026-01-01", periods=n, freq="s", tz="UTC")
    return pd.DataFrame(
        {"timestamp": ts, "open": open_, "high": high,
         "low": low, "close": close, "volume": volume}
    )


def to_ms(ts_series: pd.Series) -> np.ndarray:
    utc_naive = ts_series.dt.tz_convert("UTC").dt.tz_localize(None)
    ns = utc_naive.astype("datetime64[ns]").astype("int64")
    return ns // 10 ** 6


def load_data(args) -> pd.DataFrame:
    """Загрузить свечи (CSV или синтетические) и прокинуть через simccxt."""
    if args.csv:
        df = pd.read_csv(args.csv)
        df["timestamp"] = pd.to_datetime(df["timestamp"], utc=True)
        df = df.sort_values("timestamp").reset_index(drop=True)
        origin = f"CSV {args.csv}"
        print(f"[data] loaded {len(df):,} rows from {args.csv}")
    else:
        df = synthetic_candles(days=args.days, seed=args.seed)
        origin = f"synthetic ({args.days}d 1s bars, seed={args.seed})"
        print(f"[data] generated {len(df):,} rows of {origin}")

    df["ts_ms"] = to_ms(df["timestamp"])

    # Прокидываем через simccxt: загружаем и читаем обратно.
    ex = simccxt.binance()
    ex.add_market("BTC/USDT", "spot")
    ex.deposit("USDT", 1_000_000.0)
    rows = df[["ts_ms", "open", "high", "low", "close", "volume"]].to_numpy().tolist()
    ex.load_candles("BTC/USDT", "1s", rows)
    candles = ex.fetch_ohlcv("BTC/USDT", "1s", limit=None)
    out = pd.DataFrame(candles, columns=["ts_ms", "open", "high", "low", "close", "volume"])
    out["timestamp"] = pd.to_datetime(out["ts_ms"], unit="ms", utc=True)
    out = out.sort_values("ts_ms").reset_index(drop=True)
    print(f"[data] read {len(out):,} bars back via simccxt.fetch_ohlcv ({origin})")
    print(f"[data] span {out['timestamp'].min()} .. {out['timestamp'].max()}")
    return out
