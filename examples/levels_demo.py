#!/usr/bin/env python3
"""
levels_demo.py
=============
Support/resistance research driven by the simccxt *live* simulator feed.

Unlike `research/research_levels.py` (bulk analysis), this example consumes
data the way a live exchange would deliver it — bar by bar through
`fetch_ohlcv_live()` while the simulator clock advances with `step()`:

    load_candles_from_csv  (data from DISK)
    reset / set_step_ms / resume
    loop:
        bars = fetch_ohlcv_live(symbol, tf)   # bars with timestamp <= now
        detect NEW swing pivots only from seen bars (causal, no look-ahead)
        log touches of already-detected levels as they happen (live)
        step(1)

At the end it prints:
  * levels table: direction, price, pivot/confirmed timestamps, touches,
    avg volume, rejection/breakout rate, forward returns
  * confirming-bars detail for the most-touched level
  * per-touch log for that level

and saves 4 PNG plots:
  1_levels_price.png   full price + support/resistance lines
  2_level_zoom.png     zoom around the top level: pivot + confirming bars + touches
  3_volume_profile.png avg volume around touches (offsets s)
  4_returns.png        forward-return distribution

Run:
    python3 examples/levels_demo.py [--days 2] [--outdir examples/levels_demo_out]
"""

import argparse
import os
import sys

import numpy as np
import pandas as pd

import simccxt

SYMBOL = "BTC/USDT"
TF = "1m"
TF_MS = 60_000


# ------------------------------------------------------------------ synthetic data
def synthetic_1m(days: int = 2, seed: int = 7) -> pd.DataFrame:
    """Deterministic 1-minute bars with realistic swing structure."""
    n = days * 24 * 60
    rng = np.random.default_rng(seed)
    t = np.arange(n) / (24 * 60.0)                     # time in days
    logp = (np.log(100_000.0) + 0.03 * t
            + 0.05 * np.sin(2 * np.pi * t / 2.0)
            + 0.012 * np.sin(2 * np.pi * t * 3.0)
            + np.cumsum(rng.normal(0.0, 0.00025, n)))
    close = np.exp(logp)
    rng2 = np.random.default_rng(seed + 1)
    half = 0.0002 + 0.0008 * np.abs(rng2.normal(0.0, 1.0, n))
    open_ = np.concatenate(([close[0]], close[:-1]))
    ts = pd.date_range("2026-03-01", periods=n, freq="min", tz="UTC")
    return pd.DataFrame({
        "timestamp": ts,
        "open": open_,
        "high": np.maximum(open_, close) * (1.0 + half),
        "low": np.minimum(open_, close) * (1.0 - half),
        "close": close,
        "volume": rng.lognormal(0.0, 0.4, n) * 2.0,
    })


# ------------------------------------------------- incremental (causal) detector
class LiveLevels:
    """Online swing pivot detector: a pivot at i is confirmed only when the
    (i+p)-th bar after it has arrived. Nothing uses future data."""

    def __init__(self, pivot: int = 3, min_swing: float = 0.004,
                 merge_tol: float = 0.0005):
        self.pivot = pivot
        self.min_swing = min_swing
        # near-duplicate levels (same direction, price within merge_tol) are
        # dropped: without this, adjacent pivots spawn overlapping levels and
        # their touch dots look like noise
        self.merge_tol = merge_tol
        self.bars: list[dict] = []              # bars seen so far (live feed)
        self.checked = 0                        # pivots confirmed up to this idx
        self.levels: list[dict] = []            # detected levels

    def feed(self, new_bars: list[list]) -> list[dict]:
        """Append just-delivered bars, confirm any newly-knowable pivots."""
        for b in new_bars[len(self.bars):]:
            self.bars.append({"ts": int(b[0]), "o": b[1], "h": b[2],
                              "l": b[3], "c": b[4], "v": b[5]})
        out = []
        p = self.pivot
        # center i is confirmable iff its full window [i-p, i+p] has arrived
        top = len(self.bars) - p - 1
        for i in range(max(self.checked + 1, p), top + 1):
            self.checked = i
            lv = self._check(i)
            if lv is not None and not self._is_duplicate(lv):
                self.levels.append(lv)
                out.append(lv)
        return out

    def _is_duplicate(self, lv: dict) -> bool:
        """True if a same-direction level already exists within tolerance."""
        tol = self.merge_tol * lv["price"]
        return any(l["direction"] == lv["direction"]
                   and abs(l["price"] - lv["price"]) <= tol
                   for l in self.levels)

    def _check(self, i: int) -> dict | None:
        p = self.pivot
        hi = np.array([b["h"] for b in self.bars[i - p:i + p + 1]])
        lo = np.array([b["l"] for b in self.bars[i - p:i + p + 1]])
        if hi[p] > hi[:p].max() and hi[p] > hi[p + 1:].max() \
                and (hi[p] - max(hi[:p].max(), hi[p + 1:].max())) >= self.min_swing * hi[p]:
            return self._mk(i, hi[p], "resistance")
        if lo[p] < lo[:p].min() and lo[p] < lo[p + 1:].min() \
                and (min(lo[:p].min(), lo[p + 1:].min()) - lo[p]) >= self.min_swing * lo[p]:
            return self._mk(i, lo[p], "support")
        return None

    def _mk(self, i: int, price: float, direction: str) -> dict:
        # confirming bars = the 2*p+1 window around the pivot that proves it;
        # the level becomes known exactly when bar i+p arrives (causal)
        frame = self.bars[i - self.pivot:i + self.pivot + 1]
        return {
            "price": float(price),
            "direction": direction,
            "pivot_idx": i,
            "pivot_ts": self.bars[i]["ts"],
            "confirmed_ts": self.bars[i + self.pivot]["ts"],
            "confirming_bars": [b["ts"] for b in frame],  # window that proves the pivot
            "touches": [],
        }


