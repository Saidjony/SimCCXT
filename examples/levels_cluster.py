#!/usr/bin/env python3
"""
levels_cluster.py
=================
Level research WITHOUT swing pivots and WITHOUT fixed rules like ±0.1%.

Pipeline (exactly as specified):
    all bars -> all prices -> sort by price -> price clusters (adaptive eps)
    -> occurrence times of each cluster -> Δprice / Δtime analysis
    -> levels = price zones with a repetition structure over time

A level is a *price zone* [lo, hi] whose width comes from the data itself.
eps (the "close in price" scale) is adaptive: half of the median bar range.
No fixed percent tolerance, no local extrema.

Time structure per cluster: repetitions (count), lifespan (duration),
distinct visits (segments split by a time gap), Δtime gaps, Δprice gaps.

Detection is retrospective by construction (needs the whole series to sort);
interaction *outcomes* are measured strictly after each touch bar.

Only the existing simccxt Python API is used (load_candles + fetch_ohlcv).
trade count / OI / buy-sell split are still unavailable there -> NaN, honest.

Run:
    python3 examples/levels_cluster.py [--days 5] [--csv xrp_1m.csv]
"""

import argparse
import os
import sys

import numpy as np
import pandas as pd

import simccxt

SYMBOL = "XRP/USDT"
TF = "1m"
TF_MS = 60_000


# ------------------------------------------------------------------ data
def synthetic_xrp_1m(days: int = 5, seed: int = 11) -> pd.DataFrame:
    """Deterministic 1-minute XRP bars (~2.5 USDT) with wave structure."""
    n = days * 24 * 60
    rng = np.random.default_rng(seed)
    t = np.arange(n) / (24 * 60.0)                      # days
    logp = (np.log(2.5) + 0.02 * t
            + 0.06 * np.sin(2 * np.pi * t / 2.0)        # 2-day wave
            + 0.018 * np.sin(2 * np.pi * t * 3.0)       # 8h wave
            + np.cumsum(rng.normal(0.0, 0.00035, n)))
    close = np.exp(logp)
    rng2 = np.random.default_rng(seed + 1)
    half = 0.0003 + 0.0012 * np.abs(rng2.normal(0.0, 1.0, n))
    open_ = np.concatenate(([close[0]], close[:-1]))
    ts = pd.date_range("2026-04-01", periods=n, freq="min", tz="UTC")
    return pd.DataFrame({
        "timestamp": ts,
        "open": open_,
        "high": np.maximum(open_, close) * (1.0 + half),
        "low": np.minimum(open_, close) * (1.0 - half),
        "close": close,
        "volume": rng.lognormal(0.0, 0.45, n) * 40.0,
    })


