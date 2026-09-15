#!/usr/bin/env python3
"""
zones.py
========
Accumulation zones + strong levels from detected swing levels.

Idea (heuristic, all raw numbers are exported so you can test it yourself):
  * sort levels by price, merge neighbours whose gap <= ``max_gap_pct``
    (single linkage) into a cluster;
  * a cluster is an *accumulation zone* when it holds >= ``min_levels``
    levels AND >= ``min_touches`` touches AND spans <= ``max_width_pct``
    AND price dwelled inside >= ``min_dwell_bars``;
  * a level far from any neighbour (gap >= ``strong_gap_pct`` on the
    nearer side) with >= ``strong_min_touches`` touches is a *strong
    level* (lonely + respected).

What you asked for, plus the bits that make it usable:
  * time bounds: ``t0`` = first touch, ``t1`` = last touch of the cluster
    (a rectangle needs time, not just price);
  * ``dwell_bars`` — how long price sat inside (pass-through is not
    accumulation);
  * ``vol_ratio`` — per-bar volume inside the zone vs the baseline
    (accumulation backed by volume is a different animal);
  * ``status`` — "активна" (recent touch / price still inside),
    "пробой вверх/вниз" (price left the zone by ``break_pct`` after ``t1``),
    else "стоит";
  * overlapping clusters are merged through single linkage, so one
    sideways market gives one zone, not five stacked rectangles.

Pure numpy/pandas-free python (numpy only); no simccxt/dash imports,
so research scripts can reuse it too.
"""

import numpy as np

DEFAULTS = dict(
    max_gap_pct=0.003,       # neighbours merge into one cluster
    min_levels=3,            # at least this many levels in a zone
    min_touches=5,           # at least this many touches in a zone
    max_width_pct=0.02,      # zone wider than this is not accumulation
    min_dwell_bars=30,       # price must sit inside this long
    strong_gap_pct=0.005,    # isolation radius for a strong level
    strong_min_touches=5,    # touches needed for a strong level
    break_pct=0.002,         # excursion after t1 counted as breakout
    active_gap_bars=30,      # last touch within this => zone is active
)


def _split_wide(cluster: list, max_width_pct: float) -> list:
    """Recursively split a cluster at its largest internal gap until every
    piece spans <= max_width_pct (cures single-linkage chaining across a
    whole sideways market)."""
    if len(cluster) < 2:
        return [cluster]
    ordered = sorted(cluster, key=lambda l: l["price"])
    span = (ordered[-1]["price"] - ordered[0]["price"]) / ordered[0]["price"]
    if span <= max_width_pct:
        return [ordered]
    gaps = [(ordered[i + 1]["price"] - ordered[i]["price"], i)
            for i in range(len(ordered) - 1)]
    _, at = max(gaps)
    return (_split_wide(ordered[:at + 1], max_width_pct)
            + _split_wide(ordered[at + 1:], max_width_pct))


def cluster_levels(levels: list, max_gap_pct: float) -> list:
    """Single-linkage clusters of level dicts (each has ``price``)."""
    if not levels:
        return []
    ordered = sorted(levels, key=lambda l: l["price"])
    clusters, cur = [], [ordered[0]]
    for lv in ordered[1:]:
        gap = (lv["price"] - cur[-1]["price"]) / cur[-1]["price"]
        if gap <= max_gap_pct:
            cur.append(lv)
        else:
            clusters.append(cur)
            cur = [lv]
    clusters.append(cur)
    return clusters


def _as_array(bars) -> np.ndarray:
    a = np.asarray(bars, dtype=float)
    if a.ndim != 2 or a.shape[1] < 6:
        raise ValueError("bars must be [ts,o,h,l,c,v] rows")
    return a


