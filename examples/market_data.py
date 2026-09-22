#!/usr/bin/env python3
"""
market_data.py
=============
Disk cache + internet download for the simccxt exchange simulator (Binance public klines).

Legacy research helper (optional Python bindings only). The main app
(Rust server + TypeScript terminal) loads CSV caches directly and needs
no Python at all.

Logic ("if the coin is not in the laptop's memory, fetch it from internet"):
  cache:  data/ohlcv/{SYMBOL}_{tf}.csv   (timestamp ms, open, high, low, close, volume)
  1. read the cache file if it exists;
  2. compute which bars of [start, end) are missing (bar-grid aligned);
  3. download ONLY the missing segments from Binance REST klines (paginated);
  4. merge + dedupe + sort, write the cache back, return the window.

No API key needed (public klines endpoint). Stdlib only (urllib + csv).
Rows are plain lists: [ts_ms, open, high, low, close, volume].
"""

import csv
import json
import os
import socket
import time
import urllib.error
import urllib.request

TF_MS = {"1m": 60_000, "5m": 300_000, "15m": 900_000, "1h": 3_600_000}
COLUMNS = ["ts_ms", "open", "high", "low", "close", "volume"]
CACHE_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                         "..", "data", "ohlcv")

# Public klines endpoints, tried in order. api.binance.com is geo-restricted
# in some regions (HTTP 451) — data-api.binance.vision serves the same
# public market data without those restrictions.
BINANCE_HOSTS = ["https://api.binance.com", "https://data-api.binance.vision"]


def to_binance_symbol(symbol: str) -> str:
    """"BTC/USDT" -> "BTCUSDT" (Binance spot)."""
    base, _, quote = symbol.partition("/")
    if not base or not quote:
        raise ValueError(f"bad symbol {symbol!r}, expected BASE/QUOTE")
    return (base + quote).upper()


def cache_path(symbol: str, tf: str, cache_dir: str = CACHE_DIR) -> str:
    return os.path.join(cache_dir, f"{to_binance_symbol(symbol)}_{tf}.csv")


def _fetch_page(url: str, timeout: float) -> list:
    """One klines page; raises the last error to the caller."""
    req = urllib.request.Request(url, headers={"User-Agent": "simccxt/0.1"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode())


def fetch_klines(symbol: str, tf: str, start_ms: int, end_ms: int,
                 limit: int = 1000, pause: float = 0.15,
                 timeout: float = 30) -> list:
    """Download klines [start_ms, end_ms] from Binance (paginated).

    Tries BINANCE_HOSTS in order: a host answering 403/451 (geo-restriction)
    or unreachable is skipped in favour of the next one.
    """
    if tf not in TF_MS:
        raise ValueError(f"unsupported timeframe {tf!r} (need one of {sorted(TF_MS)})")
    sym = to_binance_symbol(symbol)
    step = TF_MS[tf]
    out, cursor = [], int(start_ms)
    last_err: Exception | None = None
    while cursor <= int(end_ms):
        rows = None
        for host in BINANCE_HOSTS:
            url = (f"{host}/api/v3/klines?symbol={sym}&interval={tf}"
                   f"&startTime={cursor}&endTime={int(end_ms)}&limit={limit}")
            try:
                rows = _fetch_page(url, timeout)
                last_err = None
                break
            except urllib.error.HTTPError as exc:
                last_err = exc
                if exc.code in (403, 451) and host != BINANCE_HOSTS[-1]:
                    continue  # restricted here — try the mirror
                raise
            except (urllib.error.URLError, socket.timeout, TimeoutError) as exc:
                last_err = exc
                if host != BINANCE_HOSTS[-1]:
                    continue  # unreachable — try the mirror
                raise
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
    if last_err is not None and not out:
        raise last_err
    return out


def read_cache(symbol: str, tf: str, cache_dir: str = CACHE_DIR) -> list:
    """Cached bars as [[ts_ms, o, h, l, c, v], ...], sorted by ts (may be empty)."""
    path = cache_path(symbol, tf, cache_dir)
    if not os.path.exists(path):
        return []
    with open(path, newline="") as f:
        rdr = csv.DictReader(f)
        rows = []
        for rec in rdr:
            try:
                rows.append([int(float(rec["ts_ms"])), float(rec["open"]),
                             float(rec["high"]), float(rec["low"]),
                             float(rec["close"]), float(rec["volume"])])
            except (KeyError, TypeError, ValueError):
                continue
    rows.sort(key=lambda r: r[0])
    return rows


def write_cache(rows: list, symbol: str, tf: str,
                cache_dir: str = CACHE_DIR) -> str:
    os.makedirs(cache_dir, exist_ok=True)
    path = cache_path(symbol, tf, cache_dir)
    rows = sorted(rows, key=lambda r: r[0])
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(COLUMNS)
        w.writerows(rows)
    return path


def read_window(symbol: str, tf: str, start_ms: int, end_ms: int,
                cache_dir: str = CACHE_DIR) -> tuple:
    """Cached bars for [start_ms, end_ms) without any network.

    Returns (window_rows, covered) where covered=True means the disk cache
    already holds every bar of the requested range.
    """
    cached = read_cache(symbol, tf, cache_dir)
    window = [r for r in cached if start_ms <= r[0] < end_ms]
    step = TF_MS[tf]
    want = set(range(int(start_ms), int(end_ms), step))
    have = {r[0] for r in window}
    return window, (bool(want) and want <= have)


def load_or_download(symbol: str, tf: str, start_ms: int, end_ms: int,
                     cache_dir: str = CACHE_DIR,
                     timeout: float = 30) -> tuple:
    """Return bars for [start_ms, end_ms); download only what's missing.

    Returns (window_rows, stats) with stats = {cached, downloaded, source}.
    """
    step = TF_MS[tf]
    want = list(range(int(start_ms), int(end_ms), int(step)))
    cached = read_cache(symbol, tf, cache_dir)
    have = {r[0] for r in cached}
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
            downloaded.extend(fetch_klines(symbol, tf, a, b, timeout=timeout))

    if downloaded:
        merged = {r[0]: r for r in cached}
        for r in downloaded:
            merged[r[0]] = r
        full = sorted(merged.values(), key=lambda r: r[0])
        write_cache(full, symbol, tf, cache_dir)
    else:
        full = cached

    window = [r for r in full if start_ms <= r[0] < end_ms]
    stats = {"cached": len(cached), "downloaded": len(downloaded),
             "source": "binance" if (len(cached) or len(downloaded)) else "empty"}
    return window, stats