def to_ms(ts: pd.Series) -> np.ndarray:
    naive = ts.dt.tz_convert("UTC").dt.tz_localize(None)
    return (naive.astype("datetime64[ns]").astype("int64") // 10**6).to_numpy()


def load_bars(args) -> tuple[pd.DataFrame, str]:
    if args.csv:
        df = pd.read_csv(args.csv)
        df["timestamp"] = pd.to_datetime(df["timestamp"], utc=True)
        df = df.sort_values("timestamp").reset_index(drop=True)
        origin = f"CSV {args.csv}"
    else:
        df = synthetic_xrp_1m(days=args.days, seed=args.seed)
        origin = f"synthetic XRP {args.days}d/1m seed={args.seed}"
    df["ts_ms"] = to_ms(df["timestamp"])

    ex = simccxt.binance()
    ex.add_market(SYMBOL, "spot")
    ex.load_candles(SYMBOL, TF, df[["ts_ms", "open", "high", "low",
                                    "close", "volume"]].to_numpy().tolist())
    out = pd.DataFrame(ex.fetch_ohlcv(SYMBOL, TF, limit=None),
                       columns=["ts_ms", "open", "high", "low", "close", "volume"])
    out["timestamp"] = pd.to_datetime(out["ts_ms"], unit="ms", utc=True)
    out = out.sort_values("ts_ms").reset_index(drop=True)
    print(f"[data] {len(out):,} bars via simccxt ({origin})")
    return out, origin


# ------------------------------------------------- price clustering
def cluster_prices(prices: np.ndarray, times_ms: np.ndarray,
                   bin_width: float, density_mult: float):
    """Density clustering over price-sorted values.

    Bin the sorted closes with an adaptive width; bins holding far more bars
    than the median bin are repetition zones. Adjacent dense bins merge.
    Returns (clusters, baseline_count, threshold_count, nbins).
    """
    lo = float(prices.min())
    edges = np.arange(lo, float(prices.max()) + bin_width, bin_width)
    if len(edges) < 3:
        edges = np.array([lo, lo + bin_width, lo + 2 * bin_width])
    idx = np.clip(np.digitize(prices, edges) - 1, 0, len(edges) - 2)
    counts = np.bincount(idx, minlength=len(edges) - 1)
    baseline = float(np.median(counts))
    thresh = max(density_mult * baseline, 1.0)
    dense = np.flatnonzero(counts >= thresh)

    clusters = []
    if dense.size:
        # merge adjacent dense bins into zones
        groups, cur = [], [int(dense[0])]
        for b in dense[1:]:
            if b == cur[-1] + 1:
                cur.append(int(b))
            else:
                groups.append(cur)
                cur = [int(b)]
        groups.append(cur)
        for g in groups:
            m = np.isin(idx, g)
            clusters.append({"prices": prices[m], "times": times_ms[m],
                             "lo": float(edges[g[0]]), "hi": float(edges[g[-1] + 1]),
                             "bin_count": int(counts[g].sum())})
    return clusters, baseline, thresh, len(edges) - 1


def time_structure(times_ms: np.ndarray, visit_gap_ms: int) -> dict:
    """Repetition structure of one price cluster over time."""
    t = np.sort(times_ms)
    gaps = np.diff(t) if len(t) > 1 else np.array([], dtype=np.int64)
    # distinct visits: segments separated by a long absence
    bounds, s = [], 0
    for i, g in enumerate(gaps):
        if g > visit_gap_ms:
            bounds.append((t[s], t[i]))
            s = i + 1
    bounds.append((t[s], t[-1]))
    starts = np.array([b[0] for b in bounds])
    visit_gaps = np.diff(starts) if len(starts) > 1 else np.array([], dtype=np.int64)
    return {
        "count": int(len(t)),
        "duration_ms": int(t[-1] - t[0]) if len(t) > 1 else 0,
        "gaps_ms": gaps,
        "n_visits": len(bounds),
        "median_gap_ms": float(np.median(gaps)) if len(gaps) else 0.0,
        "max_gap_ms": int(gaps.max()) if len(gaps) else 0,
        "median_visit_gap_ms": float(np.median(visit_gaps)) if len(visit_gaps) else 0.0,
    }


def detect_levels(bar: pd.DataFrame, bin_width: float, density_mult: float,
                  visit_gap_ms: int, min_count: int, min_duration_h: float,
                  min_visits: int, max_levels: int) -> pd.DataFrame:
    """All closes -> sort -> dense price bands -> keep zones with time structure."""
    prices = bar["close"].to_numpy()
    times = bar["ts_ms"].to_numpy()
    clusters, baseline, thresh, nbins = cluster_prices(prices, times,
                                                       bin_width, density_mult)
    print(f"[cluster] {nbins} price bins, median bin count={baseline:.1f}, "
          f"dense threshold={thresh:.1f} -> {len(clusters)} candidate zones")

    rows = []
    for c in clusters:
        ts = time_structure(c["times"], visit_gap_ms)
        if ts["count"] < min_count:
            continue
        if ts["duration_ms"] < min_duration_h * 3600_000:
            continue
        if ts["n_visits"] < min_visits:
            continue
        p = c["prices"]
        # Δprice: gaps between neighbouring sorted prices inside the zone
        pgaps = np.diff(np.sort(p))
        pad = bin_width / 2.0
        lo, hi = c["lo"] - pad, c["hi"] + pad   # data-derived zone
        rows.append({
            "price": float(np.median(p)),
            "lo": float(lo),
            "hi": float(hi),
            "count": ts["count"],
            "duration_h": ts["duration_ms"] / 3600_000,
            "n_visits": ts["n_visits"],
            "median_dtime_min": ts["median_gap_ms"] / 60_000,
            "median_visit_gap_min": ts["median_visit_gap_ms"] / 60_000,
            "max_dtime_min": ts["max_gap_ms"] / 60_000,
            "median_dprice": float(np.median(pgaps)) if len(pgaps) else 0.0,
            "score": ts["count"] * (1.0 + np.log1p(ts["duration_ms"] / 3600_000))
                     * ts["n_visits"],
            "times": np.sort(c["times"]),
        })
    levels = pd.DataFrame(rows)
    if levels.empty:
        return levels
    levels = levels.sort_values("score", ascending=False).head(max_levels)
    levels = levels.reset_index(drop=True)
    print(f"[levels] kept {len(levels)} zones with repetition structure")
    return levels


# --------------------------------------- interactions + outcomes (causal part)
def walk_zone(bar: pd.DataFrame, lv: pd.Series, ctx_s: int, horizons_s: list[int],
              max_horizon_s: int) -> pd.DataFrame:
    """Touches of one data-derived zone + strictly-post-touch outcomes."""
    ts = bar["ts_ms"].to_numpy()
    close = bar["close"].to_numpy()
    high = bar["high"].to_numpy()
    low = bar["low"].to_numpy()
    volume = bar["volume"].to_numpy()

    lo, hi, mid = float(lv["lo"]), float(lv["hi"]), float(lv["price"])
    half_px = max((hi - lo) / 2.0 / mid, 0.0)   # data-derived scale, no fixed %
    in_zone = (close >= lo) & (close <= hi)
    entry_above = in_zone & np.concatenate(([False], close[:-1] > hi))
    entry_below = in_zone & np.concatenate(([False], close[:-1] < lo))
    entries = np.flatnonzero(entry_above | entry_below)
    if entries.size == 0:
        return pd.DataFrame()

    vol = pd.Series(volume)
    vm = vol.rolling(300, min_periods=50).mean().to_numpy()
    vs = vol.rolling(300, min_periods=50).std(ddof=0).to_numpy()

    out = []
    for e in entries:
        t0, entry = int(ts[e]), close[e]
        side = +1 if close[e - 1] > hi else -1      # +1 from above, -1 from below
        fwd_end = int(np.searchsorted(ts, t0 + max_horizon_s * 1000, side="right"))
        fwd = slice(e + 1, fwd_end)                 # strictly after the touch bar
        cs = int(np.searchsorted(ts, t0 - ctx_s * 1000, side="left"))
        ctx = slice(cs, e)

        if side > 0:                                # approached from above
            pen = (lo - low[fwd].min()) / mid if fwd.stop > fwd.start else np.nan
            rej = (high[fwd].max() - hi) / mid if fwd.stop > fwd.start else np.nan
        else:                                       # approached from below
            pen = (high[fwd].max() - hi) / mid if fwd.stop > fwd.start else np.nan
            rej = (lo - low[fwd].min()) / mid if fwd.stop > fwd.start else np.nan
        breakout = bool(pen >= half_px) if not np.isnan(pen) else False
        reject = (not breakout) and (not np.isnan(rej)) and bool(rej >= half_px)

        row = {
            "level": mid, "side": "above" if side > 0 else "below",
            "touch_ts": pd.to_datetime(t0, unit="ms", utc=True),
            "entry": entry, "dist_pct": (entry - mid) / mid * 100.0,
            "ctx_volume": float(volume[ctx].sum()),
            "ctx_trade_count": np.nan, "ctx_oi_change": np.nan,
            "volume_zscore": _z(volume[ctx].sum(), vm[e], vs[e]) if cs < e else np.nan,
            "penetration": pen, "rejection": rej,
            "breakout": breakout, "reject": reject,
        }
        for h in horizons_s:
            j = int(np.searchsorted(ts, t0 + h * 1000, side="right")) - 1
            row[f"ret_{h}s"] = ((close[j] - entry) / entry * 100.0) if j > e else np.nan
        out.append(row)
    return pd.DataFrame(out)


def _z(x, mu, sd):
    return np.nan if sd is None or np.isnan(sd) or sd == 0 else (x - mu) / sd


def summarize(res: pd.DataFrame, horizons: list[int]) -> pd.DataFrame:
    if res.empty:
        return res
    g = res.groupby("level", as_index=False)
    agg = {"touches": ("entry", "size"),
           "from_above": ("side", lambda s: (s == "above").sum()),
           "from_below": ("side", lambda s: (s == "below").sum()),
           "avg_trades": ("ctx_trade_count", "mean"),
           "avg_volume": ("ctx_volume", "mean"),
           "avg_oi_change": ("ctx_oi_change", "mean"),
           "rejection_rate": ("reject", "mean"),
           "breakout_rate": ("breakout", "mean")}
    for h in horizons:
        agg[f"return_{h}s"] = (f"ret_{h}s", "mean")
    return g.agg(**agg)


# ------------------------------------------------------------------ plots
def plot_all(bar, levels, res, outdir, horizons):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    os.makedirs(outdir, exist_ok=True)
    t = bar["timestamp"]

    # 1) price + zones (shaded bands)
    fig, ax = plt.subplots(figsize=(12, 5))
    step = max(1, len(bar) // 60_000)
    ax.plot(t[::step], bar["close"].iloc[::step], lw=0.6, color="k")
    for _, lv in levels.iterrows():
        ax.axhspan(lv["lo"], lv["hi"], color="orange", alpha=0.25)
        ax.axhline(lv["price"], color="darkorange", lw=0.8)
    ax.set_title("1) XRP/USDT + data-derived zones (bands, not lines)")
    fig.savefig(f"{outdir}/1_price_zones.png", dpi=110)
    plt.close(fig)

    # 2) sorted-price rank plot: the "sort by price" step made visible
    fig, ax = plt.subplots(figsize=(12, 4))
    prices = bar["close"].to_numpy()
    order = np.argsort(prices, kind="stable")
    ax.scatter(np.arange(len(prices)), prices[order], s=1, color="grey", alpha=0.5)
    for _, lv in levels.iterrows():
        ax.axhspan(lv["lo"], lv["hi"], color="orange", alpha=0.3)
    ax.set_xlabel("rank in price-sorted order")
    ax.set_ylabel("price")
    ax.set_title("2) all prices sorted — dense bands = repetition zones")
    fig.savefig(f"{outdir}/2_price_rank.png", dpi=110)
    plt.close(fig)

    top = levels.iloc[0]
    sub = res[res["level"] == top["price"]] if len(res) else res
    # 3) time structure of the top zone: occurrences + Δtime gaps
    fig, axes = plt.subplots(2, 1, figsize=(12, 5), sharex=False)
    occ = top["times"]
    axes[0].eventplot([occ], lineoffsets=0.5, linelengths=0.7, color="darkorange")
    axes[0].set_yticks([])
    axes[0].set_title(f"3) occurrences of zone {top['price']:.4f} "
                      f"({top['count']} hits, {top['n_visits']} visits, "
                      f"{top['duration_h']:.1f}h lifespan)")
    axes[0].set_xlabel("time (ms)")
    gaps_min = np.diff(np.sort(occ)) / 60_000
    nontrivial = gaps_min[gaps_min > 1.0]   # drop trivial consecutive-bar gaps
    axes[1].hist(nontrivial if len(nontrivial) else [0], bins=30)
    axes[1].set_xlabel("Δtime between occurrences > 1 min (min, log scale)")
    axes[1].set_xscale("log")
    axes[1].set_title(f"Δtime gap distribution "
                      f"({len(gaps_min) - len(nontrivial)} trivial 1-min gaps omitted)")
    fig.tight_layout()
    fig.savefig(f"{outdir}/3_time_structure.png", dpi=110)
    plt.close(fig)

    # 4) volume around touches
    fig, ax = plt.subplots(figsize=(9, 4))
    offs = [-30, -15, -5, -1, 0, 1, 5, 15, 30, 60]
    prof = []
    v, tms = bar["volume"].to_numpy(), bar["ts_ms"].to_numpy()
    for off in offs:
        vals = []
        for t0 in res["touch_ts"]:
            target = int(t0.timestamp() * 1000) + off * TF_MS
            j = int(np.searchsorted(tms, target, side="right")) - 1
            # bar j must actually cover the target (no clamping past data end)
            if j >= 0 and tms[j] >= target - TF_MS:
                vals.append(v[j])
        prof.append(np.nanmean(vals) if vals else np.nan)
    ax.plot(offs, prof, marker="o")
    ax.axvline(0, color="grey", ls=":")
    ax.set_xlabel("minutes relative to touch")
    ax.set_ylabel("avg volume")
    ax.set_title("4) average volume around zone touches")
    fig.savefig(f"{outdir}/4_volume.png", dpi=110)
    plt.close(fig)

    # 5) forward-return distribution
    fig, axes = plt.subplots(len(horizons), 1, figsize=(9, 3 * len(horizons)),
                             squeeze=False)
    for ax, h in zip(axes[:, 0], horizons):
        vals = res[f"ret_{h}s"].dropna()
        if len(vals):
            ax.hist(vals, bins=40, alpha=0.7)
            ax.axvline(0, color="k", lw=0.8)
            ax.set_title(f"forward return {h // 60}m after touch (%)")
    fig.savefig(f"{outdir}/5_forward_returns.png", dpi=110)
    plt.close(fig)
    print(f"[plots] 5 figures -> {outdir}/")


# ------------------------------------------------------------------ main
def parse_args(argv=None):
    p = argparse.ArgumentParser(description="XRP price-time clustering levels")
    p.add_argument("--csv", help="OHLCV csv timestamp,open,high,low,close,volume")
    p.add_argument("--days", type=int, default=5)
    p.add_argument("--seed", type=int, default=11)
    p.add_argument("--eps-scale", type=float, default=0.5, dest="eps_scale",
                   help="price bin width = scale * median bar range (adaptive)")
    p.add_argument("--density-mult", type=float, default=3.0, dest="density_mult",
                   help="dense bin = count >= mult * median bin count")
    p.add_argument("--min-count", type=int, default=10, dest="min_count")
    p.add_argument("--min-duration-h", type=float, default=3.0, dest="min_duration_h")
    p.add_argument("--min-visits", type=int, default=2, dest="min_visits")
    p.add_argument("--visit-gap-min", type=int, default=60, dest="visit_gap_min",
                   help="gap splitting distinct visits (minutes)")
    p.add_argument("--max-levels", type=int, default=30, dest="max_levels")
    p.add_argument("--ctx-s", type=int, default=600, dest="ctx_s",
                   help="context window seconds before touch (default 10m)")
    p.add_argument("--max-horizon", type=int, default=3600, dest="max_horizon")
    p.add_argument("--horizons", type=int, nargs="+", default=[60, 300, 600, 1800, 3600])
    p.add_argument("--top", type=int, default=15)
    p.add_argument("--outdir", default=os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                                    "levels_cluster_out"))
    return p.parse_args(argv)


def main(argv=None) -> int:
    args = parse_args(argv)
    os.makedirs(args.outdir, exist_ok=True)

    # 1) data via simccxt
    bar, origin = load_bars(args)

    # adaptive scale: half of the median bar range (NOT a fixed %)
    med_range = float(np.median((bar["high"] - bar["low"]).to_numpy()))
    bin_width = args.eps_scale * med_range
    print(f"[scale] median bar range={med_range:.6f} -> adaptive bin width={bin_width:.6f}")

    # 2-6) all prices -> sort -> dense bands -> time structure -> levels
    levels = detect_levels(bar, bin_width, args.density_mult,
                           args.visit_gap_min * 60_000,
                           args.min_count, args.min_duration_h,
                           args.min_visits, args.max_levels)
    if levels.empty:
        print("no zones passed the repetition filters — relax --min-count/--min-visits")
        return 1

    # 7) interactions + outcomes (causal part)
    frames = [w for _, lv in levels.iterrows()
              if len(w := walk_zone(bar, lv, args.ctx_s, args.horizons,
                                    args.max_horizon))]
    res = pd.concat(frames, ignore_index=True) if frames else pd.DataFrame()
    print(f"[research] {len(res):,} touches")

    # 8) results table
    summary = summarize(res, args.horizons)
    meta = levels.set_index("price")[["lo", "hi", "count", "n_visits", "duration_h",
                                      "median_dtime_min", "median_visit_gap_min",
                                      "median_dprice"]]
    table = summary.set_index("level").join(meta, how="left")
    order = ["touches", "from_above", "from_below", "avg_trades", "avg_volume",
             "avg_oi_change", "rejection_rate", "breakout_rate"] + \
            [f"return_{h}s" for h in args.horizons]
    print("\n" + "=" * 120)
    print("LEVELS = price zones with repetition structure "
          "(level | touches | zone | count | visits | lifespan | Δtime | outcomes)")
    print("=" * 120)
    show = table.reset_index().head(args.top)
    print(show[["level", "touches", "from_above", "from_below", "count",
                "n_visits", "duration_h", "median_visit_gap_min", "median_dprice",
                "rejection_rate", "breakout_rate"] +
               [f"return_{h}s" for h in args.horizons]].to_string(index=False))
    print("-" * 120)
    print("avg_trades / avg_oi_change are NaN: unavailable in the simccxt Python API.")
    print("=" * 120)
    res.to_csv(f"{args.outdir}/touches.csv", index=False)
    table.to_csv(f"{args.outdir}/levels_summary.csv")

    plot_all(bar, levels, res, args.outdir, args.horizons)
    print(f"\nDone ({origin}).")
    return 0


if __name__ == "__main__":
    sys.exit(main())