def find_zones(levels: list, bars, step_ms: int,
               cfg: dict | None = None) -> list:
    """Accumulation zones from levels + full bar history.

    ``levels``: detector levels (``price``, ``touches`` with ``ts``).
    ``bars``: [ts,o,h,l,c,v] rows (list or np array), ascending ts.
    Returns zone dicts sorted by touches desc. Each has:
    y0, y1, mid, width_pct, t0, t1, n_levels, n_touches, dwell_bars,
    vol_ratio, status ("активна" | "пробой вверх" | "пробой вниз" | "стоит").
    """
    c = dict(DEFAULTS)
    if cfg:
        c.update(cfg)
    if not levels:
        return []
    arr = _as_array(bars)
    if not len(arr):
        return []
    ts, closes, vols = arr[:, 0].astype(np.int64), arr[:, 4], arr[:, 5]
    base_vol = float(vols.mean()) if float(vols.mean()) > 0 else 1.0
    step = int(step_ms) if step_ms else 60_000
    now = int(ts[-1])
    zones = []
    clusters = []
    for cl in cluster_levels(levels, c["max_gap_pct"]):
        clusters.extend(_split_wide(cl, c["max_width_pct"]))
    for cl in clusters:
        touch_ts = [t["ts"] for lv in cl for t in lv.get("touches", [])]
        if len(cl) < c["min_levels"] or len(touch_ts) < c["min_touches"]:
            continue
        y0 = min(lv["price"] for lv in cl)
        y1 = max(lv["price"] for lv in cl)
        mid = (y0 + y1) / 2.0
        width_pct = (y1 - y0) / mid if mid else 0.0
        if width_pct > c["max_width_pct"]:
            continue
        t0, t1 = int(min(touch_ts)), int(max(touch_ts))
        # A level the price sits inside without leaving produces few
        # re-entries, so widen the bounds to when the cluster's levels
        # became known: accumulation is also "price lived here".
        born = [int(lv["confirmed_ts"]) for lv in cl
                if lv.get("confirmed_ts") is not None]
        if born:
            t0 = min(t0, min(born))
            t1 = max(t1, max(born))
        m = (ts >= t0) & (ts <= t1)
        dwell = int(m.sum())
        if dwell < c["min_dwell_bars"]:
            continue
        inside = m & (closes >= y0) & (closes <= y1)
        vol_ratio = (float(vols[inside].mean()) / base_vol
                     if inside.any() else 0.0)
        post = closes[ts > t1]
        up = bool(len(post) and post.max() > y1 * (1 + c["break_pct"]))
        dn = bool(len(post) and post.min() < y0 * (1 - c["break_pct"]))
        if up and not dn:
            status = "пробой вверх"
        elif dn and not up:
            status = "пробой вниз"
        elif (now - t1) <= c["active_gap_bars"] * step:
            status = "активна"
        else:
            status = "стоит"
        zones.append({
            "y0": y0, "y1": y1, "mid": mid, "width_pct": width_pct,
            "t0": t0, "t1": t1, "n_levels": len(cl),
            "n_touches": len(touch_ts), "dwell_bars": dwell,
            "vol_ratio": vol_ratio, "status": status,
        })
    zones.sort(key=lambda z: z["n_touches"], reverse=True)
    return zones


def find_strong_levels(levels: list, cfg: dict | None = None) -> set:
    """Prices of strong levels: isolated (big gap to the nearest neighbour
    on the closer side) AND repeatedly touched."""
    c = dict(DEFAULTS)
    if cfg:
        c.update(cfg)
    if not levels:
        return set()
    prices = sorted(lv["price"] for lv in levels)
    strong = set()
    for lv in levels:
        p = lv["price"]
        gaps = []
        if p > prices[0]:
            below = max(q for q in prices if q < p)
            gaps.append((p - below) / p)
        if p < prices[-1]:
            above = min(q for q in prices if q > p)
            gaps.append((above - p) / p)
        near = min(gaps) if gaps else float("inf")
        if near >= c["strong_gap_pct"] \
                and len(lv.get("touches", [])) >= c["strong_min_touches"]:
            strong.add(p)
    return strong
