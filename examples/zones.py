#!/usr/bin/env python3
"""
zones.py
========
Accumulation zones + strong levels from detected swing levels.

Idea (heuristic, all raw numbers are exported so you can test it yourself):
  * sort levels by price, merge neighbours whose gap <= ``max_gap_pct``
    (single linkage) into a cluster; split wide clusters at the largest
    internal gap so one sideways market gives compact bands;
  * a cluster is an *accumulation zone* when it holds >= ``min_levels``
    levels AND >= ``min_touches`` touches AND spans <= ``max_width_pct``;
  * STRICT containment: every bar of the zone's [t0, t1] sits fully
    inside [y0, y1]. The first bar that leaves the band CLOSES the zone;
    the scan continues, so one cluster can yield several zones over time;
  * quality = bars inside (``dwell_bars``): zones sort by it desc;
  * a level far from any neighbour (gap >= ``strong_gap_pct`` on the
    nearer side) with >= ``strong_min_touches`` touches is a *strong
    level* (lonely + respected).

What you asked for, plus the bits that make it usable:
  * time bounds: ``t0`` = first touch, ``t1`` = last touch of the cluster
    (a rectangle needs time, not just price);
  * ``dwell_bars`` — every bar inside (by construction); more is better;
  * ``vol_ratio`` — per-bar volume inside the zone vs the baseline
    (accumulation backed by volume is a different animal);
  * ``status`` — "активна" (never left the band through the last bar),
    otherwise which side broke it ("пробой вверх/вниз/в обе стороны");
  * overlapping clusters are merged through single linkage, then split
    wide ones, so one sideways market gives compact non-overlapping zones.

Pure numpy/pandas-free python (numpy only); no simccxt/dash imports,
so research scripts can reuse it too.
"""

import numpy as np

DEFAULTS = dict(
    max_gap_pct=0.003,       # neighbours merge into one cluster
    min_levels=3,            # at least this many levels in a zone
    min_touches=5,           # at least this many touches in a zone segment
    max_width_pct=0.02,      # zone wider than this is not accumulation
    min_dwell_bars=30,       # at least this many bars inside the zone
    pad_pct=0.0,             # band tolerance (0 = a wick out closes the zone)
    strong_gap_pct=0.005,    # isolation radius for a strong level
    strong_min_touches=5,    # touches needed for a strong level
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

    Strict rules:
    1. EVERY bar of the zone's [t0, t1] sits fully inside [y0, y1]
       (low >= y0 and high <= y1) — the band comes from a cluster of
       close levels, the time span from its touches.
    2. The first bar that leaves the band CLOSES the zone (t1 = the last
       bar still inside); the scan then continues, so one cluster can
       produce several zones over time.
    3. Quality = bars inside: zones sort by ``dwell_bars`` desc.

    ``levels``: detector levels (``price``, ``touches`` with ``ts``,
    ``confirmed_ts``).
    ``bars``: [ts,o,h,l,c,v] rows (list or np array), ascending ts.
    Returns zone dicts sorted by dwell_bars desc. Each has:
    y0, y1, mid, width_pct, t0, t1, n_levels, n_touches, dwell_bars,
    vol_ratio, status ("активна" | "пробой вверх" | "пробой вниз" |
    "пробой в обе стороны").
    """
    c = dict(DEFAULTS)
    if cfg:
        c.update(cfg)
    if not levels:
        return []
    arr = _as_array(bars)
    if not len(arr):
        return []
    import bisect
    ts = arr[:, 0].astype(np.int64)
    highs, lows, vols = arr[:, 2], arr[:, 3], arr[:, 5]
    base_vol = float(vols.mean()) if float(vols.mean()) > 0 else 1.0
    zones = []
    clusters = []
    for cl in cluster_levels(levels, c["max_gap_pct"]):
        clusters.extend(_split_wide(cl, c["max_width_pct"]))
    for cl in clusters:
        if len(cl) < c["min_levels"]:
            continue
        y0 = min(lv["price"] for lv in cl)
        y1 = max(lv["price"] for lv in cl)
        mid = (y0 + y1) / 2.0
        width_pct = (y1 - y0) / mid if mid else 0.0
        if width_pct > c["max_width_pct"]:
            continue
        pad = c.get("pad_pct", 0.0)
        lo, hi = y0 * (1 - pad), y1 * (1 + pad)
        touch_ts = sorted(t["ts"] for lv in cl for t in lv.get("touches", []))
        if not touch_ts:
            continue
        ti = 0
        while ti < len(touch_ts):
            i0 = int(np.searchsorted(ts, touch_ts[ti]))
            if i0 >= len(ts):
                break
            j = i0
            while j < len(ts) and lows[j] >= lo and highs[j] <= hi:
                j += 1
            i1 = j - 1  # last bar still inside (touch bar itself may wick out)
            if i1 < i0:
                ti += 1
                continue
            t0, t1 = int(ts[i0]), int(ts[i1])
            dwell = i1 - i0 + 1
            seg_touches = [t for t in touch_ts if t0 <= t <= t1]
            ti = bisect.bisect_right(touch_ts, t1, lo=ti)
            if dwell < c["min_dwell_bars"] \
                    or len(seg_touches) < c["min_touches"]:
                continue
            known = [lv for lv in cl
                     if lv.get("confirmed_ts") is None
                     or int(lv["confirmed_ts"]) <= t1]
            if len(known) < c["min_levels"]:
                continue
            vol_ratio = float(vols[i0:i1 + 1].mean()) / base_vol
            if j >= len(ts):
                status = "активна"  # never left the band through `now`
            else:
                up = bool(highs[j] > hi)
                dn = bool(lows[j] < lo)
                status = ("пробой в обе стороны" if (up and dn)
                          else "пробой вверх" if up else "пробой вниз")
            zones.append({
                "y0": y0, "y1": y1, "mid": mid, "width_pct": width_pct,
                "t0": t0, "t1": t1, "n_levels": len(known),
                "n_touches": len(seg_touches), "dwell_bars": dwell,
                "vol_ratio": vol_ratio, "status": status,
            })
    zones.sort(key=lambda z: z["dwell_bars"], reverse=True)
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
