#!/usr/bin/env python3
"""
live_dashboard_taipy.py
=======================
Taipy GUI + Plotly rewrite of live_dashboard.py — no Dash, no dcc.

Fixes vs. earlier Taipy attempt:
  * page built with taipy.gui.builder (tgb) -> no "variable not available" warnings
  * plotly figure bound via figure="{fig}" -> no "no Data Accessor" warning
  * periodic UI push via invoke_long_callback (Taipy 3.x official API),
    not via non-public broadcast_changes/broadcast_change
  * background thread only prepares a snapshot; Taipy callbacks do the
    state assignments in the session context
  * socketio ping_interval=1 + async_mode="threading" so updates don't
    pile up in the websocket buffer

Run:
    pip install "taipy>=3.0" plotly
    .venv/bin/python examples/live_dashboard_taipy.py [--port 5000]
    open http://127.0.0.1:5000/
"""

import argparse
import datetime as dt
import os
import sys
import threading
import time as _time

import numpy as np
import pandas as pd
import plotly.graph_objects as go

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import levels_demo as LD
import market_data as MD

import simccxt

try:
    from taipy.gui import Gui, State, invoke_long_callback
    import taipy.gui.builder as tgb
except ImportError:
    sys.exit('pip install "taipy>=3.0" plotly')

TFS = {"1m": 60_000, "5m": 300_000, "15m": 900_000, "1h": 3_600_000}
N_BARS = 1200
ZONE = 0.001
MAX_BARS = 200_000


# ================================================================ helpers
def synthetic(symbol: str, tf: str, n: int = N_BARS) -> pd.DataFrame:
    """Deterministic OHLC bars for ANY coin/timeframe (offline fallback)."""
    import hashlib
    h = int(hashlib.md5(symbol.encode()).hexdigest()[:8], 16)
    mag = 10 ** (h % 5)
    s0 = float(mag) * (1.0 + (h % 100) / 100.0)
    seed = h % 10_000
    step_s = TFS[tf] // 1000
    rng = np.random.default_rng(seed)
    t = np.arange(n) / (n / 4.0)
    logp = (np.log(s0) + 0.02 * t
            + 0.05 * np.sin(2 * np.pi * t / 4.0)
            + 0.015 * np.sin(2 * np.pi * t * 3.0)
            + np.cumsum(rng.normal(0.0, 0.0006 * (step_s / 60.0) ** 0.5, n)))
    close = np.exp(logp)
    rng2 = np.random.default_rng(seed + 1)
    half = 0.0003 + 0.0012 * np.abs(rng2.normal(0.0, 1.0, n))
    open_ = np.concatenate(([close[0]], close[:-1]))
    freq = {"1m": "min", "5m": "5min", "15m": "15min", "1h": "h"}[tf]
    ts = pd.date_range("2026-05-01", periods=n, freq=freq, tz="UTC")
    return pd.DataFrame({
        "timestamp": ts, "open": open_,
        "high": np.maximum(open_, close) * (1.0 + half),
        "low": np.minimum(open_, close) * (1.0 - half),
        "close": close, "volume": rng.lognormal(0.0, 0.4, n) * 2.0,
    })


