#!/usr/bin/env python3
"""
live_dashboard_taipy.py
=======================
Real-time updating website for simccxt level research (Taipy + Plotly).

Same behaviour as the Dash version: pick a coin + timeframe + DATE RANGE;
the simccxt simulator clock advances on every tick and the Plotly chart
updates live: new OHLC bars stream in from disk-backed data, swing levels
are detected online (no look-ahead), touches are logged.

Data path ("not in laptop memory -> fetch from internet"):
  data/ohlcv/{SYM}_{tf}.csv cache is checked first; only missing bars of the
  selected [start, end) range are downloaded from Binance public klines.
  No internet / bad range -> deterministic synthetic fallback (status line says).

  coin:       BTC/USDT, ETH/USDT, XRP/USDT, SOL/USDT  (or any BASE/QUOTE)
  timeframe:  1m / 5m / 15m / 1h  (simulator step = 1 bar)
  dates:      start/end (UTC); ranges > 200k bars are refused with a message
  speed:      simulator steps per tick (1 tick ≈ 1 second wall time)
  window:     how many recent bars to display

Run:
    python live_dashboard_taipy.py [--port 8050]
    open http://127.0.0.1:8050/
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import os
import sys
import time as _time
from typing import Any, Optional

import numpy as np
import pandas as pd
import plotly.graph_objects as go
from taipy.gui import Gui, State, invoke_long_callback, notify

# Optional project modules (same as the Dash version). If missing, synthetic
# data is used for the full session so the UI still runs standalone.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
try:
    import levels_demo as LD
    import market_data as MD
    import simccxt
    HAS_SIM = True
except ImportError:
    HAS_SIM = False
    LD = MD = simccxt = None  # type: ignore


# ---------------------------------------------------------------------------
# Constants (mirrored from the Dash app)
# ---------------------------------------------------------------------------
TFS = {"1m": 60_000, "5m": 300_000, "15m": 900_000, "1h": 3_600_000}
N_BARS = 1200
ZONE = 0.001
MAX_BARS = 200_000
STATUS_MSG = (
    "старт: синтетика, выбери даты — данные догрузятся с Binance "
    "в кэш data/ohlcv/"
)

# Module-level simulator state (single-threaded; Taipy callbacks share it)
STATE: Optional[dict] = None
PAPER: dict = {
    "deposit": 10000.0,
    "risk_pct": 1.0,
    "equity": 10000.0,
    "open": None,
    "closed": [],
    "draft": {},
    "last_msg": "",
}


# ---------------------------------------------------------------------------
# Synthetic OHLC (offline fallback)
# ---------------------------------------------------------------------------
def synthetic(symbol: str, tf: str, n: int = N_BARS) -> pd.DataFrame:
    """Deterministic OHLC bars for ANY coin/timeframe (offline fallback)."""
    h = int(hashlib.md5(symbol.encode()).hexdigest()[:8], 16)
    mag = 10 ** (h % 5)
    s0 = float(mag) * (1.0 + (h % 100) / 100.0)
    seed = h % 10_000
    step_s = TFS[tf] // 1000
    rng = np.random.default_rng(seed)
    t = np.arange(n) / (n / 4.0)
    logp = (
        np.log(s0)
        + 0.02 * t
        + 0.05 * np.sin(2 * np.pi * t / 4.0)
        + 0.015 * np.sin(2 * np.pi * t * 3.0)
        + np.cumsum(rng.normal(0.0, 0.0006 * (step_s / 60.0) ** 0.5, n))
    )
    close = np.exp(logp)
    rng2 = np.random.default_rng(seed + 1)
    half = 0.0003 + 0.0012 * np.abs(rng2.normal(0.0, 1.0, n))
    open_ = np.concatenate(([close[0]], close[:-1]))
    freq = {"1m": "min", "5m": "5min", "15m": "15min", "1h": "h"}[tf]
    ts = pd.date_range("2026-05-01", periods=n, freq=freq, tz="UTC")
    return pd.DataFrame(
        {
            "timestamp": ts,
            "open": open_,
            "high": np.maximum(open_, close) * (1.0 + half),
            "low": np.minimum(open_, close) * (1.0 - half),
            "close": close,
            "volume": rng.lognormal(0.0, 0.4, n) * 2.0,
        }
    )


def to_ms(ts: pd.Series) -> np.ndarray:
    naive = ts.dt.tz_convert("UTC").dt.tz_localize(None)
    return (naive.astype("datetime64[ns]").astype("int64") // 10**6).to_numpy()


# ---------------------------------------------------------------------------
# Minimal LiveLevels stub when levels_demo is absent
# ---------------------------------------------------------------------------
class _StubLiveLevels:
    def __init__(self, pivot=5, min_swing=0.0005, merge_tol=0.0005):
        self.levels: list = []
        self._pivot = pivot
        self._min_swing = min_swing
        self._bars: list = []

    def feed(self, bars):
        """Very light pivot detection so the UI still shows something offline."""
        self._bars = list(bars)
        if len(self._bars) < self._pivot * 2 + 1:
            return
        n = len(self._bars)
        for i in range(self._pivot, n - self._pivot):
            hi = self._bars[i][2]
            lo = self._bars[i][3]
            left_h = [self._bars[j][2] for j in range(i - self._pivot, i)]
            right_h = [self._bars[j][2] for j in range(i + 1, i + self._pivot + 1)]
            left_l = [self._bars[j][3] for j in range(i - self._pivot, i)]
            right_l = [self._bars[j][3] for j in range(i + 1, i + self._pivot + 1)]
            ts = int(self._bars[i][0])
            if hi >= max(left_h) and hi >= max(right_h):
                if not any(
                    abs(lv["price"] - hi) / hi < 0.001
                    and lv["direction"] == "resistance"
                    for lv in self.levels
                ):
                    self.levels.append(
                        {
                            "price": float(hi),
                            "direction": "resistance",
                            "pivot_ts": ts,
                            "confirmed_ts": int(self._bars[i + self._pivot][0]),
                            "confirming_bars": [
                                int(self._bars[j][0])
                                for j in range(i, i + self._pivot + 1)
                            ],
                            "touches": [],
                        }
                    )
            if lo <= min(left_l) and lo <= min(right_l):
                if not any(
                    abs(lv["price"] - lo) / lo < 0.001
                    and lv["direction"] == "support"
                    for lv in self.levels
                ):
                    self.levels.append(
                        {
                            "price": float(lo),
                            "direction": "support",
                            "pivot_ts": ts,
                            "confirmed_ts": int(self._bars[i + self._pivot][0]),
                            "confirming_bars": [
                                int(self._bars[j][0])
                                for j in range(i, i + self._pivot + 1)
                            ],
                            "touches": [],
                        }
                    )


# ---------------------------------------------------------------------------
# Session management
# ---------------------------------------------------------------------------
def new_session(
    symbol: str, tf: str, df: pd.DataFrame, source: str, key: tuple
) -> dict:
    """Build a simulator session from an OHLC dataframe."""
    if "ts_ms" not in df.columns:
        if "timestamp" in df.columns:
            df = df.copy()
            df["ts_ms"] = to_ms(df["timestamp"])
        else:
            raise ValueError("dataframe needs ts_ms or timestamp")
    df = df.sort_values("ts_ms").reset_index(drop=True)

    if HAS_SIM:
        ex = simccxt.binance()
        ex.add_market(symbol, "spot")
        ex.load_candles(
            symbol,
            tf,
            df[["ts_ms", "open", "high", "low", "close", "volume"]]
            .to_numpy()
            .tolist(),
        )
        ex.reset(int(df["ts_ms"].iloc[0]))
        ex.set_step_ms(TFS[tf])
        ex.resume()
        det = LD.LiveLevels(pivot=5, min_swing=0.0005, merge_tol=ZONE * 0.5)
    else:
        # Standalone fallback: keep bars in memory and step through them
        bars = df[["ts_ms", "open", "high", "low", "close", "volume"]].to_numpy()
        det = _StubLiveLevels(pivot=5, min_swing=0.0005, merge_tol=ZONE * 0.5)

        class _FakeEx:
            def __init__(self, bars, step_ms):
                self._bars = bars
                self._i = 0
                self._step_ms = step_ms
                self._start = int(bars[0, 0]) if len(bars) else 0

            def step(self, n: int):
                self._i = min(self._i + n, len(self._bars))

            def fetch_ohlcv_live(self, symbol, tf):
                return self._bars[: self._i].tolist() if self._i else []

            def now_ms(self):
                if self._i == 0:
                    return self._start
                return int(self._bars[min(self._i, len(self._bars)) - 1, 0])

            def replay_done(self, symbol, tf):
                return self._i >= len(self._bars)

        ex = _FakeEx(bars, TFS[tf])
        # Advance a bit so the chart is not empty on first paint
        ex.step(min(80, len(bars)))

    # Feed detector with whatever is already visible and mark those bars seen
    live0 = ex.fetch_ohlcv_live(symbol, tf)
    if live0:
        det.feed(live0)
        seen0 = len(live0)
    else:
        seen0 = 0

    return {
        "ex": ex,
        "symbol": symbol,
        "tf": tf,
        "key": key,
        "source": source,
        "range": (int(df["ts_ms"].iloc[0]), int(df["ts_ms"].iloc[-1])),
        "det": det,
        "seen": seen0,
        "running": True,
        "done": False,
        "speed": 2,
        "window": 300,
        "follow": True,
        "frozen": False,
    }


def _as_date_str(v) -> Optional[str]:
    """Taipy date control may return str, date, or datetime — normalize to YYYY-MM-DD."""
    if v is None or v == "":
        return None
    if isinstance(v, str):
        return v[:10]
    if isinstance(v, (dt.date, dt.datetime)):
        return v.isoformat()[:10]
    try:
        return pd.Timestamp(v).strftime("%Y-%m-%d")
    except Exception:
        return None


def desired_key(symbol, tf, start_date, end_date):
    start_date = _as_date_str(start_date)
    end_date = _as_date_str(end_date)
    if not start_date or not end_date:
        return None
    s = int(pd.Timestamp(start_date, tz="UTC").timestamp() * 1000)
    e = int(
        (pd.Timestamp(end_date, tz="UTC") + pd.Timedelta(days=1)).timestamp()
        * 1000
    )
    return (symbol, tf, s, e)


def valid_symbol(symbol) -> bool:
    if not isinstance(symbol, str) or "/" not in symbol:
        return False
    base, _, quote = symbol.partition("/")
    return bool(base.strip()) and bool(quote.strip())


def ensure_session(symbol, tf, start_date, end_date) -> bool:
    """Rebuild the session if the desired (coin, tf, dates) key changed."""
    global STATE, STATUS_MSG
    if not valid_symbol(symbol):
        STATUS_MSG = "введи монету как BASE/QUOTE, например DOGE/USDT"
        return False
    key = desired_key(symbol, tf, start_date, end_date)
    if key is None or (STATE is not None and key == STATE.get("key")):
        return False
    _, _, s, e = key
    n_exp = (e - s) // TFS[tf]
    if n_exp > MAX_BARS:
        STATUS_MSG = (
            f"отказ: диапазон {n_exp} баров > лимита {MAX_BARS} "
            f"(уменьши даты или возьми больший таймфрейм)"
        )
        if STATE is not None:
            STATE["key"] = key
        return False
    try:
        t0 = _time.time()
        if HAS_SIM and MD is not None:
            df, st = MD.load_or_download(symbol, tf, s, e)
            if len(df) == 0:
                raise RuntimeError("пустой диапазон (нет баров)")
            STATE = new_session(
                symbol, tf, df, source="binance(cache)+dl", key=key
            )
            STATUS_MSG = (
                f"Binance: из кэша {st['cached']}, докачано {st['downloaded']} "
                f"за {_time.time() - t0:.1f}с"
            )
        else:
            raise RuntimeError("simccxt/market_data недоступны")
    except Exception as exc:
        df = synthetic(symbol, tf)
        df["ts_ms"] = to_ms(df["timestamp"])
        STATE = new_session(
            symbol, tf, df, source="synthetic-fallback", key=key
        )
        STATUS_MSG = f"загрузка не удалась ({exc}); показан синтетический ряд"
    return True


# ---------------------------------------------------------------------------
# Touches + paper trading (identical logic to the Dash version)
# ---------------------------------------------------------------------------
def log_touches(det, new_bars, prev_close, zone=ZONE):
    ts = np.array([int(b[0]) for b in new_bars])
    cl = np.array([b[4] for b in new_bars])
    for k in range(len(new_bars)):
        t, c = ts[k], cl[k]
        prev = cl[k - 1] if k > 0 else prev_close
        if prev is None:
            continue
        for lv in det.levels:
            if lv["confirmed_ts"] >= t:
                continue
            lo, hi = lv["price"] * (1 - zone), lv["price"] * (1 + zone)
            if not (lo <= c <= hi):
                continue
            if lv["direction"] == "support" and prev > hi:
                lv["touches"].append({"ts": t, "price": c, "out": "touch"})
            elif lv["direction"] == "resistance" and prev < lo:
                lv["touches"].append({"ts": t, "price": c, "out": "touch"})


def calc_trade(equity, risk_pct, side, entry, sl, tp=None):
    if not (entry and entry > 0):
        raise ValueError("укажи цену входа > 0")
    if not (sl and sl > 0):
        raise ValueError("укажи стоп-лосс > 0")
    if abs(entry - sl) <= 0:
        raise ValueError("стоп должен отличаться от входа")
    if side > 0 and sl >= entry:
        raise ValueError("для LONG стоп должен быть НИЖЕ входа")
    if side < 0 and sl <= entry:
        raise ValueError("для SHORT стоп должен быть ВЫШЕ входа")
    if tp:
        if side > 0 and tp <= entry:
            raise ValueError("для LONG тейк должен быть ВЫШЕ входа")
        if side < 0 and tp >= entry:
            raise ValueError("для SHORT тейк должен быть НИЖЕ входа")
    risk_usd = equity * risk_pct / 100.0
    stop_dist = abs(entry - sl)
    size = risk_usd / stop_dist
    out = {
        "side": side,
        "entry": entry,
        "sl": sl,
        "tp": tp,
        "risk_usd": risk_usd,
        "stop_dist": stop_dist,
        "size": size,
        "notional": size * entry,
        "lev": size * entry / equity if equity else 0.0,
    }
    if tp:
        out["profit_usd"] = abs(tp - entry) * size
        out["rr"] = out["profit_usd"] / risk_usd if risk_usd else 0.0
    return out


def close_paper(px, ts, reason):
    t = PAPER["open"]
    pnl = t["side"] * (px - t["entry"]) * t["size"]
    PAPER["equity"] += pnl
    PAPER["closed"].append(
        {
            "open_ts": t["open_ts"],
            "close_ts": ts,
            "dir": "LONG" if t["side"] > 0 else "SHORT",
            "entry": t["entry"],
            "exit": px,
            "sl": t["sl"],
            "tp": t["tp"] or "-",
            "size": t["size"],
            "pnl": pnl,
            "pnl_pct": pnl / t["notional"] * 100.0 if t["notional"] else 0.0,
            "reason": reason,
            "equity": PAPER["equity"],
        }
    )
    PAPER["open"] = None
    PAPER["last_msg"] = (
        f"сделка закрыта: {reason} по {px:,.4f}, "
        f"P&L {pnl:+.2f} USDT (счёт {PAPER['equity']:.2f})"
    )


def settle_paper(new_bars):
    t = PAPER["open"]
    if not t:
        return
    for b in new_bars:
        ts, hi, lo = int(b[0]), b[2], b[3]
        if t["side"] > 0:
            if lo <= t["sl"]:
                close_paper(t["sl"], ts, "STOP")
                return
            if t["tp"] and hi >= t["tp"]:
                close_paper(t["tp"], ts, "TP")
                return
        else:
            if hi >= t["sl"]:
                close_paper(t["sl"], ts, "STOP")
                return
            if t["tp"] and lo <= t["tp"]:
                close_paper(t["tp"], ts, "TP")
                return


def paper_overlay():
    t = PAPER["open"]
    if t:
        return {"entry": t["entry"], "sl": t["sl"], "tp": t["tp"]}
    d = PAPER.get("draft") or {}
    if d.get("entry") and d.get("sl"):
        return {"entry": d["entry"], "sl": d["sl"], "tp": d.get("tp")}
    return None


# ---------------------------------------------------------------------------
# Figure / tables builders
# ---------------------------------------------------------------------------
def fmt_ts(ms):
    return pd.to_datetime(ms, unit="ms", utc=True).strftime("%m-%d %H:%M")


def make_fig(st, window) -> go.Figure:
    ex = st["ex"]
    bars = np.array(ex.fetch_ohlcv_live(st["symbol"], st["tf"]))
    fig = go.Figure()
    if len(bars) == 0:
        fig.update_layout(
            title="нет баров",
            height=700,
            xaxis_rangeslider_visible=False,
            margin=dict(l=50, r=10, t=30, b=30),
        )
        return fig

    seg = bars[max(0, len(bars) - int(window)) :]
    # ISO strings serialize cleanly through Taipy → plotly.js
    t = pd.to_datetime(seg[:, 0], unit="ms", utc=True).strftime("%Y-%m-%d %H:%M:%S").tolist()
    fig.add_trace(
        go.Candlestick(
            x=t,
            open=seg[:, 1].tolist(),
            high=seg[:, 2].tolist(),
            low=seg[:, 3].tolist(),
            close=seg[:, 4].tolist(),
            name=f"{st['symbol']} (бары)",
            increasing_line_color="#26a69a",
            increasing_fillcolor="#26a69a",
            decreasing_line_color="#ef5350",
            decreasing_fillcolor="#ef5350",
        )
    )
    for lv in st["det"].levels:
        fig.add_hline(
            y=lv["price"],
            line_color="green" if lv["direction"] == "support" else "red",
            line_dash="dash",
            line_width=1,
            opacity=0.8,
        )

    def _xstr(ms):
        return pd.to_datetime(ms, unit="ms", utc=True).strftime("%Y-%m-%d %H:%M:%S")

    px, py, ptext = [], [], []
    for lv in st["det"].levels:
        cb = lv.get("confirming_bars") or []
        if cb and cb[-1] >= int(seg[0, 0]):
            fig.add_vrect(
                x0=_xstr(cb[0]),
                x1=_xstr(cb[-1]),
                fillcolor="blue",
                opacity=0.07,
                line_width=0,
            )
        if lv["pivot_ts"] >= int(seg[0, 0]):
            px.append(_xstr(lv["pivot_ts"]))
            py.append(lv["price"])
            ptext.append(
                f"уровень {lv['price']:,.4f} ({lv['direction']})<br>"
                f"пивот-бар {_xstr(lv['pivot_ts'])}<br>"
                f"подтверждён {_xstr(lv['confirmed_ts'])}<br>"
                f"окно подтверждения: {len(cb)} баров<br>"
                f"касаний: {len(lv['touches'])}"
            )
    if px:
        fig.add_trace(
            go.Scatter(
                x=px,
                y=py,
                mode="markers",
                name="пивот-бары",
                marker=dict(symbol="diamond", size=10, color="blue"),
                text=ptext,
                hoverinfo="text",
            )
        )

    ov = paper_overlay()
    if ov:
        e, s = ov["entry"], ov["sl"]
        fig.add_hline(
            y=e,
            line_color="blue",
            line_width=2,
            annotation_text="вход",
            annotation_position="right",
        )
        fig.add_hline(
            y=s,
            line_color="red",
            line_width=2,
            line_dash="dot",
            annotation_text="стоп",
            annotation_position="right",
        )
        fig.add_hrect(
            y0=min(e, s),
            y1=max(e, s),
            fillcolor="red",
            opacity=0.07,
            line_width=0,
        )
        if ov.get("tp"):
            tp = ov["tp"]
            fig.add_hline(
                y=tp,
                line_color="green",
                line_width=2,
                line_dash="dot",
                annotation_text="тейк",
                annotation_position="right",
            )
            fig.add_hrect(
                y0=min(e, tp),
                y1=max(e, tp),
                fillcolor="green",
                opacity=0.07,
                line_width=0,
            )

    xs, ys = [], []
    for lv in st["det"].levels:
        for tc in lv["touches"]:
            if tc["ts"] >= int(seg[0, 0]):
                xs.append(_xstr(tc["ts"]))
                ys.append(tc["price"])
    if xs:
        fig.add_trace(
            go.Scatter(
                x=xs,
                y=ys,
                mode="markers",
                name="касания",
                marker=dict(color="orange", size=6),
            )
        )

    t0seg = int(seg[0, 0])
    if PAPER["open"]:
        tr = PAPER["open"]
        if tr["open_ts"] >= t0seg:
            fig.add_trace(
                go.Scatter(
                    x=[_xstr(tr["open_ts"])],
                    y=[tr["entry"]],
                    mode="markers",
                    name="вход (открыта)",
                    marker=dict(
                        symbol="triangle-up" if tr["side"] > 0 else "triangle-down",
                        size=14,
                        color="blue",
                    ),
                    hoverinfo="text",
                    text=(
                        f"ВХОД {'LONG' if tr['side'] > 0 else 'SHORT'}<br>"
                        f"цена {tr['entry']:,.4f}<br>размер {tr['size']:.6f}<br>"
                        f"стоп {tr['sl']:,.4f}"
                    ),
                )
            )
    first_seg = True
    for c in PAPER["closed"][-30:]:
        if c["open_ts"] < t0seg:
            continue
        col = "green" if c["pnl"] >= 0 else "red"
        fig.add_trace(
            go.Scatter(
                x=[_xstr(c["open_ts"]), _xstr(c["close_ts"])],
                y=[c["entry"], c["exit"]],
                mode="lines+markers",
                name="сделки" if first_seg else None,
                showlegend=first_seg,
                line=dict(color=col, width=2, dash="dot"),
                marker=dict(size=9, color=col),
                hoverinfo="text",
                text=[
                    f"ВХОД {c['dir']}<br>{c['entry']:,.4f}<br>{_xstr(c['open_ts'])}",
                    f"ВЫХОД {c['exit']:,.4f}<br>"
                    f"P&L {c['pnl']:+.2f} USDT ({c['reason']})<br>"
                    f"{_xstr(c['close_ts'])}",
                ],
            )
        )
        first_seg = False

    last = seg[-1]
    fig.add_annotation(
        x=t[-1],
        y=float(last[4]),
        text=f"{float(last[4]):,.1f}",
        showarrow=False,
        bgcolor="yellow",
    )
    fig.update_layout(
        xaxis_rangeslider_visible=False,
        height=700,
        hovermode="x unified",
        showlegend=True,
        uirevision="live" if st.get("follow", True) else "hold",
        margin=dict(l=50, r=10, t=10, b=30),
        xaxis_type="date",
    )
    return fig


def make_stats(st) -> str:
    global STATUS_MSG
    if st is None:
        return STATUS_MSG
    ex = st["ex"]
    n_lv = len(st["det"].levels)
    n_t = sum(len(lv["touches"]) for lv in st["det"].levels)
    now = pd.to_datetime(ex.now_ms(), unit="ms", utc=True).strftime("%m-%d %H:%M")
    flag = (
        "FINISHED"
        if st["done"]
        else ("RUNNING" if st["running"] else "PAUSED")
    )
    if st.get("frozen"):
        flag += " + VIEW FROZEN"
    elif not st.get("follow", True):
        flag += " + VIEW: свой"
    r0, r1 = st["range"]
    rng = (
        pd.to_datetime(r0, unit="ms", utc=True).strftime("%m-%d")
        + ".."
        + pd.to_datetime(r1, unit="ms", utc=True).strftime("%m-%d")
    )
    return (
        f"{st['symbol']} {st['tf']} [{st['source']} {rng}] | "
        f"sim-time {now} | bars {st['seen']} | levels {n_lv} | "
        f"touches {n_t} | {flag} || {STATUS_MSG}"
    )


def make_levels_rows(st) -> list:
    if st is None:
        return []
    rows = []
    for lv in sorted(
        st["det"].levels, key=lambda l: len(l["touches"]), reverse=True
    ):
        rows.append(
            {
                "уровень": f"{lv['price']:,.4f}",
                "тип": (
                    "поддержка"
                    if lv["direction"] == "support"
                    else "сопротивление"
                ),
                "пивот-бар": fmt_ts(lv["pivot_ts"]),
                "подтверждён": fmt_ts(lv["confirmed_ts"]),
                "баров в окне": len(lv.get("confirming_bars") or []),
                "касания": len(lv["touches"]),
            }
        )
    return rows


def make_closed_rows() -> list:
    rows = []
    for c in reversed(PAPER["closed"][-50:]):
        rows.append(
            {
                "открыта": fmt_ts(c["open_ts"]),
                "закрыта": fmt_ts(c["close_ts"]),
                "напр.": c["dir"],
                "вход": f"{c['entry']:,.4f}",
                "выход": f"{c['exit']:,.4f}",
                "стоп": f"{c['sl']:,.4f}",
                "размер": f"{c['size']:.6f}",
                "P&L, $": f"{c['pnl']:+.2f}",
                "P&L, %": f"{c['pnl_pct']:+.3f}%",
                "причина": c["reason"],
                "счёт": f"{c['equity']:.2f}",
            }
        )
    return rows


def make_open_panel(st) -> str:
    t = PAPER["open"]
    if t is None:
        return (
            f"счёт {PAPER['equity']:.2f} USDT "
            f"(депозит {PAPER['deposit']:.2f}, риск {PAPER['risk_pct']}%) — "
            f"нет открытой сделки"
        )
    bars = np.array(st["ex"].fetch_ohlcv_live(st["symbol"], st["tf"]))
    last = float(bars[-1, 4]) if len(bars) else t["entry"]
    pnl = t["side"] * (last - t["entry"]) * t["size"]
    ts = bars[:, 0].astype(np.int64) if len(bars) else np.array([])
    held = bars[ts >= t["open_ts"]] if len(bars) else bars
    hi = float(held[:, 2].max()) if len(held) else last
    lo = float(held[:, 3].min()) if len(held) else last
    mfe = (
        max(t["side"] * (hi - t["entry"]), t["side"] * (last - t["entry"]), 0.0)
        * t["size"]
    )
    mae = (
        min(t["side"] * (lo - t["entry"]), t["side"] * (last - t["entry"]), 0.0)
        * t["size"]
    )
    return (
        f"ОТКРЫТА {'LONG' if t['side'] > 0 else 'SHORT'} {t['size']:.6f} "
        f"(~{t['notional']:.1f} USDT, плечо-экв. x{t['lev']:.2f}) | "
        f"вход {t['entry']} | стоп {t['sl']} | "
        f"P&L {pnl:+.2f} USDT ({pnl / t['notional'] * 100:+.3f}%) | "
        f"MFE {mfe:+.2f} / MAE {mae:+.2f}"
    )


def calc_preview_text(equity, risk_pct, draft) -> str:
    if not draft or not draft.get("entry") or not draft.get("sl"):
        return "введи вход и стоп — размер посчитается сам"
    try:
        c = calc_trade(
            equity,
            risk_pct,
            draft["side"],
            draft["entry"],
            draft["sl"],
            draft.get("tp"),
        )
    except ValueError as exc:
        return f"⚠ {exc}"
    base = ""
    if STATE is not None and "/" in STATE.get("symbol", ""):
        base = STATE["symbol"].split("/")[0]
    s = (
        f"риск {c['risk_usd']:.2f} USDT | стоп-дистанция {c['stop_dist']:,.4f} "
        f"({c['stop_dist'] / c['entry'] * 100:.3f}%) | "
        f"размер {c['size']:.6f} {base} (~{c['notional']:.1f} USDT, "
        f"плечо-экв. x{c['lev']:.2f}) | макс. убыток −{c['risk_usd']:.2f}"
    )
    if "rr" in c:
        s += f" | профит +{c['profit_usd']:.2f} | R:R 1:{c['rr']:.2f}"
    return s


# ---------------------------------------------------------------------------
# One simulator step (called from the long-callback status handler)
# ---------------------------------------------------------------------------
def tick_once(speed: int, window: int) -> None:
    """Advance the simulator by `speed` bars and update detectors / paper."""
    global STATE
    if STATE is None or not STATE.get("running") or STATE.get("done"):
        return
    st, ex, det = STATE, STATE["ex"], STATE["det"]
    ex.step(int(speed))
    bars = ex.fetch_ohlcv_live(st["symbol"], st["tf"])
    if len(bars) > st["seen"]:
        new = bars[st["seen"] :]
        det.feed(bars)
        prev_close = (
            float(bars[st["seen"] - 1][4]) if st["seen"] > 0 else None
        )
        log_touches(det, new, prev_close)
        settle_paper(new)
        st["seen"] = len(bars)
    st["done"] = ex.replay_done(st["symbol"], st["tf"])
    st["window"] = int(window)
    st["speed"] = int(speed)


# ---------------------------------------------------------------------------
# Taipy state defaults
# ---------------------------------------------------------------------------
today = dt.datetime.now(dt.timezone.utc).date()
coin = "BTC/USDT"
tf = "1m"
tf_lov = list(TFS.keys())
start_date = (today - dt.timedelta(days=2)).isoformat()
end_date = today.isoformat()
speed = 2
window = 300
window_lov = [100, 200, 300, 500, 1200]

deposit = 10000.0
risk_pct = 1.0
direction = "long"
direction_lov = ["long", "short"]
entry_price: Optional[float] = None
stop_price: Optional[float] = None
tp_price: Optional[float] = None

# Pre-build a session so the first paint already has OHLC bars
# (on_init alone is unreliable across Taipy versions / first connection).
_ = ensure_session(coin, tf, start_date, end_date)
if STATE is not None:
    fig = make_fig(STATE, window)
    stats_text = make_stats(STATE)
    open_panel = make_open_panel(STATE)
    levels_data = make_levels_rows(STATE)
else:
    fig = go.Figure()
    stats_text = STATUS_MSG
    open_panel = "—"
    levels_data: list = []
calc_preview = "введи вход и стоп — размер посчитается сам"
trade_status = ""
closed_data: list = []
freeze_label = "⏸ Заморозить график"
pause_label = "Пауза / Продолжить"
refresh_active = False  # set True after first client connection starts the tick loop


# ---------------------------------------------------------------------------
# Callbacks
# ---------------------------------------------------------------------------
def _refresh_ui(state: State) -> None:
    """Push current STATE / PAPER into bound Taipy variables."""
    global STATE
    if STATE is None:
        state.stats_text = STATUS_MSG
        state.open_panel = "—"
        state.levels_data = []
        state.closed_data = make_closed_rows()
        return
    if not STATE.get("frozen"):
        # always assign a brand-new Figure so Taipy detects the change
        new_fig = make_fig(STATE, int(state.window or 300))
        state.fig = new_fig
    state.stats_text = make_stats(STATE)
    state.open_panel = make_open_panel(STATE)
    state.levels_data = make_levels_rows(STATE)
    state.closed_data = make_closed_rows()
    # draft preview
    side = +1 if (getattr(state, "direction", None) or "long") == "long" else -1
    draft = {}
    try:
        e = float(state.entry_price) if state.entry_price not in (None, "") else None
        s = float(state.stop_price) if state.stop_price not in (None, "") else None
        t = float(state.tp_price) if state.tp_price not in (None, "") else None
        if e and s:
            draft = {"side": side, "entry": e, "sl": s, "tp": t}
            PAPER["draft"] = draft
    except (TypeError, ValueError):
        pass
    state.calc_preview = calc_preview_text(
        PAPER["equity"], PAPER["risk_pct"], draft or PAPER.get("draft")
    )


def on_params_change(state: State) -> None:
    """Coin / TF / dates changed → rebuild session, then refresh UI."""
    global STATE, PAPER
    rebuilt = ensure_session(
        state.coin, state.tf, state.start_date, state.end_date
    )
    if rebuilt and PAPER["open"]:
        PAPER["open"] = None
        PAPER["last_msg"] = "открытая сделка отменена (смена инструмента/дат)"
        state.trade_status = PAPER["last_msg"]
    if STATE is not None:
        STATE["speed"] = int(state.speed)
        STATE["window"] = int(state.window)
        STATE["follow"] = True
        STATE["frozen"] = False
        state.freeze_label = "⏸ Заморозить график"
    _refresh_ui(state)
    if not state.refresh_active and STATE is not None:
        state.refresh_active = True
        # Start the background tick loop (status every 1000 ms)
        invoke_long_callback(
            state,
            _bg_loop,
            [],
            _on_tick_status,
            [],
            1000,
        )


def _bg_loop() -> None:
    """Long-running placeholder; actual work is in the status callback."""
    while True:
        _time.sleep(3600)


def _on_tick_status(state: State, status) -> None:
    """Called ~every second by invoke_long_callback."""
    global STATE
    if STATE is None:
        # try to init once
        ensure_session(state.coin, state.tf, state.start_date, state.end_date)
        if STATE is None:
            state.stats_text = STATUS_MSG
            return
    tick_once(int(state.speed), int(state.window))
    _refresh_ui(state)


def on_speed_window(state: State) -> None:
    if STATE is not None:
        STATE["speed"] = int(state.speed)
        STATE["window"] = int(state.window)
        if not STATE.get("frozen"):
            state.fig = make_fig(STATE, state.window)


def on_pause(state: State) -> None:
    if STATE is None:
        notify(state, "w", "сначала дождись загрузки данных")
        return
    STATE["running"] = not STATE["running"]
    state.stats_text = make_stats(STATE)
    notify(
        state,
        "i",
        "симуляция " + ("продолжена" if STATE["running"] else "на паузе"),
    )


def on_reset(state: State) -> None:
    global STATE
    STATE = None
    ensure_session(state.coin, state.tf, state.start_date, state.end_date)
    if STATE is not None:
        STATE["follow"] = True
        STATE["frozen"] = False
    state.freeze_label = "⏸ Заморозить график"
    _refresh_ui(state)
    notify(state, "i", "сессия сброшена")


def on_freeze(state: State) -> None:
    if STATE is None:
        return
    STATE["frozen"] = not STATE.get("frozen", False)
    state.freeze_label = (
        "▶ Продолжить график" if STATE["frozen"] else "⏸ Заморозить график"
    )
    state.stats_text = make_stats(STATE)
    if not STATE["frozen"]:
        state.fig = make_fig(STATE, state.window)


def on_draft_change(state: State) -> None:
    side = +1 if state.direction == "long" else -1
    draft = {}
    try:
        e = float(state.entry_price) if state.entry_price not in (None, "") else None
        s = float(state.stop_price) if state.stop_price not in (None, "") else None
        t = float(state.tp_price) if state.tp_price not in (None, "") else None
        if e and s:
            draft = {"side": side, "entry": e, "sl": s, "tp": t}
    except (TypeError, ValueError):
        draft = {}
    PAPER["draft"] = draft
    state.calc_preview = calc_preview_text(
        PAPER["equity"], PAPER["risk_pct"], draft
    )
    if STATE is not None and not STATE.get("frozen"):
        state.fig = make_fig(STATE, state.window)


def on_open_trade(state: State) -> None:
    if STATE is None:
        state.trade_status = "сначала дождись загрузки данных"
        return
    if PAPER["open"]:
        state.trade_status = "сделка уже открыта — сначала закрой её"
        return
    try:
        PAPER["deposit"] = float(state.deposit or PAPER["deposit"])
        PAPER["risk_pct"] = float(
            state.risk_pct if state.risk_pct not in (None, "") else 1.0
        )
    except (TypeError, ValueError):
        state.trade_status = "⚠ депозит и риск должны быть числами"
        return
    side = +1 if state.direction == "long" else -1
    try:
        e = float(state.entry_price) if state.entry_price not in (None, "") else None
        s = float(state.stop_price) if state.stop_price not in (None, "") else None
        t = float(state.tp_price) if state.tp_price not in (None, "") else None
    except (TypeError, ValueError):
        state.trade_status = "⚠ заполни вход и стоп числами"
        return
    if not e or not s:
        state.trade_status = "⚠ заполни вход и стоп"
        return
    try:
        c = calc_trade(PAPER["equity"], PAPER["risk_pct"], side, e, s, t)
    except ValueError as exc:
        state.trade_status = f"⚠ {exc}"
        return
    PAPER["open"] = dict(c, open_ts=STATE["ex"].now_ms())
    state.trade_status = (
        f"открыта {'LONG' if c['side'] > 0 else 'SHORT'} {c['size']:.6f} "
        f"по {c['entry']}, стоп {c['sl']}, риск {c['risk_usd']:.2f} USDT"
    )
    state.open_panel = make_open_panel(STATE)
    if not STATE.get("frozen"):
        state.fig = make_fig(STATE, state.window)


def on_close_trade(state: State) -> None:
    t = PAPER["open"]
    if not t or STATE is None:
        state.trade_status = "нет открытой сделки"
        return
    bars = STATE["ex"].fetch_ohlcv_live(STATE["symbol"], STATE["tf"])
    px = float(bars[-1][4]) if bars else t["entry"]
    close_paper(px, STATE["ex"].now_ms(), "MANUAL")
    state.trade_status = PAPER["last_msg"]
    state.open_panel = make_open_panel(STATE)
    state.closed_data = make_closed_rows()
    if not STATE.get("frozen"):
        state.fig = make_fig(STATE, state.window)


def on_reset_acct(state: State) -> None:
    try:
        PAPER["deposit"] = float(state.deposit or 10000.0)
        PAPER["risk_pct"] = float(
            state.risk_pct if state.risk_pct not in (None, "") else 1.0
        )
    except (TypeError, ValueError):
        state.trade_status = "⚠ депозит и риск должны быть числами"
        return
    PAPER["equity"] = PAPER["deposit"]
    PAPER["open"] = None
    PAPER["closed"] = []
    PAPER["last_msg"] = (
        f"счёт сброшен: {PAPER['equity']:.2f} USDT, риск {PAPER['risk_pct']}%"
    )
    state.trade_status = PAPER["last_msg"]
    state.open_panel = make_open_panel(STATE) if STATE else "—"
    state.closed_data = []
    if STATE is not None and not STATE.get("frozen"):
        state.fig = make_fig(STATE, state.window)


def on_init(state: State) -> None:
    """Called once per browser connection. Rebuild UI from STATE and start ticks."""
    global STATE
    # Prefer existing module-level session; rebuild only if params differ
    rebuilt = ensure_session(
        state.coin, state.tf, state.start_date, state.end_date
    )
    if STATE is None:
        # force synthetic session as last resort
        df = synthetic(state.coin or "BTC/USDT", state.tf or "1m")
        df["ts_ms"] = to_ms(df["timestamp"])
        key = desired_key(state.coin, state.tf, state.start_date, state.end_date) or (
            state.coin, state.tf, 0, 1
        )
        STATE = new_session(
            state.coin or "BTC/USDT",
            state.tf or "1m",
            df,
            source="synthetic-fallback",
            key=key,
        )
    _refresh_ui(state)
    if not getattr(state, "refresh_active", False):
        state.refresh_active = True
        invoke_long_callback(
            state,
            _bg_loop,
            [],
            _on_tick_status,
            [],
            1000,
        )


# ---------------------------------------------------------------------------
# Page layout (Taipy builder API)
# ---------------------------------------------------------------------------
page = """
<|layout|columns=1 300px|gap=12px|

