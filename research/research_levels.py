#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
research_levels.py
==================
Market-level research for BTC/USDT using the *existing* simccxt Python API.

What it does
------------
1. data loading            -> feed OHLCV through simccxt.Exchange and bulk-read it back
2. level detection         -> causal (no-look-ahead) swing-high / swing-low pivot levels
3. level interaction       -> price entering a configurable zone around a level = a touch
4. feature extraction      -> context 10s before touch, volume, trade count, OI, ...
5. forward outcome calc    -> returns at 1/5/10/30/60s, max penetration / rejection, MFE / MAE
6. results                -> aggregate table + CSV
7. visualization           -> matplotlib figures

IMPORTANT — data availability
-----------------------------
The current simccxt Python API exposes OHLCV candles with 6 columns
[ts_ms, open, high, low, close, volume] via `load_candles` / `fetch_ohlcv`.

The following are NOT available in the Python API and are therefore reported as
NaN / plotted as "not available" (NOT fabricated):

  * per-candle trade count        (no method returns it; load_candles has no
                                   trade_count column)
  * per-candle Open Interest      (no set_open_interest / load_oi method in the
                                   Python API; fetch_open_interest only returns the
                                   current value, which is None unless set in Rust)
  * buy_volume / sell_volume      (OHLCV volume is total volume only)

Usage
-----
    # default: synthetic 3-day 1-second BTC/USDT demo data through simccxt
    python research_levels.py

    # real data: csv with columns   timestamp,open,high,low,close,volume
    #             (timestamp seconds or ms; existing simccxt backends use ms)
    python research_levels.py --csv data/btc.csv

    python research_levels.py --level-zone 0.001 --max-horizon 60
    python research_levels.py --method swing --pivot 5 --outdir output

