#!/usr/bin/env python3
"""
zones.py
========
Accumulation zones + strong levels.

Strict rules (your three, literally):
1. EVERY bar of a zone sits fully inside its band [y0, y1].
2. The first bar that leaves the band CLOSES the zone; the scan
   continues, so one area can yield several zones over time.
3. Quality = bars inside (``dwell_bars``): zones sort by it desc.

How a zone is born (bar-first, so wicks can't silently kill it before
it exists — the band is the seed window's own extremes, hence the first
``seed_bars`` bars always fit):
  * slide a ``seed_bars`` window; where its span <= ``max_width_pct``,
    fix the band [min low, max high] and extend forward while bars stay
    inside; the first outside bar closes the zone (its side = breakout);
  * then confirm with levels: the band must hold >= ``min_levels``
    (known by the zone end) AND >= ``min_touches`` touches inside
    [t0, t1] — your "many close levels + many touches" condition.

Plus, for usability:
  * ``vol_ratio`` — per-bar volume inside vs the baseline;
  * ``status`` — "активна" (reached the last bar) or breakout side;
  * strong levels (separate function): isolated (>= ``strong_gap_pct``
    to the nearer neighbour) AND touched >= ``strong_min_touches``.

Pure numpy; no simccxt/dash imports, reusable from research scripts.
"""

import bisect

import numpy as np

DEFAULTS = dict(
    seed_bars=30,            # seed window length (zone dwell is always >= this)
    max_width_pct=0.02,      # seed span wider than this is not accumulation
    min_levels=2,            # levels (known by zone end) inside the band
    min_touches=3,           # touches inside [t0, t1]
    pad_pct=0.0,             # band tolerance (0 = a wick out closes the zone)
    merge_gap_bars=10,       # merge zones this close in time (if bands overlap)
    merge_iou=0.5,           # min band overlap to merge adjacent zones
    strong_gap_pct=0.005,    # isolation radius for a strong level
    strong_min_touches=5,    # touches needed for a strong level
)


def _as_array(bars) -> np.ndarray:
    a = np.asarray(bars, dtype=float)
    if a.ndim != 2 or a.shape[1] < 6:
        raise ValueError("bars must be [ts,o,h,l,c,v] rows")
    return a


def find_zones(levels: list, bars, step_ms: int,
               cfg: dict | None = None) -> list:
    """Accumulation zones, sorted by ``dwell_bars`` desc.

    ``levels``: detector levels (``price``, ``touches`` with ``ts``,
    ``confirmed_ts``). ``bars``: [ts,o,h,l,c,v] rows, ascending ts.
    ``step_ms`` kept for API compatibility (unused).
    Zone dict: y0, y1, mid, width_pct, t0, t1, n_levels, n_touches,
    dwell_bars, vol_ratio, status.
    """
    c = dict(DEFAULTS)
    if cfg:
        c.update(cfg)
    arr = _as_array(bars)
    n = len(arr)
    W = int(c["seed_bars"])
    if n < W or W < 2:
        return []
    ts = arr[:, 0].astype(np.int64)
    highs, lows, vols = arr[:, 2], arr[:, 3], arr[:, 5]
    base_vol = float(vols.mean()) if float(vols.mean()) > 0 else 1.0

    # rolling seed extremes (vectorised, no per-bar python loop)
    shape, strides = (n - W + 1, W), (lows.strides[0], lows.strides[0])
    wl = np.lib.stride_tricks.as_strided(lows, shape, strides)
    wh = np.lib.stride_tricks.as_strided(highs, shape, strides)
    seed_lo, seed_hi = wl.min(axis=1), wh.max(axis=1)
    mid = (seed_lo + seed_hi) / 2.0
    ok = ((seed_hi - seed_lo) / np.where(mid != 0, mid, 1.0)
          <= c["max_width_pct"])
    starts = np.where(ok)[0]
    if not len(starts):
        return []

    # level / touch indexes for the confirmation filters
    lp = np.array([float(lv["price"]) for lv in levels]) if levels else np.array([])
    lc = np.array([float("-inf") if lv.get("confirmed_ts") is None
                   else int(lv["confirmed_ts"]) for lv in levels]) \
        if levels else np.array([])
    touch_ts = sorted(t["ts"] for lv in (levels or [])
                      for t in lv.get("touches", []))

    pad = c.get("pad_pct", 0.0)
    zones = []
    starts_list = starts.tolist()
    p, cursor = 0, 0
    while True:
        p = bisect.bisect_left(starts_list, cursor, lo=p)
        if p >= len(starts_list):
            break
        i = int(starts_list[p])
        lo, hi = float(seed_lo[i]) * (1 - pad), float(seed_hi[i]) * (1 + pad)
        # first bar outside, scanned in chunks so sequential zones cost
        # O(span) each instead of O(suffix) (matters at 100k+ bars)
        CH, k, bi = 4096, i, None
        while k < n:
            e = min(k + CH, n)
            bad = (lows[k:e] < lo) | (highs[k:e] > hi)
            if bad.any():
                bi = k + int(np.argmax(bad))
                break
            k = e
        if bi is None:
            j = n - 1
            status = "активна"
        else:
            j = bi - 1                    # last bar inside
            up = bool(highs[bi] > hi)
            dn = bool(lows[bi] < lo)
            status = ("пробой в обе стороны" if (up and dn)
                      else "пробой вверх" if up else "пробой вниз")
        t0, t1 = int(ts[i]), int(ts[j])
        dwell = j - i + 1
        cursor = j + 1
        if dwell < W:  # cannot happen (seed fits by construction); guard
            continue
        n_lv = int((((lp >= lo) & (lp <= hi) & (lc <= t1)).sum())
                   if len(lp) else 0)
        a = bisect.bisect_left(touch_ts, t0)
        b = bisect.bisect_right(touch_ts, t1, lo=a)
        n_tch = b - a
        if n_lv < c["min_levels"] or n_tch < c["min_touches"]:
            continue
        zones.append({
            "y0": float(seed_lo[i]), "y1": float(seed_hi[i]),
            "mid": float((seed_lo[i] + seed_hi[i]) / 2.0),
            "width_pct": float((seed_hi[i] - seed_lo[i]) / mid[i])
            if mid[i] else 0.0,
            "t0": t0, "t1": t1, "n_levels": n_lv,
            "n_touches": n_tch, "dwell_bars": int(dwell),
            "vol_ratio": float(vols[i:j + 1].mean()) / base_vol,
            "status": status,
        })
    zones = _merge_chains(zones, ts, lows, highs, vols, base_vol, touch_ts,
                          lp, lc, c, step_ms)
    zones.sort(key=lambda z: z["dwell_bars"], reverse=True)
    return zones