# ------------------------------------------------------------------ live runner
def run_live(days: int, seed: int, pivot: int, min_swing: float, zone: float):
    df = synthetic_1m(days=days, seed=seed)
    path = "/tmp/simccxt_demo_1m.csv"
    df.to_csv(path, index=False)

    ex = simccxt.binance()
    ex.add_market(SYMBOL, "spot")
    ex.load_candles_from_csv(SYMBOL, TF, path)          # data FROM DISK

    first_ms = int(pd.to_datetime(df["timestamp"].iloc[0], utc=True).timestamp() * 1000)
    ex.reset(first_ms)
    ex.set_step_ms(TF_MS)                               # 1 step == 1 bar
    ex.resume()

    det = LiveLevels(pivot=pivot, min_swing=min_swing)
    seen = 0
    total = len(df)
    while not ex.replay_done(SYMBOL, TF):
        bars = ex.fetch_ohlcv_live(SYMBOL, TF)          # live feed: only bars <= now
        if len(bars) > seen:
            fresh = det.feed(bars)
            # log touches of levels that were known when this bar arrived
            for k in range(seen, len(bars)):
                _log_live_touch(det, bars[k], zone,
                                prev=float(bars[k - 1][4]) if k > 0 else None)
            seen = len(bars)
        ex.step(1)

    all_bars = np.array(ex.fetch_ohlcv_live(SYMBOL, TF))  # full series after replay
    return det, all_bars


def _log_live_touch(det: LiveLevels, b: list, zone: float, prev: float | None) -> None:
    """Record a touch if this just-arrived bar enters a known level's zone
    from the approach side. (Levels known only after their confirm bar.)"""
    ts, c = int(b[0]), b[4]
    for lv in det.levels:
        if lv["confirmed_ts"] >= ts:                   # level was not known yet
            continue
        lo, hi = lv["price"] * (1 - zone), lv["price"] * (1 + zone)
        inside = lo <= c <= hi
        if not inside or prev is None:
            continue
        if lv["direction"] == "support" and prev > hi:
            lv["touches"].append({"ts": ts, "price": c,
                                  "dist_pct": (c - lv["price"]) / lv["price"] * 100.0})
        elif lv["direction"] == "resistance" and prev < lo:
            lv["touches"].append({"ts": ts, "price": c,
                                  "dist_pct": (c - lv["price"]) / lv["price"] * 100.0})


# ------------------------------------------------------------------ outcomes
def evaluate(levels: list[dict], bars: np.ndarray, horizons_s: list[int],
             break_px: float, reject_px: float) -> pd.DataFrame:
    ts = bars[:, 0].astype(np.int64)
    rows = []
    for lv in levels:
        if not lv["touches"]:
            continue
        for t in lv["touches"]:
            e = int(np.searchsorted(ts, t["ts"], side="right")) - 1
            t_end = t["ts"] + 3600 * 1000
            fwd = slice(e + 1, int(np.searchsorted(ts, t_end, side="right")))
            out = {"level": lv["price"], "direction": lv["direction"],
                   "touch_ts": t["ts"], "entry": t["price"], "dist": t["dist_pct"]}
            hi = bars[fwd, 2].max() if fwd.stop > fwd.start else np.nan
            lo = bars[fwd, 3].min() if fwd.stop > fwd.start else np.nan
            if lv["direction"] == "support":
                pen = (lv["price"] - lo) / lv["price"] if not np.isnan(lo) else np.nan
                rej = (hi - lv["price"]) / lv["price"] if not np.isnan(hi) else np.nan
            else:
                pen = (hi - lv["price"]) / lv["price"] if not np.isnan(hi) else np.nan
                rej = (lv["price"] - lo) / lv["price"] if not np.isnan(lo) else np.nan
            out["penetration"] = pen
            out["rejection"] = rej
            out["breakout"] = bool(pen >= break_px) if not np.isnan(pen) else False
            out["reject"] = (not out["breakout"]) and (not np.isnan(rej)) and rej >= reject_px
            for h in horizons_s:
                j = int(np.searchsorted(ts, t["ts"] + h * 1000, side="right")) - 1
                out[f"ret_{h}s"] = ((bars[j, 4] - t["price"]) / t["price"] * 100.0
                                    if j > e else np.nan)
            rows.append(out)
    return pd.DataFrame(rows)