<|part|
<|chart|figure={fig}|rebuild|height=700px|>
|>

<|part|
#### Данные
**Монета**
<|{coin}|input|on_change=on_params_change|change_delay=800|>

**Таймфрейм**
<|{tf}|selector|lov={tf_lov}|dropdown|on_change=on_params_change|>

**Период (UTC)**
Начало: <|{start_date}|date|on_change=on_params_change|>
Конец: <|{end_date}|date|on_change=on_params_change|>

**Скорость (баров/тик)**
<|{speed}|slider|min=1|max=20|on_change=on_speed_window|>

**Окно (баров)**
<|{window}|selector|lov={window_lov}|dropdown|on_change=on_speed_window|>

<|{pause_label}|button|on_action=on_pause|>
<|Заново|button|on_action=on_reset|>
<|{freeze_label}|button|on_action=on_freeze|>

<pre>{stats_text}</pre>

<small>смена монеты / таймфрейма / дат автоматически догружает данные:
сначала кэш data/ohlcv/, недостающее — с Binance.</small>

#### Виртуальная торговля
**Депозит, USDT**
<|{deposit}|number|on_change=on_draft_change|>

**Риск на сделку, %**
<|{risk_pct}|number|on_change=on_draft_change|>

<|Сбросить счёт|button|on_action=on_reset_acct|>