def _merge_chains(zones: list, ts, lows, highs, vols, base_vol, touch_ts,
                  lp, lc, c: dict, step_ms: int) -> list:
    """Glue a chain of adjacent zones with near-identical bands into one
    sustained zone (wick-breakouts fragment sideways into tiles otherwise).

    Merged only when: the time gap is small, the price bands overlap a lot,
    the merged band still respects ``max_width_pct``, and EVERY bar of the
    merged span fits inside (strict containment re-verified, never assumed).
    """
    if len(zones) < 2:
        return zones
    step = int(step_ms) if step_ms else 60_000
    max_gap_ms = int(c.get("merge_gap_bars", 10)) * step
    min_iou = float(c.get("merge_iou", 0.5))
    zones = sorted(zones, key=lambda z: z["t0"])
    out = [zones[0]]
    for z in zones[1:]:
        a = out[-1]
        inter = min(a["y1"], z["y1"]) - max(a["y0"], z["y0"])
        union = max(a["y1"], z["y1"]) - min(a["y0"], z["y0"])
        iou = (inter / union) if union > 0 else 0.0
        if z["t0"] - a["t1"] <= max_gap_ms and iou >= min_iou:
            y0, y1 = min(a["y0"], z["y0"]), max(a["y1"], z["y1"])
            mid = (y0 + y1) / 2.0
            if mid and (y1 - y0) / mid <= c["max_width_pct"]:
                m = (ts >= a["t0"]) & (ts <= z["t1"])
                seg_lows, seg_highs = lows[m], highs[m]
                if len(seg_lows) and (seg_lows >= y0).all() \
                        and (seg_highs <= y1).all():
                    i0 = int(np.searchsorted(ts, a["t0"]))
                    i1 = int(np.searchsorted(ts, z["t1"], side="right")) - 1
                    ta = bisect.bisect_left(touch_ts, a["t0"])
                    tb = bisect.bisect_right(touch_ts, z["t1"], lo=ta)
                    a.update({
                        "y0": y0, "y1": y1, "mid": mid,
                        "width_pct": (y1 - y0) / mid,
                        "t1": z["t1"],
                        "n_levels": int((((lp >= y0) & (lp <= y1)
                                          & (lc <= z["t1"])).sum())
                                        if len(lp) else 0),
                        "n_touches": tb - ta,
                        "dwell_bars": i1 - i0 + 1,
                        "vol_ratio": float(vols[i0:i1 + 1].mean()) / base_vol,
                        "status": z["status"],
                    })
                    continue
        out.append(z)
    # merged zones can chain further (a+b merged, then +c): one more pass
    # suffices in practice since bands only widen slightly per merge
    return out


def find_strong_levels(levels: list, cfg: dict | None = None) -> set:
    """Prices of strong levels: isolated (big gap to the nearest neighbour
    on the closer side) AND repeatedly touched."""
    c = dict(DEFAULTS)
    if cfg:
        c.update(cfg)
    if not levels:
        return set()
    order = sorted(range(len(levels)), key=lambda k: levels[k]["price"])
    strong = set()
    for pos, k in enumerate(order):
        p = levels[k]["price"]
        gaps = []
        if pos > 0:
            gaps.append((p - levels[order[pos - 1]]["price"]) / p)
        if pos < len(order) - 1:
            gaps.append((levels[order[pos + 1]]["price"] - p) / p)
        near = min(gaps) if gaps else float("inf")
        if near >= c["strong_gap_pct"] \
                and len(levels[k].get("touches", [])) >= c["strong_min_touches"]:
            strong.add(p)
    return strong