Requires: simccxt (native), numpy, pandas, matplotlib (analysis/plots only).
"""

from __future__ import annotations

import argparse
import os
import sys

import numpy as np
import pandas as pd

import simccxt  # the existing native Python binding


# ---------------------------------------------------------------------------
# Availability report
# ---------------------------------------------------------------------------
def availability_report() -> None:
    """Document exactly which source fields the current Python API can provide."""
    print("=" * 72)
    print("simccxt Python API — market data availability")
    print("=" * 72)
    ex = simccxt.Exchange()
    methods = sorted(n for n in dir(ex) if not n.startswith("_"))
    print(f"Exchange methods exposed to Python: {methods}")
    print(
        "\nOHLCV candles:    available  (load_candles / fetch_ohlcv, "
        "[ts_ms, o, h, l, c, v])"
    )
    print("trade count:      NOT AVAILABLE per candle (missing method / column)")
    print("open interest:    NOT AVAILABLE as a series "
          "(no set_open_interest / load_oi in Python)")
    print("buy/sell volume:  NOT AVAILABLE (volume is aggregate)")
    print(
        "\nMissing Python API surface required for the above fields:\n"
        "  * a way to load per-candle trade_count (e.g. an extended candle\n"
        "    schema for load_candles, or a load_trades_agg method)\n"
        "  * set_open_interest(symbol, ...) / load_oi(...) to seed OI history\n"
        "  * replay stepping (step/run/seek are not exposed on the Python\n"
        "    Exchange) — without it, batch analysis must bulk-read fetch_ohlcv"
    )
    print("=" * 72)


# ---------------------------------------------------------------------------
# 1. data loading
# ---------------------------------------------------------------------------
def synthetic_candles(days: int = 3, seed: int = 7) -> pd.DataFrame:
    """Deterministic synthetic 1-second BTC/USDT candles (for a runnable demo)."""
    n = days * 86_400
    rng = np.random.default_rng(seed)
    t = np.arange(n) / 86_400.0                      # time in days
    trend = 0.03 * t
    cycle1 = 0.05 * np.sin(2 * np.pi * t / 3.0)      # 3-day cycle
    cycle2 = 0.015 * np.sin(2 * np.pi * t * 2.0)     # 2-day cycle
    micro = np.cumsum(rng.normal(0.0, 0.0006, n))    # small per-second noise
    logp = np.log(100_000.0) + trend + cycle1 + cycle2 + micro
    close = np.exp(logp)
    rng2 = np.random.default_rng(seed + 1)
    half = 0.0003 + 0.0015 * np.abs(rng2.normal(0.0, 1.0, n))  # intra-bar range
    open_ = np.concatenate(([close[0]], close[:-1]))
    high = np.maximum(open_, close) * (1.0 + half)
    low = np.minimum(open_, close) * (1.0 - half)
    volume = rng.lognormal(0.0, 0.5, n) * 0.5
    ts = pd.date_range("2026-01-01", periods=n, freq="s", tz="UTC")
    return pd.DataFrame(
        {"timestamp": ts, "open": open_, "high": high, "low": low,
         "close": close, "volume": volume}
    )


def _to_ms(ts_index: pd.Series) -> np.ndarray:
    """UTC datetime series -> epoch milliseconds (unit/timezone agnostic)."""
    utc_naive = ts_index.dt.tz_convert("UTC").dt.tz_localize(None)
    ns = utc_naive.astype("datetime64[ns]").astype("int64")
    return ns // 10**6


def load_data(args) -> tuple[pd.DataFrame, str]:
    """Load candles (real CSV or synthetic) and push them *through simccxt*."""
    if args.csv:
        df = pd.read_csv(args.csv)
        df["timestamp"] = pd.to_datetime(df["timestamp"], utc=True)
        df = df.sort_values("timestamp").reset_index(drop=True)
        col_ms = _to_ms(df["timestamp"])
        origin = "CSV"
        print(f"[data] loaded {len(df):,} rows from {args.csv}")
    else:
        df = synthetic_candles(days=args.days, seed=args.seed)
        col_ms = _to_ms(df["timestamp"])
        origin = f"synthetic ({args.days} days of 1s bars, seed={args.seed})"
        print(f"[data] generated {len(df):,} rows of {origin}")

    df["ts_ms"] = col_ms

    # Push through the existing simccxt Exchange API (only real methods).
    ex = simccxt.binance()
    ex.add_market("ETH/USDT", "spot")
    ex.deposit("USDT", 1_000_000.0)
    rows = df[["ts_ms", "open", "high", "low", "close", "volume"]].to_numpy().tolist()
    ex.load_candles("BTC/USDT", "1s", rows)
    candles = ex.fetch_ohlcv("BTC/USDT", "1s", limit=None)
    out = pd.DataFrame(candles, columns=["ts_ms", "open", "high", "low", "close", "volume"])
    out["timestamp"] = pd.to_datetime(out["ts_ms"], unit="ms", utc=True)
    out = out.sort_values("ts_ms").reset_index(drop=True)
    print(f"[data] read {len(out):,} bars back from simccxt.fetch_ohlcv ({origin})")
    print(f"[data] span {out['timestamp'].min()} .. {out['timestamp'].max()}")

    # columns that the API does not provide -> stay NaN (never fabricated)
    out["trade_count"] = np.nan
    out["buy_volume"] = np.nan
    out["sell_volume"] = np.nan
    out["open_interest"] = np.nan
    return out, origin


# ---------------------------------------------------------------------------
# 2. level detection (causal swing highs / swing lows)
# ---------------------------------------------------------------------------
def detect_swing_levels(bar: pd.DataFrame, pivot: int, zone: float,
                        dedupe_tolerance: float, min_swing: float = 0.0008,
                        max_levels: int = 40) -> pd.DataFrame:
    """
    Causal pivot detection.

    A bar i is a pivot high when `high[i]` is the strict max of
    [i-pivot, i+pivot] and exceeds the window neighbours by at least
    `min_swing` (fraction); it only becomes *known* once bar i+pivot exists,
    so the level's ``detected_at`` equals the confirmation bar. No future data
    leaks into the detection decision. The level list is capped at `max_levels`
    (evenly sampled over time) to keep the research table readable; raise
    ``--max-levels`` for exhaustive studies.
    """
    n = len(bar)
    highs = bar["high"].to_numpy()
    lows = bar["low"].to_numpy()
    ts_ms = bar["ts_ms"].to_numpy()

    records = []
    for i in range(pivot, n - pivot):
        hi = highs[i]
        lo = lows[i]
        left_hi = highs[i - pivot:i].max()
        right_hi = highs[i + 1:i + pivot + 1].max()
        left_lo = lows[i - pivot:i].min()
        right_lo = lows[i + 1:i + pivot + 1].min()
        if hi > left_hi and hi > right_hi and (hi - max(left_hi, right_hi)) >= min_swing * hi:
            kind, price = "resistance", hi
        elif lo < left_lo and lo < right_lo and (min(left_lo, right_lo) - lo) >= min_swing * lo:
            kind, price = "support", lo
        else:
            continue
        records.append(
            {
                "level_price": float(price),
                "direction": kind,
                "pivot_idx": i,
                "detected_idx": i + pivot,          # when the pivot becomes known
                "detected_at": pd.to_datetime(ts_ms[i + pivot], unit="ms", utc=True),
            }
        )

    levels = pd.DataFrame(records)
    if levels.empty:
        return levels

    # cluster / dedupe levels that fall inside the same tolerance band
    keep = []
    masked = np.array(levels["level_price"].to_numpy(), copy=True)
    tol = dedupe_tolerance
    for idx, row in levels.iterrows():
        if not np.isnan(row["level_price"]):
            keep.append(idx)
            same = np.abs(masked - row["level_price"]) <= tol * row["level_price"]
            masked[same] = np.nan
    levels = levels.loc[keep].reset_index(drop=True)

    # cap the number of levels (evenly spaced in time) for tractable output
    if len(levels) > max_levels:
        idx = np.linspace(0, len(levels) - 1, max_levels).round().astype(int)
        levels = levels.iloc[np.unique(idx)].reset_index(drop=True)

    levels["zone"] = zone
    n_sup = (levels["direction"] == "support").sum()
    print(f"[levels] detected {len(levels):,} levels "
          f"({n_sup:,} support / {len(levels) - n_sup:,} resistance, "
          f"capped at {max_levels})")
    return levels


# ---------------------------------------------------------------------------
# 3. level interaction detection + 4. feature extraction + 5. forward outcomes
# ---------------------------------------------------------------------------
def walk_level(bar: pd.DataFrame, level: pd.Series, zone: float,
               ctx_s: int, horizons_s: list[int], max_horizon_s: int,
               break_threshold: float, reject_threshold: float) -> pd.DataFrame:
    """
    Find every price <touch> (= zone entry) of one level and compute trigger
    features + forward outcomes. Everything strictly chronological; outcomes
    only ever use bars AFTER the touch bar.
    """
    ts = bar["ts_ms"].to_numpy()
    close = bar["close"].to_numpy()
    high = bar["high"].to_numpy()
    low = bar["low"].to_numpy()
    volume = bar["volume"].to_numpy()

    price = float(level["level_price"])
    direction = level["direction"]
    upper = price * (1.0 + zone)
    lower = price * (1.0 - zone)
    start = int(level["detected_idx"]) + 1          # level known only after confirmation

    in_zone = (close >= lower) & (close <= upper) & (np.arange(len(bar)) >= start)
    if direction == "support":
        entry_flag = in_zone & np.concatenate(([False], close[:-1] > upper))
    else:
        entry_flag = in_zone & np.concatenate(([False], close[:-1] < lower))

    entry_idx = np.flatnonzero(entry_flag)
    if entry_idx.size == 0:
        return pd.DataFrame()

    # baseline (rolling) volume stats for z-scores — computed only on the past
    vol_series = pd.Series(volume)
    vol_mean = vol_series.rolling(300, min_periods=50).mean().to_numpy()
    vol_std = vol_series.rolling(300, min_periods=50).std(ddof=0).to_numpy()

    rows = []
    for e in entry_idx:
        t0 = ts[e]
        entry_price = close[e]
        t_end = t0 + max_horizon_s * 1000
        fwd_end = int(np.searchsorted(ts, t_end, side="right"))
        fwd = slice(e + 1, fwd_end)                 # strictly after the touch bar

        # trigger / context window [t0 - ctx_s, t0)
        ctx_end = e
        ctx_start = int(np.searchsorted(ts, t0 - ctx_s * 1000, side="left"))
        ctx = slice(ctx_start, ctx_end)

        row: dict = {
            "level_price": price,
            "direction": direction,
            "touch_ts": pd.to_datetime(t0, unit="ms", utc=True),
            "entry_price": entry_price,
            "distance_to_level_pct": (entry_price - price) / price * 100.0,
            # context window features (10s before the touch)
            "ctx_volume": float(volume[ctx].sum()),
            "ctx_trade_count": np.nan,
            "ctx_oi_change": np.nan,
            "volume_zscore": (
                _safe_zscore(volume[ctx].sum(), vol_mean[e], vol_std[e])
                if ctx_start < e else np.nan
            ),
            "trade_count_zscore": np.nan,
            "ctx_price_change_pct": _safe_pct(close[e], close[ctx_start]) if ctx_start < e else np.nan,
        }

        # forward outcome features (only bars strictly after the touch bar;
        # MFE/MAE are measured from the entry price and include the entry bar)
        fw_hi = high[fwd].max() if fwd_end > e + 1 else np.nan
        fw_lo = low[fwd].min() if fwd_end > e + 1 else np.nan
        mfe_hi = max(high[e], fw_hi) if not np.isnan(fw_hi) else high[e]
        mae_lo = min(low[e], fw_lo) if not np.isnan(fw_lo) else low[e]
        if direction == "support":
            penetration = (price - fw_lo) / price if not np.isnan(fw_lo) else np.nan
            rejection = (fw_hi - price) / price if not np.isnan(fw_hi) else np.nan
            mfe = (mfe_hi - entry_price) / entry_price
            mae = (entry_price - mae_lo) / entry_price
            breakout = penetration >= break_threshold
            rejected = (not bool(breakout)) and rejection >= reject_threshold
        else:
            penetration = (fw_hi - price) / price if not np.isnan(fw_hi) else np.nan
            rejection = (price - fw_lo) / price if not np.isnan(fw_lo) else np.nan
            mfe = (entry_price - mae_lo) / entry_price
            mae = (mfe_hi - entry_price) / entry_price
            breakout = penetration >= break_threshold
            rejected = (not bool(breakout)) and rejection >= reject_threshold

        row["max_penetration_pct"] = penetration
        row["max_rejection_pct"] = rejection
        row["max_favorable_excursion"] = mfe
        row["max_adverse_excursion"] = mae
        row["breakout"] = bool(breakout) if not np.isnan(penetration) else False
        row["rejection"] = bool(rejected)

        for h in horizons_s:
            t_h = t0 + h * 1000
            i_h = int(np.searchsorted(ts, t_h, side="right")) - 1
            row[f"price_{h}s"] = close[i_h] if i_h > e or i_h == e else np.nan
            row[f"return_{h}s"] = _safe_pct(close[i_h], entry_price) if i_h > e else np.nan

        rows.append(row)

    return pd.DataFrame(rows)


def _safe_pct(a: float, b: float) -> float:
    return np.nan if b == 0 or np.isnan(a) or np.isnan(b) else (a - b) / b * 100.0


def _safe_zscore(x: float, mu: float, sd: float) -> float:
    if sd is None or np.isnan(sd) or sd == 0:
        return np.nan
    return (x - mu) / sd


def run_research(bar: pd.DataFrame, levels: pd.DataFrame, args) -> tuple[pd.DataFrame, pd.DataFrame]:
    """Detect interactions for every level, drain forward outcomes to results."""
    frames = []
    for _, lv in levels.iterrows():
        sub = walk_level(bar, lv, args.zone, args.ctx_s, args.horizons,
                         args.max_horizon, args.break_px, args.reject_px)
        if len(sub):
            frames.append(sub)
    if not frames:
        print("[research] no level interactions found — relax the zone "
              "(--level-zone) or lower the pivot lookback.")
        return pd.DataFrame(), pd.DataFrame()
    return pd.concat(frames, ignore_index=True), levels


# ---------------------------------------------------------------------------
# 6. results
# ---------------------------------------------------------------------------
def summarize(results: pd.DataFrame, horizons: list[int]) -> pd.DataFrame:
    """Aggregate per-level table: touches, averages, rejection/breakout rates."""
    if results.empty:
        return pd.DataFrame()
    g = results.groupby(["level_price", "direction"], as_index=False)
    agg: dict = {
        "touches": ("entry_price", "size"),
        "avg_trades": ("ctx_trade_count", "mean"),
        "avg_volume": ("ctx_volume", "mean"),
        "avg_oi_change": ("ctx_oi_change", "mean"),
        "rejection_rate": ("rejection", "mean"),
        "breakout_rate": ("breakout", "mean"),
        "avg_penetration": ("max_penetration_pct", "mean"),
        "avg_rejection_dist": ("max_rejection_pct", "mean"),
    }
    for h in horizons:
        agg[f"return_{h}s"] = (f"return_{h}s", "mean")
    return g.agg(**agg)


# ---------------------------------------------------------------------------
# 7. visualization
# ---------------------------------------------------------------------------
def plot_all(bar: pd.DataFrame, results: pd.DataFrame, orig: pd.DataFrame,
             outdir: str, horizons: list[int]) -> None:
    if importlib_import():
        return
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    os.makedirs(outdir, exist_ok=True)
    t = bar["timestamp"]

    # 1) price + levels
    fig, ax = plt.subplots(figsize=(12, 5))
    step = max(1, len(bar) // 40_000)          # downsample for plotting
    ax.plot(t[::step], bar["close"].iloc[::step], lw=0.5, color="k", label="BTC close")
    for _, lv in orig.iterrows():
        color = {"support": "green", "resistance": "red"}[lv["direction"]]
        ax.axhline(lv["level_price"], color=color, lw=0.7, ls="--", alpha=0.8)
    ax.set_title("1) BTC/USDT price and detected levels (green=support, red=resistance)")
    ax.legend(loc="upper left", fontsize=8)
    fig.savefig(os.path.join(outdir, "1_prices_levels.png"), dpi=110)
    plt.close(fig)

    # 2) trade count around the level — NOT AVAILABLE → honest placeholder
    _placeholder_plot(os.path.join(outdir, "2_trades_around_level.png"),
                      "Per-candle trade count is NOT available in the simccxt\n"
                      "Python API (load_candles schema has no trade_count).\n"
                      "-> add a trade_count column / load_trades_agg method.")

    # 3) average volume around the touch (offsets relative to touch bar)
    fig, ax = plt.subplots(figsize=(9, 4))
    offsets = [-10, -5, -2, -1, 0, 1, 2, 5, 10, 30, 60]
    # proper per-touch volume profile
    prof = _touch_volume_profile(bar, results, offsets)
    ax.plot(offsets, prof, marker="o")
    ax.axvline(0, color="grey", ls=":")
    ax.set_xlabel("seconds relative to level touch")
    ax.set_ylabel("avg volume")
    ax.set_title("3) average traded volume around level touches")
    fig.savefig(os.path.join(outdir, "3_volume_around_level.png"), dpi=110)
    plt.close(fig)

    # 4) open interest — NOT AVAILABLE → honest placeholder
    _placeholder_plot(os.path.join(outdir, "4_open_interest.png"),
                      "Open Interest history is NOT available in the simccxt\n"
                      "Python API (no set_open_interest / load_oi method;\n"
                      "fetch_open_interest returns the current value only).")

    # 5) distribution of forward returns
    fig, axes = plt.subplots(len(horizons), 1, figsize=(9, 3 * len(horizons)),
                             squeeze=False)
    for ax, h in zip(axes[:, 0], horizons):
        col = f"return_{h}s"
        if col in results and results[col].notna().any():
            ax.hist(results[col].dropna(), bins=40, alpha=0.7)
            ax.set_title(f"distribution of {h}s forward return (%)")
        else:
            ax.text(0.5, 0.5, "no data", ha="center")
    fig.suptitle("5) forward-return distributions after level touches")
    fig.savefig(os.path.join(outdir, "5_forward_returns.png"), dpi=110)
    plt.close(fig)

    print(f"[plots] saved 5 figures to {outdir}/")


def importlib_import() -> bool:
    """Return True if matplotlib is unavailable (plots are skipped)."""
    try:  # noqa
        import matplotlib  # noqa
        return False
    except Exception:
        print("[plots] matplotlib not installed — skipping figures.")
        return True


def _placeholder_plot(path: str, text: str) -> None:
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    fig, ax = plt.subplots(figsize=(9, 3))
    ax.text(0.5, 0.5, text, ha="center", va="center", fontsize=11)
    ax.set_axis_off()
    fig.savefig(path, dpi=110)
    plt.close(fig)


def _touch_volume_profile(bar: pd.DataFrame, results: pd.DataFrame,
                          offsets: list[int]) -> list[float]:
    ts = bar["ts_ms"].to_numpy()
    vol = bar["volume"].to_numpy()
    means = []
    for off in offsets:
        vals = []
        for t0_utc in results["touch_ts"]:
            t0 = int(t0_utc.timestamp() * 1000)
            target = t0 + off * 1000
            j = int(np.searchsorted(ts, target, side="right")) - 1
            if j >= 0:
                vals.append(vol[j])
        means.append(np.mean(vals) if vals else np.nan)
    return means


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
def parse_args(argv=None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description="BTC/USDT market-level research (simccxt)")
    p.add_argument("--csv", help="path to OHLCV csv "
                                 "cols: timestamp,open,high,low,close,volume")
    p.add_argument("--days", type=int, default=3, help="synthetic demo length in days")
    p.add_argument("--seed", type=int, default=7, help="RNG seed (deterministic)")
    p.add_argument("--pivot", type=int, default=5, help="swing pivot lookback bars")
    p.add_argument("--min-swing", type=float, default=0.0008, dest="min_swing",
                   help="minimum pivot prominence as a fraction (0.08%%)")
    p.add_argument("--max-levels", type=int, default=40, dest="max_levels",
                   help="cap on number of levels analysed (evenly sampled)")
    p.add_argument("--top", type=int, default=20,
                   help="print only the top-N rows of the summary table")
    p.add_argument("--level-zone", type=float, default=0.001, dest="zone",
                   help="level zone as a fraction (default 0.001 = 0.1%%)")
    p.add_argument("--ctx-s", type=int, default=10, dest="ctx_s",
                   help="context window seconds before the touch")
    p.add_argument("--max-horizon", type=int, default=60, dest="max_horizon",
                   help="max forward window seconds")
    p.add_argument("--horizons", type=int, nargs="+", default=[1, 5, 10, 30, 60],
                   help="forward horizons (seconds)")
    p.add_argument("--break-px", type=float, default=0.002, dest="break_px",
                   help="penetration fraction that counts as breakout (default 0.2%%)")
    p.add_argument("--reject-px", type=float, default=0.001, dest="reject_px",
                   help="rejection distance fraction (default 0.1%%)")
    p.add_argument("--outdir", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "output"),
                   help="output directory")
    return p.parse_args(argv)


def main(argv=None) -> int:
    args = parse_args(argv)
    os.makedirs(args.outdir, exist_ok=True)

    availability_report()

    # 1. data loading
    bar, origin = load_data(args)

    # 2. level detection
    levels = detect_swing_levels(bar, args.pivot, args.zone,
                                 dedupe_tolerance=args.zone,
                                 min_swing=args.min_swing,
                                 max_levels=args.max_levels)

    # 3 + 4 + 5. interactions, features, forward outcomes
    results, levels = run_research(bar, levels, args)

    # 6. results
    summary = summarize(results, args.horizons)
    if summary.empty:
        print("\nNo touches found with the given settings.")
        return 1

    order = ["touches", "avg_trades", "avg_volume", "avg_oi_change",
             "rejection_rate", "breakout_rate"]
    for h in args.horizons:
        order.append(f"return_{h}s")
    order += ["avg_penetration", "avg_rejection_dist"]

    print("\n" + "=" * 110)
    print("RESULT — per-level market behaviour (avg of level touches)")
    print("=" * 110)
    fmt = {c: lambda v: f"{v:>10.4f}" if not isinstance(v, (np.integer, int))
           else f"{v:>10d}" for c in order}
    table = summary.set_index(["level_price", "direction"])
    print(table[order].head(args.top).to_string(formatters=fmt))
    print("-" * 110)
    print("avg_trades / avg_oi_change are NaN: trade count and Open Interest are")
    print("NOT available in the current simccxt Python API (see availability report).")
    print("All raw per-touch measurements are saved to CSV for your own hypotheses.")
    print("levels with no interactions are not shown; full summary in CSV.")
    print("=" * 110)

    results.to_csv(os.path.join(args.outdir, "touches.csv"), index=False)
    summary.to_csv(os.path.join(args.outdir, "levels_summary.csv"), index=False)
    print(f"\n[saved] touches.csv ({len(results):,} rows) and levels_summary.csv "
          f"in {args.outdir}/")

    # 7. visualization
    plot_all(bar, results, levels, os.path.join(args.outdir, "plots"), args.horizons)
    print(f"\nDone. Data origin: {origin}. Re-run with --seed/--csv for other data.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