**Направление**
<|{direction}|selector|lov={direction_lov}|dropdown|on_change=on_draft_change|>

**Вход**
<|{entry_price}|number|on_change=on_draft_change|>

**Стоп**
<|{stop_price}|number|on_change=on_draft_change|>

**Тейк (необяз.)**
<|{tp_price}|number|on_change=on_draft_change|>

<|Открыть сделку|button|on_action=on_open_trade|>
<|Закрыть по рынку|button|on_action=on_close_trade|>

<pre>{calc_preview}</pre>
<small style="color:#064">{trade_status}</small>
<pre style="background:#f6f6f6;padding:6px 10px">{open_panel}</pre>
|>
|>

### Сделки
<|{closed_data}|table|page_size=10|width=100%|>

### Уровни: откуда взялся каждый (пивот → подтверждение → касания)
<|{levels_data}|table|page_size=15|width=100%|>
"""


def main(argv=None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8050)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args(argv)

    # on_init is discovered by name on the module — do NOT pass it to run()
    gui = Gui(page=page)
    print(f"open http://{args.host}:{args.port}/  (Ctrl+C to stop)")
    gui.run(
        host=args.host,
        port=args.port,
        title="simccxt live levels (Taipy)",
        dark_mode=False,
        use_reloader=False,
        run_browser=True,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