def fmt_ts(ms: int) -> str:
    return pd.to_datetime(int(ms), unit="ms", utc=True).strftime("%m-%d %H:%M")


# ------------------------------------------------------------------ main
def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="simccxt live-feed levels demo")
    ap.add_argument("--days", type=int, default=2)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--pivot", type=int, default=5,
                    help="swing lookback bars each side")
    ap.add_argument("--min-swing", type=float, default=0.0005, dest="min_swing",
                    help="minimum pivot prominence (fraction, default 0.05%%)")
    ap.add_argument("--zone", type=float, default=0.001,
                    help="level zone width as a fraction (default 0.1%%)")
    ap.add_argument("--outdir", default=os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                                     "levels_demo_out"))
    args = ap.parse_args(argv)
    os.makedirs(args.outdir, exist_ok=True)

    det, bars = run_live(args.days, args.seed, pivot=args.pivot,
                         min_swing=args.min_swing, zone=args.zone)
    print(f"\nlive replay finished: {len(bars):,} bars, "
          f"{len(det.levels)} levels detected online")
    if not det.levels:
        print("no levels — try --days larger or relax min_swing")
        return 1

    res = evaluate(det.levels, bars, [60, 300, 600, 1800, 3600],
                   break_px=0.003, reject_px=0.001)

    # ---- levels table
    g = res.groupby(["level", "direction"], as_index=False) if len(res) else pd.DataFrame()
    print("\n" + "=" * 100)
    print("LEVELS (price, type, pivot/confirm, touches, volume, reject/break, ret30m/60m)")
    print("=" * 100)
    rows = []
    for lv in det.levels:
        sub = res[res["level"] == lv["price"]] if len(res) else res
        n = len(sub)
        vol = (bars[max(0, lv["pivot_idx"] - 3):lv["pivot_idx"] + 4, 5].mean()
               if lv["pivot_idx"] + 4 < len(bars) else np.nan)
        rows.append({
            "level": f"{lv['price']:,.1f}", "dir": lv["direction"][:4],
            "pivot": fmt_ts(lv["pivot_ts"]), "confirmed": fmt_ts(lv["confirmed_ts"]),
            "touches": n,
            "avg_vol": f"{vol:.2f}",
            "rej%": f"{100 * sub['reject'].mean():.1f}" if n else "-",
            "brk%": f"{100 * sub['breakout'].mean():.1f}" if n else "-",
            "ret30m%": f"{sub['ret_1800s'].mean():+.3f}" if n else "-",
            "ret60m%": f"{sub['ret_3600s'].mean():+.3f}" if n else "-",
        })
    t = pd.DataFrame(rows)
    print(t.to_string(index=False))
    res.to_csv(os.path.join(args.outdir, "touches.csv"), index=False)

    # ---- top level detail: confirming bars + touch log
    if len(res):
        top = res.groupby("level").size().idxmax()
        lv = next(x for x in det.levels if abs(x["price"] - top) < 1e-9)
        print("\n" + "-" * 100)
        print(f"TOP LEVEL {lv['price']:,.1f} ({lv['direction']}), "
              f"pivot {fmt_ts(lv['pivot_ts'])}, confirmed {fmt_ts(lv['confirmed_ts'])}")
        print(f"confirming bars (the {2 * det.pivot + 1}-bar window that proves the pivot):")
        for k, cb in enumerate(lv["confirming_bars"]):
            mark = " <-- PIVOT BAR" if k == det.pivot else ""
            print(f"   {fmt_ts(cb)}{mark}")
        print("\ntouch log (time, entry, dist%, penetration%, rejection%, outcome, ret60m%):")
        for _, r in res[res["level"] == top].iterrows():
            oc = "BREAKOUT" if r["breakout"] else ("REJECT" if r["reject"] else "hold")
            print(f"   {fmt_ts(r['touch_ts'])}  {r['entry']:>10.1f}  "
                  f"{r['dist']:>+6.3f}%  pen {100 * r['penetration']:5.2f}%  "
                  f"rej {100 * r['rejection']:5.2f}%  {oc:>8}  ret60m {r['ret_3600s']:+.3f}%")

    _plots(bars, det.levels, res, args.outdir)
    print(f"\nplots + touches.csv saved to {args.outdir}/")
    return 0