def to_ms(ts: pd.Series) -> np.ndarray:
    naive = ts.dt.tz_convert("UTC").dt.tz_localize(None)
    return (naive.astype("datetime64[ns]").astype("int64") // 10**6).to_numpy()


# ================================================================ session
STATE = None
_STATE_LOCK = threading.RLock()
STATUS_MSG = ("старт: синтетика, выбери даты — данные догрузятся "
              "с Binance в кэш data/ohlcv/")


def new_session(symbol: str, tf: str, df: pd.DataFrame, source: str,
                key: tuple) -> dict:
    df = df.sort_values("ts_ms").reset_index(drop=True)
    ex = simccxt.binance()
    ex.add_market(symbol, "spot")
    ex.load_candles(symbol, tf, df[["ts_ms", "open", "high", "low",
                                    "close", "volume"]].to_numpy().tolist())
    ex.reset(int(df["ts_ms"].iloc[0]))
    ex.set_step_ms(TFS[tf])
    ex.resume()
    return {"ex": ex, "symbol": symbol, "tf": tf, "key": key,
            "source": source, "range": (int(df["ts_ms"].iloc[0]),
                                        int(df["ts_ms"].iloc[-1])),
            "det": LD.LiveLevels(pivot=5, min_swing=0.0005,
                                 merge_tol=ZONE * 0.5),
            "seen": 0, "running": True, "done": False,
            "speed": 2, "window": 300}


def desired_key(symbol, tf, start_date, end_date):
    if not start_date or not end_date:
        return None
    s = int(pd.Timestamp(start_date, tz="UTC").timestamp() * 1000)
    e = int((pd.Timestamp(end_date, tz="UTC")
             + pd.Timedelta(days=1)).timestamp() * 1000)
    return (symbol, tf, s, e)


def valid_symbol(symbol) -> bool:
    if not isinstance(symbol, str) or "/" not in symbol:
        return False
    base, _, quote = symbol.partition("/")
    return bool(base.strip()) and bool(quote.strip())


def ensure_session(symbol, tf, start_date, end_date):
    """Rebuild session if (coin, tf, dates) key changed. Caller holds lock."""
    global STATE, STATUS_MSG
    if not valid_symbol(symbol):
        STATUS_MSG = "введи монету как BASE/QUOTE, например DOGE/USDT (Enter)"
        return False
    key = desired_key(symbol, tf, start_date, end_date)
    if key is None or (STATE is not None and key == STATE.get("key")):
        return False
    _, _, s, e = key
    n_exp = (e - s) // TFS[tf]
    if n_exp > MAX_BARS:
        STATUS_MSG = (f"отказ: диапазон {n_exp} баров > лимита {MAX_BARS} "
                      f"(уменьши даты или возьми больший таймфрейм)")
        if STATE is not None:
            STATE["key"] = key
        return False
    try:
        t0 = _time.time()
        df, st = MD.load_or_download(symbol, tf, s, e)
        if len(df) == 0:
            raise RuntimeError("пустой диапазон (нет баров)")
        STATE = new_session(symbol, tf, df, source="binance(cache)+dl", key=key)
        STATUS_MSG = (f"Binance: из кэша {st['cached']}, докачано "
                      f"{st['downloaded']} за {_time.time() - t0:.1f}с")
    except Exception as exc:  # noqa: BLE001
        df = synthetic(symbol, tf)
        df["ts_ms"] = to_ms(df["timestamp"])
        STATE = new_session(symbol, tf, df, source="synthetic-fallback", key=key)
        STATUS_MSG = f"загрузка не удалась ({exc}); показан синтетический ряд"
    return True


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


# ================================================================ paper trading
PAPER = {"deposit": 10000.0, "risk_pct": 1.0, "equity": 10000.0,
         "open": None, "closed": [], "draft": {}, "last_msg": ""}


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
    out = {"side": side, "entry": entry, "sl": sl, "tp": tp,
           "risk_usd": risk_usd, "stop_dist": stop_dist, "size": size,
           "notional": size * entry,
           "lev": size * entry / equity if equity else 0.0}
    if tp:
        out["profit_usd"] = abs(tp - entry) * size
        out["rr"] = out["profit_usd"] / risk_usd if risk_usd else 0.0
    return out


def close_paper(px, ts, reason):
    t = PAPER["open"]
    pnl = t["side"] * (px - t["entry"]) * t["size"]
    PAPER["equity"] += pnl
    PAPER["closed"].append({
        "open_ts": t["open_ts"], "close_ts": ts,
        "dir": "LONG" if t["side"] > 0 else "SHORT",
        "entry": t["entry"], "exit": px, "sl": t["sl"],
        "tp": t["tp"] or "-",
        "size": t["size"], "pnl": pnl,
        "pnl_pct": pnl / t["notional"] * 100.0 if t["notional"] else 0.0,
        "reason": reason, "equity": PAPER["equity"]})
    PAPER["open"] = None
    PAPER["last_msg"] = (f"сделка закрыта: {reason} по {px:,.4f}, "
                         f"P&L {pnl:+.2f} USDT (счёт {PAPER['equity']:.2f})")


def settle_paper(new_bars):
    t = PAPER["open"]
    if not t:
        return
    for b in new_bars:
        ts, hi, lo = int(b[0]), b[2], b[3]
        if t["side"] > 0:
            if lo <= t["sl"]:
                close_paper(t["sl"], ts, "STOP"); return
            if t["tp"] and hi >= t["tp"]:
                close_paper(t["tp"], ts, "TP"); return
        else:
            if hi >= t["sl"]:
                close_paper(t["sl"], ts, "STOP"); return
            if t["tp"] and lo <= t["tp"]:
                close_paper(t["tp"], ts, "TP"); return


def paper_overlay():
    t = PAPER["open"]
    if t:
        return {"entry": t["entry"], "sl": t["sl"], "tp": t["tp"]}
    d = PAPER.get("draft") or {}
    if d.get("entry") and d.get("sl"):
        return {"entry": d["entry"], "sl": d["sl"], "tp": d.get("tp")}
    return None


# ================================================================ UI vars
_today = dt.datetime.now(dt.timezone.utc).date()

coin = "BTC/USDT"
tf = "1m"
tf_list = list(TFS)
start_date = _today - dt.timedelta(days=2)
end_date = _today
speed = 2
window = 300
window_list = [100, 200, 300, 500, 1200]
deposit = 10000.0
risk_pct = 1.0
direction = "long"
direction_list = ["long", "short"]
entry_in = ""
stop_in = ""
tp_in = ""

fig = go.Figure()
stats = STATUS_MSG
panel = "—"
calc_preview = "введи вход и стоп — размер посчитается сам"
trade_status = ""
levels_df = pd.DataFrame(columns=["level", "dir", "pivot", "confirmed",
                                  "window", "touches"])
trades_df = pd.DataFrame(columns=["open_ts", "close_ts", "dir", "entry",
                                  "exit", "sl", "size", "pnl", "pnl_pct",
                                  "reason", "equity"])
frozen_label = "⏸ Заморозить график"
FROZEN = False


# ================================================================ formatting
def fmt_ts(ms):
    return pd.to_datetime(ms, unit="ms", utc=True).strftime("%m-%d %H:%M")


def read_draft(direction, entry, stop, tp):
    side = +1 if (direction or "long") == "long" else -1
    try:
        e = float(entry) if entry not in (None, "") else None
        s = float(stop) if stop not in (None, "") else None
        t = float(tp) if tp not in (None, "") else None
    except (TypeError, ValueError):
        return None
    if e and s:
        return {"side": side, "entry": e, "sl": s, "tp": t}
    return None


def calc_text(equity, risk_pct, draft):
    if not draft:
        return "введи вход и стоп — размер посчитается сам"
    try:
        c = calc_trade(equity, risk_pct, draft["side"], draft["entry"],
                       draft["sl"], draft["tp"])
    except ValueError as exc:
        return f"⚠ {exc}"
    base = ""
    if STATE is not None and "/" in STATE.get("symbol", ""):
        base = STATE["symbol"].split("/")[0]
    s = (f"риск {c['risk_usd']:.2f} USDT | стоп-дистанция {c['stop_dist']:,.4f} "
         f"({c['stop_dist'] / c['entry'] * 100:.3f}%) | "
         f"размер {c['size']:.6f} {base} (~{c['notional']:.1f} USDT, "
         f"плечо-экв. x{c['lev']:.2f}) | макс. убыток −{c['risk_usd']:.2f}")
    if "rr" in c:
        s += f" | профит +{c['profit_usd']:.2f} | R:R 1:{c['rr']:.2f}"
    return s


def make_panel(st):
    t = PAPER["open"]
    if t is None:
        return (f"счёт {PAPER['equity']:.2f} USDT "
                f"(депозит {PAPER['deposit']:.2f}, риск {PAPER['risk_pct']}%) — "
                f"нет открытой сделки")
    bars = np.array(st["ex"].fetch_ohlcv_live(st["symbol"], st["tf"]))
    last = float(bars[-1, 4]) if len(bars) else t["entry"]
    pnl = t["side"] * (last - t["entry"]) * t["size"]
    ts = bars[:, 0].astype(np.int64) if len(bars) else np.array([])
    held = bars[ts >= t["open_ts"]] if len(bars) else bars
    hi = float(held[:, 2].max()) if len(held) else last
    lo = float(held[:, 3].min()) if len(held) else last
    mfe = max(t["side"] * (hi - t["entry"]),
              t["side"] * (last - t["entry"]), 0.0) * t["size"]
    mae = min(t["side"] * (lo - t["entry"]),
              t["side"] * (last - t["entry"]), 0.0) * t["size"]
    return (f"ОТКРЫТА {'LONG' if t['side'] > 0 else 'SHORT'} {t['size']:.6f} "
            f"(~{t['notional']:.1f} USDT, плечо-экв. x{t['lev']:.2f}) | "
            f"вход {t['entry']} | стоп {t['sl']} | "
            f"P&L {pnl:+.2f} USDT ({pnl / t['notional'] * 100:+.3f}%) | "
            f"MFE {mfe:+.2f} / MAE {mae:+.2f}")


def trades_to_df():
    rows = []
    for c in reversed(PAPER["closed"][-50:]):
        rows.append({
            "open_ts": fmt_ts(c["open_ts"]), "close_ts": fmt_ts(c["close_ts"]),
            "dir": c["dir"], "entry": f"{c['entry']:,.4f}",
            "exit": f"{c['exit']:,.4f}", "sl": f"{c['sl']:,.4f}",
            "size": f"{c['size']:.6f}", "pnl": f"{c['pnl']:+.2f}",
            "pnl_pct": f"{c['pnl_pct']:+.3f}%", "reason": c["reason"],
            "equity": f"{c['equity']:.2f}"})
    return pd.DataFrame(rows, columns=trades_df.columns)


def levels_to_df(st):
    cols = ["level", "dir", "pivot", "confirmed", "window", "touches"]
    if st is None:
        return pd.DataFrame(columns=cols)
    rows = []
    for lv in sorted(st["det"].levels,
                     key=lambda l: len(l["touches"]), reverse=True):
        rows.append({
            "level": f"{lv['price']:,.4f}",
            "dir": "поддержка" if lv["direction"] == "support" else "сопротивление",
            "pivot": pd.to_datetime(lv["pivot_ts"], unit="ms",
                                    utc=True).strftime("%m-%d %H:%M"),
            "confirmed": pd.to_datetime(lv["confirmed_ts"], unit="ms",
                                        utc=True).strftime("%m-%d %H:%M"),
            "window": len(lv.get("confirming_bars") or []),
            "touches": len(lv["touches"]),
        })
    return pd.DataFrame(rows, columns=cols)


def make_stats(st, frozen=False):
    ex = st["ex"]
    n_lv = len(st["det"].levels)
    n_t = sum(len(lv["touches"]) for lv in st["det"].levels)
    now = pd.to_datetime(ex.now_ms(), unit="ms", utc=True).strftime("%m-%d %H:%M")
    flag = "FINISHED" if st["done"] else ("RUNNING" if st["running"] else "PAUSED")
    if frozen:
        flag += " + VIEW FROZEN"
    r0, r1 = st["range"]
    rng = (pd.to_datetime(r0, unit="ms", utc=True).strftime("%m-%d") + ".." +
           pd.to_datetime(r1, unit="ms", utc=True).strftime("%m-%d"))
    return (f"{st['symbol']} {st['tf']} [{st['source']} {rng}] | sim-time {now} | "
            f"bars {st['seen']} | levels {n_lv} | touches {n_t} | {flag} || "
            f"{STATUS_MSG}")


def make_fig(st, window):
    ex = st["ex"]
    bars = np.array(ex.fetch_ohlcv_live(st["symbol"], st["tf"]))
    fig = go.Figure()
    if len(bars):
        seg = bars[max(0, len(bars) - window):]
        t = pd.to_datetime(seg[:, 0], unit="ms")
        fig.add_trace(go.Ohlc(x=t, open=seg[:, 1], high=seg[:, 2],
                              low=seg[:, 3], close=seg[:, 4],
                              name=f"{st['symbol']} (бары)",
                              increasing_line_color="black",
                              decreasing_line_color="black"))
        for lv in st["det"].levels:
            fig.add_hline(
                y=lv["price"],
                line_color="green" if lv["direction"] == "support" else "red",
                line_dash="dash", line_width=1, opacity=0.8)
        px, py, ptext = [], [], []
        for lv in st["det"].levels:
            cb = lv.get("confirming_bars") or []
            if cb and cb[-1] >= int(seg[0, 0]):
                fig.add_vrect(x0=pd.to_datetime(cb[0], unit="ms"),
                              x1=pd.to_datetime(cb[-1], unit="ms"),
                              fillcolor="blue", opacity=0.07, line_width=0)
            if lv["pivot_ts"] >= int(seg[0, 0]):
                px.append(pd.to_datetime(lv["pivot_ts"], unit="ms"))
                py.append(lv["price"])
                ptext.append(
                    f"уровень {lv['price']:,.4f} ({lv['direction']})<br>"
                    f"пивот-бар {pd.to_datetime(lv['pivot_ts'], unit='ms')}<br>"
                    f"подтверждён {pd.to_datetime(lv['confirmed_ts'], unit='ms')}<br>"
                    f"окно подтверждения: {len(cb)} баров<br>"
                    f"касаний: {len(lv['touches'])}")
        if px:
            fig.add_trace(go.Scatter(x=px, y=py, mode="markers",
                                     name="пивот-бары",
                                     marker=dict(symbol="diamond", size=10,
                                                 color="blue"),
                                     text=ptext, hoverinfo="text"))
        ov = paper_overlay()
        if ov:
            e, s = ov["entry"], ov["sl"]
            fig.add_hline(y=e, line_color="blue", line_width=2,
                          annotation_text="вход", annotation_position="right")
            fig.add_hline(y=s, line_color="red", line_width=2, line_dash="dot",
                          annotation_text="стоп", annotation_position="right")
            fig.add_hrect(y0=min(e, s), y1=max(e, s),
                          fillcolor="red", opacity=0.07, line_width=0)
            if ov.get("tp"):
                tk = ov["tp"]
                fig.add_hline(y=tk, line_color="green", line_width=2,
                              line_dash="dot", annotation_text="тейк",
                              annotation_position="right")
                fig.add_hrect(y0=min(e, tk), y1=max(e, tk),
                              fillcolor="green", opacity=0.07, line_width=0)
        xs, ys = [], []
        for lv in st["det"].levels:
            for tc in lv["touches"]:
                if tc["ts"] >= int(seg[0, 0]):
                    xs.append(pd.to_datetime(tc["ts"], unit="ms"))
                    ys.append(tc["price"])
        if xs:
            fig.add_trace(go.Scatter(x=xs, y=ys, mode="markers", name="касания",
                                     marker=dict(color="orange", size=6)))
        t0seg = int(seg[0, 0])
        if PAPER["open"]:
            tr = PAPER["open"]
            if tr["open_ts"] >= t0seg:
                fig.add_trace(go.Scatter(
                    x=[pd.to_datetime(tr["open_ts"], unit="ms")], y=[tr["entry"]],
                    mode="markers", name="вход (открыта)",
                    marker=dict(
                        symbol="triangle-up" if tr["side"] > 0 else "triangle-down",
                        size=14, color="blue"),
                    hoverinfo="text",
                    text=(f"ВХОД {'LONG' if tr['side'] > 0 else 'SHORT'}<br>"
                          f"цена {tr['entry']:,.4f}<br>"
                          f"размер {tr['size']:.6f}<br>стоп {tr['sl']:,.4f}")))
        first_seg = True
        for c in PAPER["closed"][-30:]:
            if c["open_ts"] < t0seg:
                continue
            col = "green" if c["pnl"] >= 0 else "red"
            fig.add_trace(go.Scatter(
                x=[pd.to_datetime(c["open_ts"], unit="ms"),
                   pd.to_datetime(c["close_ts"], unit="ms")],
                y=[c["entry"], c["exit"]], mode="lines+markers",
                name="сделки" if first_seg else None,
                showlegend=first_seg,
                line=dict(color=col, width=2, dash="dot"),
                marker=dict(size=9, color=col),
                hoverinfo="text",
                text=[f"ВХОД {c['dir']}<br>{c['entry']:,.4f}<br>"
                      f"{pd.to_datetime(c['open_ts'], unit='ms')}",
                      f"ВЫХОД {c['exit']:,.4f}<br>"
                      f"P&L {c['pnl']:+.2f} USDT ({c['reason']})<br>"
                      f"{pd.to_datetime(c['close_ts'], unit='ms')}"]))
            first_seg = False
        last = seg[-1]
        fig.add_annotation(x=t[-1], y=last[4], text=f"{last[4]:,.1f}",
                           showarrow=False, bgcolor="yellow")
    fig.update_layout(xaxis_rangeslider_visible=False, height=800,
                      hovermode="x unified", showlegend=True,
                      uirevision="live",
                      margin=dict(l=50, r=10, t=10, b=30))
    return fig


# ================================================================ snapshot
_LAST_SNAPSHOT = None
_SNAPSHOT_LOCK = threading.Lock()


def step_once():
    """Advance the sim by `speed` bars; return the fresh UI snapshot."""
    global STATE
    with _STATE_LOCK:
        rebuilt = ensure_session(coin, tf, start_date, end_date)
        if STATE is None:
            return None
        if rebuilt and PAPER["open"]:
            PAPER["open"] = None
            PAPER["last_msg"] = "открытая сделка отменена (смена инструмента/дат)"
        st = STATE
        st["speed"] = int(speed)
        if st["running"] and not st["done"]:
            st["ex"].step(int(speed))
            bars = st["ex"].fetch_ohlcv_live(st["symbol"], st["tf"])
            if len(bars) > st["seen"]:
                new = bars[st["seen"]:]
                st["det"].feed(bars)
                prev_close = (float(bars[st["seen"] - 1][4])
                              if st["seen"] > 0 else None)
                log_touches(st["det"], new, prev_close)
                settle_paper(new)
                st["seen"] = len(bars)
            st["done"] = st["ex"].replay_done(st["symbol"], st["tf"])
        return {
            "fig": make_fig(st, int(window)),
            "stats": make_stats(st, frozen=FROZEN),
            "levels_df": levels_to_df(st),
            "trades_df": trades_to_df(),
            "panel": make_panel(st),
            "trade_status": PAPER["last_msg"],
            "calc_preview": calc_text(PAPER["equity"], PAPER["risk_pct"],
                                      PAPER.get("draft") or {}),
        }


def tick_worker():
    """Background thread: once per second, refresh _LAST_SNAPSHOT only."""
    global _LAST_SNAPSHOT
    while True:
        _time.sleep(1.0)
        try:
            snap = step_once()
        except Exception as exc:  # noqa: BLE001
            snap = {"stats": f"tick error: {exc}", "panel": "—"}
        if snap is None:
            snap = {"stats": STATUS_MSG, "panel": "—"}
        with _SNAPSHOT_LOCK:
            _LAST_SNAPSHOT = snap


# ================================================================ Taipy callbacks
def _dummy_long():
    """invoke_long_callback keeps this running; we just block forever."""
    while True:
        _time.sleep(3600)


def _push_to_state(state: State):
    """Called every period ms by Taipy, in session context."""
    with _SNAPSHOT_LOCK:
        snap = _LAST_SNAPSHOT
    if snap is None:
        return
    if "fig" in snap and not FROZEN:
        state.fig = snap["fig"]
    if "stats" in snap:
        state.stats = snap["stats"]
    if "levels_df" in snap:
        state.levels_df = snap["levels_df"]
    if "trades_df" in snap:
        state.trades_df = snap["trades_df"]
    if "panel" in snap:
        state.panel = snap["panel"]
    if "trade_status" in snap:
        state.trade_status = snap["trade_status"]
    if "calc_preview" in snap:
        state.calc_preview = snap["calc_preview"]


def on_init(state: State):
    """Fires once per connected client."""
    invoke_long_callback(
        state,
        _dummy_long,
        [],
        _push_to_state,
        [],
        period=1000,
    )


def on_change(state: State, var_name: str, var_value):
    """Global change handler for every bound input."""
    g = globals()
    if var_name in ("coin", "tf", "start_date", "end_date", "speed", "window",
                    "deposit", "risk_pct", "direction",
                    "entry_in", "stop_in", "tp_in"):
        g[var_name] = var_value  # sync to module globals read by tick_worker
    if var_name in ("entry_in", "stop_in", "tp_in", "direction"):
        PAPER["draft"] = read_draft(state.direction, state.entry_in,
                                    state.stop_in, state.tp_in) or {}
        state.calc_preview = calc_text(PAPER["equity"], PAPER["risk_pct"],
                                       PAPER["draft"])
    elif var_name in ("deposit", "risk_pct"):
        state.calc_preview = calc_text(PAPER["equity"], PAPER["risk_pct"],
                                       PAPER.get("draft") or {})


def on_pause(state: State):
    with _STATE_LOCK:
        if STATE is not None:
            STATE["running"] = not STATE["running"]


def on_reset(state: State):
    global STATE
    with _STATE_LOCK:
        STATE = None


def on_freeze(state: State):
    global FROZEN
    FROZEN = not FROZEN
    state.frozen_label = ("▶ Продолжить график" if FROZEN
                          else "⏸ Заморозить график")


def on_open(state: State):
    global STATE
    if STATE is None:
        state.trade_status = "сначала дождись загрузки данных"
        return
    if PAPER["open"]:
        state.trade_status = "сделка уже открыта — сначала закрой её"
        return
    try:
        PAPER["deposit"] = float(state.deposit or PAPER["deposit"])
        PAPER["risk_pct"] = float(state.risk_pct
                                  if state.risk_pct not in (None, "") else 1.0)
    except (TypeError, ValueError):
        state.trade_status = "⚠ депозит и риск должны быть числами"
        return
    draft = read_draft(state.direction, state.entry_in,
                       state.stop_in, state.tp_in)
    if not draft:
        state.trade_status = "⚠ заполни вход и стоп"
        return
    try:
        c = calc_trade(PAPER["equity"], PAPER["risk_pct"], draft["side"],
                       draft["entry"], draft["sl"], draft["tp"])
    except ValueError as exc:
        state.trade_status = f"⚠ {exc}"
        return
    with _STATE_LOCK:
        open_ts = STATE["ex"].now_ms() if STATE is not None else 0
    PAPER["open"] = dict(c, open_ts=open_ts)
    state.trade_status = (f"открыта {'LONG' if c['side'] > 0 else 'SHORT'} "
                          f"{c['size']:.6f} по {c['entry']}, стоп {c['sl']}, "
                          f"риск {c['risk_usd']:.2f} USDT")


def on_close(state: State):
    if not PAPER["open"] or STATE is None:
        state.trade_status = "нет открытой сделки"
        return
    with _STATE_LOCK:
        bars = STATE["ex"].fetch_ohlcv_live(STATE["symbol"], STATE["tf"])
        now_ms = STATE["ex"].now_ms()
    px = float(bars[-1][4]) if bars else PAPER["open"]["entry"]
    close_paper(px, now_ms, "MANUAL")
    state.trade_status = PAPER["last_msg"]


def on_reset_acct(state: State):
    try:
        PAPER["deposit"] = float(state.deposit or 10000.0)
        PAPER["risk_pct"] = float(state.risk_pct
                                  if state.risk_pct not in (None, "") else 1.0)
    except (TypeError, ValueError):
        state.trade_status = "⚠ депозит и риск должны быть числами"
        return
    PAPER["equity"] = PAPER["deposit"]
    PAPER["open"] = None
    PAPER["closed"] = []
    PAPER["last_msg"] = (f"счёт сброшен: {PAPER['equity']:.2f} USDT, "
                         f"риск {PAPER['risk_pct']}%")
    state.trade_status = PAPER["last_msg"]


# ================================================================ page (tgb)
with tgb.Page() as page:
    with tgb.layout("1 320px"):
        # Plotly figure must be bound via `figure=`, not as data
        tgb.chart(figure="{fig}", height="85vh")

        with tgb.part():
            tgb.text("Данные", mode="h4")
            tgb.input("{coin}", label="Монета")
            tgb.selector("{tf}", lov="{tf_list}", label="Таймфрейм",
                         dropdown=True)
            tgb.date("{start_date}", label="Начало (UTC)")
            tgb.date("{end_date}", label="Конец (UTC)")
            tgb.slider("{speed}", min=1, max=20,
                       label="Скорость (баров/тик)")
            tgb.selector("{window}", lov="{window_list}",
                         label="Окно (баров)", dropdown=True)
            with tgb.part():
                tgb.button("Пауза / Продолжить", on_action=on_pause)
                tgb.button("Заново", on_action=on_reset)
                tgb.button("{frozen_label}", on_action=on_freeze)
            tgb.text("{stats}", mode="pre")
            tgb.text("смена монеты / таймфрейма / дат автоматически догружает "
                     "данные: сначала кэш data/ohlcv/, недостающее — с Binance.")

            tgb.text("Виртуальная торговля", mode="h4")
            tgb.number("{deposit}", label="Депозит, USDT")
            tgb.number("{risk_pct}", label="Риск на сделку, %")
            tgb.button("Сбросить счёт", on_action=on_reset_acct)
            tgb.selector("{direction}", lov="{direction_list}",
                         label="Направление", dropdown=True)
            tgb.input("{entry_in}", label="Вход")
            tgb.input("{stop_in}", label="Стоп")
            tgb.input("{tp_in}", label="Тейк (необяз.)")
            with tgb.part():
                tgb.button("Открыть сделку", on_action=on_open)
                tgb.button("Закрыть по рынку", on_action=on_close)
            tgb.text("{calc_preview}", mode="pre")
            tgb.text("{trade_status}", mode="pre")
            tgb.text("{panel}", mode="pre")

    tgb.text("Сделки", mode="h3")
    tgb.table("{trades_df}")

    tgb.text("Уровни: откуда взялся каждый "
             "(пивот → подтверждение → касания)", mode="h3")
    tgb.table("{levels_df}")


# ================================================================ main
def main(argv=None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=5000)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args(argv)

    threading.Thread(target=tick_worker, daemon=True).start()

    gui = Gui(page=page)
    # Taipy auto-discovers module-level on_init / on_change / on_action
    # handlers by name, so we don't need to pass them explicitly.
    print(f"open http://{args.host}:{args.port}/  (Ctrl+C to stop)")
    gui.run(
        host=args.host, port=args.port,
        use_reloader=False,       # the tick thread must not be started twice
        run_browser=False,
        title="simccxt live levels (taipy)",
        async_mode="threading",
        server_config={"socketio": {"ping_interval": 1}},
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())