def _plots(bars: np.ndarray, levels: list[dict], res: pd.DataFrame, outdir: str) -> None:
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    ts = pd.to_datetime(bars[:, 0], unit="ms", utc=True)
    # 1) full price + levels
    fig, ax = plt.subplots(figsize=(12, 5))
    ax.plot(ts, bars[:, 4], lw=0.6, color="k", label="BTC close (1m)")
    for lv in levels:
        ax.axhline(lv["price"], color="green" if lv["direction"] == "support" else "red",
                   lw=0.8, ls="--", alpha=0.8)
    ax.set_title("1) price + detected support(green)/resistance(red) levels")
    ax.legend(fontsize=8)
    fig.savefig(f"{outdir}/1_levels_price.png", dpi=110)
    plt.close(fig)

    if len(res):
        # 2) zoom on the top level: pivot, confirming bars, touches
        top = res.groupby("level").size().idxmax()
        lv = next(x for x in levels if abs(x["price"] - top) < 1e-9)
        last_touch_ms = int(res[res["level"] == top]["touch_ts"].max())
        first_ms = int(bars[0, 0])
        i0 = max(0, lv["pivot_idx"] - 40)
        i1 = int(min(len(bars), (last_touch_ms - first_ms) // TF_MS + 16))
        seg = slice(i0, i1)
        fig, ax = plt.subplots(figsize=(12, 5))
        ax.plot(ts[seg], bars[seg, 4], lw=0.9, color="k")
        ax.axhline(lv["price"], color="orange", lw=1.2, label=f"level {lv['price']:,.0f}")
        ax.axvline(pd.to_datetime(lv["pivot_ts"], unit="ms", utc=True),
                   color="blue", ls=":", label="pivot bar")
        ax.axvline(pd.to_datetime(lv["confirmed_ts"], unit="ms", utc=True),
                   color="purple", ls="--", label="level confirmed")
        ax.axvspan(pd.to_datetime(lv["confirming_bars"][0], unit="ms", utc=True),
                   pd.to_datetime(lv["confirming_bars"][-1], unit="ms", utc=True),
                   color="blue", alpha=0.08, label="confirming window")
        tt = res[res["level"] == top]
        ax.scatter(pd.to_datetime(tt["touch_ts"], unit="ms", utc=True), tt["entry"],
                   c=["red" if b else "green" for b in tt["breakout"]],
                   s=26, zorder=5, label="touches (red=breakout)")
        ax.set_title("2) top level: pivot, confirming bars, touches (live-detected)")
        ax.legend(fontsize=8, loc="best")
        fig.savefig(f"{outdir}/2_level_zoom.png", dpi=110)
        plt.close(fig)

        # 3) volume profile around touches
        fig, ax = plt.subplots(figsize=(9, 4))
        offs = [-30, -15, -5, -1, 0, 1, 5, 15, 30, 60]
        prof = []
        v = bars[:, 5]
        tms = bars[:, 0].astype(np.int64)
        for off in offs:
            vals = []
            for t0 in res["touch_ts"].to_numpy():
                j = int(np.searchsorted(tms, int(t0) + off * 60_000, side="right")) - 1
                vals.append(v[j] if j >= 0 else np.nan)
            prof.append(np.nanmean(vals))
        ax.plot(offs, prof, marker="o")
        ax.axvline(0, color="grey", ls=":")
        ax.set_xlabel("minutes relative to touch")
        ax.set_ylabel("avg volume")
        ax.set_title("3) average volume around level touches")
        fig.savefig(f"{outdir}/3_volume_profile.png", dpi=110)
        plt.close(fig)

        # 4) forward return distribution
        fig, axes = plt.subplots(2, 1, figsize=(9, 6), squeeze=False)
        for ax, h in zip(axes[:, 0], [1800, 3600]):
            col = f"ret_{h}s"
            vals = res[col].dropna()
            if len(vals):
                ax.hist(vals, bins=40, alpha=0.7)
                ax.axvline(0, color="k", lw=0.8)
                ax.set_title(f"distribution of {h // 60}m forward return (%)")
        fig.savefig(f"{outdir}/4_forward_returns.png", dpi=110)
        plt.close(fig)


if __name__ == "__main__":
    sys.exit(main())