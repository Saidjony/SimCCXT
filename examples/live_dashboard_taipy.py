#!/usr/bin/env python3
"""
live_dashboard_taipy.py
=======================
Taipy GUI + Plotly rewrite of live_dashboard.py — no Dash, no dcc.

Same core as the Dash version (untouched):
* simccxt replay clock (ex.step / fetch_ohlcv_live / replay_done)
* levels_demo.LiveLevels — online swing-level detection, no look-ahead
* market_data disk cache data/ohlcv/{SYM}_{tf}.csv + Binance download
  for missing bars + deterministic synthetic fallback when offline
* virtual (paper) trading with risk-based position sizing

What changed (Dash -> Taipy):
* dcc.Interval      -> background thread ticking once per second
* callbacks+States  -> module-level variables + on_change/on_action
* live push         -> Gui.broadcast_changes() (Taipy 3.0+)
* dash_table        -> Taipy table bound to DataFrames
* zoom/follow       -> Plotly uirevision="live" keeps the viewport while
                       the figure streams; "Заморозить" pauses only the
                       figure updates, the sim keeps running

Layout (same as the Dash original):
  left column  = chart, full width / height
  right column = controls (320px): coin/tf/dates/speed/window, pause,
                 virtual trading, live panel
  below        = closed-trades table + levels table

Threading model:
  The Rust engine behind simccxt.binance() is single-threaded by design.
  All access to STATE["ex"] happens ONLY in the tick thread.  UI callbacks
  (on_open / on_close) never touch STATE["ex"] directly — they use the
  LAST_PRICE / LAST_TIME_MS snapshots published by the tick thread.

Run:
    pip install "taipy>=3.0" plotly
    .venv/bin/python examples/live_dashboard_taipy.py [--port 8050]
    open http://127.0.0.1:8050/
"""

import argparse
import datetime as dt
import os
import sys
import threading
import time as _time

import numpy as np
import pandas as pd

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import levels_demo as LD
import market_data as MD
import simccxt

try:
    from taipy.gui import Gui, State, notify
except ImportError:
    sys.exit('pip install "taipy>=3.0" plotly')


TFS = {"1m": 60_000, "5m": 300_000, "15m": 900_000, "1h": 3_600_000}
N_BARS = 1200
ZONE = 0.001
MAX_BARS = 200_000


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


# ------------------------------------------------------------------ session
STATE = None
_STATE_LOCK = threading.RLock()

# Snapshots published by the tick thread, consumed by UI callbacks.
# This avoids touching STATE["ex"] (Rust, single-threaded) from callbacks.
LAST_PRICE = None
LAST_TIME_MS = None

STATUS_MSG = ("старт: синтетика, выбери даты — данные догрузятся "
              "с Binance в кэш data/ohlcv/")


def new_session(symbol: str, tf: str, df: pd.DataFrame, source: str,
                key: tuple) -> dict:
    """Build a simulator session from an OHLC dataframe (any origin)."""
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
    """(symbol, tf, start_ms, end_exclusive_ms) from picker values."""
    if not start_date or not end_date:
        return None
    s = int(pd.Timestamp(start_date, tz="UTC").timestamp() * 1000)
    e = int((pd.Timestamp(end_date, tz="UTC") + pd.Timedelta(days=1)).timestamp() * 1000)
    return (symbol, tf, s, e)


def valid_symbol(symbol) -> bool:
    """Free-form coin input must look like BASE/QUOTE (e.g. DOGE/USDT)."""
    if not isinstance(symbol, str) or "/" not in symbol:
        return False
    base, _, quote = symbol.partition("/")
    return bool(base.strip()) and bool(quote.strip())


def ensure_session(symbol, tf, start_date, end_date):
    """Rebuild the session if the desired (coin, tf, dates) key changed."""
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
            STATE["key"] = key  # consume: don't retry every tick
        return False
    try:
        t0 = _time.time()
        df, st = MD.load_or_download(symbol, tf, s, e)
        if len(df) == 0:
            raise RuntimeError("пустой диапазон (нет баров)")
        STATE = new_session(symbol, tf, df, source="binance(cache)+dl", key=key)
        STATUS_MSG = (f"Binance: из кэша {st['cached']}, докачано {st['downloaded']} "
                      f"за {_time.time() - t0:.1f}с")
    except Exception as exc:  # offline / bad symbol / bad range -> synthetic
        df = synthetic(symbol, tf)
        df["ts_ms"] = to_ms(df["timestamp"])
        STATE = new_session(symbol, tf, df, source="synthetic-fallback", key=key)
        STATUS_MSG = f"загрузка не удалась ({exc}); показан синтетический ряд"
    return True


def log_touches(det, new_bars, prev_close, zone=ZONE):
    """Log touches for newly arrived bars (incl. the batch-boundary bar)."""
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


# ------------------------------------------------------------------ paper trading
PAPER = {"deposit": 10000.0, "risk_pct": 1.0, "equity": 10000.0,
         "open": None, "closed": [], "draft": {}, "last_msg": ""}


def calc_trade(equity, risk_pct, side, entry, sl, tp=None):
    """Position sizing. Returns dict or raises ValueError with a message."""
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
    """Auto-close the open paper trade on SL/TP touch (SL has priority)."""
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
    """Draft/open trade levels for chart drawing (or None)."""
    t = PAPER["open"]
    if t:
        return {"entry": t["entry"], "sl": t["sl"], "tp": t["tp"]}
    d = PAPER.get("draft") or {}
    if d.get("entry") and d.get("sl"):
        return {"entry": d["entry"], "sl": d["sl"], "tp": d.get("tp")}
    return None


# ------------------------------------------------------------------ UI variables
# (module-level => bound by name in the Taipy page; state.<var> = value
#  writes through to these globals, so the tick thread can read them)
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

# Зеркало контролов для tick-треда. ВВОД ПОЛЬЗОВАТЕЛЯ НЕ ОБНОВЛЯЕТ
# module-глобалы (доказано пробой) — тик читает ТОЛЬКО этот словарь.
CONTROLS = {"coin": coin, "tf": tf, "start_date": start_date,
            "end_date": end_date, "speed": speed, "window": window}

chart_cols = ["t", "bx", "by", "supx", "supy", "resx", "resy",
              "tx", "ty", "px", "py", "wx", "wy", "lx", "ly",
              "oex", "oey", "osx", "osy", "otx", "oty"]
chart_df = pd.DataFrame({c: pd.Series(dtype="float64") for c in chart_cols})
chart_layout = {"margin": {"l": 50, "r": 10, "t": 10, "b": 30},
                "uirevision": "live", "hovermode": "x unified",
                "showlegend": True, "xaxis": {"type": "date", "title": ""},
                "yaxis": {"title": ""}}
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


# ------------------------------------------------------------------ page
# Layout is copied verbatim from the working version:
#   columns=1 320px  ->  chart takes the free space, controls are 320px RIGHT
page = """
# simccxt live levels (taipy + plotly, без Dash)

<|layout|columns=1 320px|gap=12px|
<|{chart_df}|chart|x=t|x[1]=bx|y[1]=by|type[1]=scatter|mode[1]=lines|color[1]=black|name[1]=бары|x[2]=supx|y[2]=supy|type[2]=scatter|mode[2]=lines|color[2]=green|name[2]=support|x[3]=resx|y[3]=resy|type[3]=scatter|mode[3]=lines|color[3]=red|name[3]=resistance|x[4]=tx|y[4]=ty|type[4]=scatter|mode[4]=markers|color[4]=orange|name[4]=касания|x[5]=px|y[5]=py|type[5]=scatter|mode[5]=markers|color[5]=blue|name[5]=пивоты|x[6]=wx|y[6]=wy|type[6]=scatter|mode[6]=lines+markers|color[6]=green|name[6]=сделки+|x[7]=lx|y[7]=ly|type[7]=scatter|mode[7]=lines+markers|color[7]=red|name[7]=сделки-|x[8]=oex|y[8]=oey|type[8]=scatter|mode[8]=lines|color[8]=blue|name[8]=вход|x[9]=osx|y[9]=osy|type[9]=scatter|mode[9]=lines|color[9]=red|name[9]=стоп|x[10]=otx|y[10]=oty|type[10]=scatter|mode[10]=lines|color[10]=green|name[10]=тейк|layout={chart_layout}|height=85vh|>
<|part|
#### Данные
**Монета**
<|{coin}|input|change_delay=400|width=100%|>
**Таймфрейм**
<|{tf}|selector|lov={tf_list}|dropdown|width=100%|>
**Период, UTC**
<|{start_date}|date|> — <|{end_date}|date|>
**Скорость (баров/тик)**
<|{speed}|slider|min=1|max=20|step=1|width=100%|>
**Окно (баров)**
<|{window}|selector|lov={window_list}|dropdown|width=100%|>

<|Пауза / Продолжить|button|on_action=on_pause|>
<|Заново|button|on_action=on_reset|>
<|{frozen_label}|button|on_action=on_freeze|>

<|{stats}|>

*смена монеты / таймфрейма / дат автоматически догружает данные: сначала кэш `data/ohlcv/`, недостающее — с Binance.*

---
#### Виртуальная торговля
**Депозит, USDT**
<|{deposit}|input|type=number|change_delay=400|width=100%|>
**Риск на сделку, %**
<|{risk_pct}|input|type=number|change_delay=400|width=100%|>
<|Сбросить счёт|button|on_action=on_reset_acct|width=100%|>
**Направление**
<|{direction}|selector|lov={direction_list}|dropdown|width=100%|>
**Вход**
<|{entry_in}|input|change_delay=300|placeholder=цена входа|width=100%|>
**Стоп**
<|{stop_in}|input|change_delay=300|placeholder=стоп-лосс|width=100%|>
**Тейк (необяз.)**
<|{tp_in}|input|change_delay=300|placeholder=тейк-профит|width=100%|>

<|Открыть сделку|button|on_action=on_open|>
<|Закрыть по рынку|button|on_action=on_close|>

<|{calc_preview}|>
<|{trade_status}|>
<|{panel}|>
|>
|>

#### Сделки
<|{trades_df}|table|page_size=10|>

#### Уровни: откуда взялся каждый (пивот → подтверждение → касания)
<|{levels_df}|table|page_size=15|>
"""


# ------------------------------------------------------------------ helpers
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
    """Open-position panel (live P&L). Caller is the tick thread."""
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
    mfe = max(t["side"] * (hi - t["entry"]), t["side"] * (last - t["entry"]), 0.0) \
          * t["size"]
    mae = min(t["side"] * (lo - t["entry"]), t["side"] * (last - t["entry"]), 0.0) \
          * t["size"]
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
            f"bars {st['seen']} | levels {n_lv} | touches {n_t} | {flag} || {STATUS_MSG}")


def _hline(x0, x1, y, xs, ys):
    xs += [x0, x1, None]
    ys += [y, y, None]


def build_chart_df(sess: dict, window: int, draft: dict) -> pd.DataFrame:
    """DataFrame для data-driven chart Taipy (штатный путь, не figure=).

    OHLC-бары рисуются сегментами вручную (вертикаль low-high + тики
    open/close): тип ohlc в Taipy не имеет маппинга осей и не доезжает
    (объект go.Figure приезжает null — проверено).
    Все x — миллисекунды float (ось type=date в layout), пропуски — NaN.
    """
    ex = sess["ex"]
    bars = np.array(ex.fetch_ohlcv_live(sess["symbol"], sess["tf"]),
                    dtype=float)
    cols = ["t", "bx", "by", "supx", "supy", "resx", "resy",
            "tx", "ty", "px", "py", "wx", "wy", "lx", "ly",
            "oex", "oey", "osx", "osy", "otx", "oty"]
    if not len(bars):
        return pd.DataFrame({c: pd.Series(dtype="float64") for c in cols})
    seg = bars[max(0, len(bars) - window):]
    t = seg[:, 0]
    dx = float(np.median(np.diff(t))) * 0.3 if len(t) > 1 else 60_000.0
    bx, by = [], []
    for i in range(len(seg)):
        x, o, h, lo, c = seg[i, 0], seg[i, 1], seg[i, 2], seg[i, 3], seg[i, 4]
        bx += [x, x, None, x - dx, x, None, x, x + dx, None]
        by += [lo, h, None, o, o, None, c, c, None]
    t0 = float(t[0])
    supx, supy, resx, resy = [], [], [], []
    for lv in sess["det"].levels:
        xs, ys = (supx, supy) if lv["direction"] == "support" else (resx, resy)
        _hline(t0, float(t[-1]), lv["price"], xs, ys)
    tx = [float(tc["ts"]) for lv in sess["det"].levels for tc in lv["touches"]
          if tc["ts"] >= t0]
    ty = [float(tc["price"]) for lv in sess["det"].levels for tc in lv["touches"]
          if tc["ts"] >= t0]
    px = [float(lv["pivot_ts"]) for lv in sess["det"].levels
          if lv["pivot_ts"] >= t0]
    py = [float(lv["price"]) for lv in sess["det"].levels
          if lv["pivot_ts"] >= t0]
    wx, wy, lx, ly = [], [], [], []
    for c in PAPER["closed"][-30:]:
        if c["open_ts"] < t0:
            continue
        xs, ys = (wx, wy) if c["pnl"] >= 0 else (lx, ly)
        xs += [float(c["open_ts"]), float(c["close_ts"]), None]
        ys += [float(c["entry"]), float(c["exit"]), None]
    oex, oey, osx, osy, otx, oty = [], [], [], [], [], []
    ov = None
    if PAPER["open"]:
        tr = PAPER["open"]
        ov = {"entry": tr["entry"], "sl": tr["sl"], "tp": tr["tp"]}
    elif draft.get("entry") and draft.get("sl"):
        ov = draft
    if ov:
        for xs, ys, v in ((oex, oey, ov["entry"]), (osx, osy, ov["sl"])):
            _hline(t0, float(t[-1]), float(v), xs, ys)
        if ov.get("tp"):
            _hline(t0, float(t[-1]), float(ov["tp"]), otx, oty)
    data = {"t": t, "bx": bx, "by": by, "supx": supx, "supy": supy,
            "resx": resx, "resy": resy, "tx": tx, "ty": ty, "px": px, "py": py,
            "wx": wx, "wy": wy, "lx": lx, "ly": ly,
            "oex": oex, "oey": oey, "osx": osx, "osy": osy,
            "otx": otx, "oty": oty}
    n = max(len(v) for v in data.values())
    out = {}
    for k, v in data.items():
        v = list(v) + [None] * (n - len(v))
        out[k] = v
    return pd.DataFrame(out, dtype="float64")


# ------------------------------------------------------------------ tick engine
_LAST_TRADE_MSG = ""  # last trade_status pushed by the tick thread


def step_once():
    """Advance the sim by `speed` bars; return the fresh UI snapshot."""
    global STATE, LAST_PRICE, LAST_TIME_MS, _LAST_TRADE_MSG
    c = dict(CONTROLS)   # снимок контролов (глобалы НЕ обновляются вводом!)
    with _STATE_LOCK:
        rebuilt = ensure_session(c["coin"], c["tf"], c["start_date"], c["end_date"])
        if STATE is None:
            return None
        if rebuilt and PAPER["open"]:
            PAPER["open"] = None
            PAPER["last_msg"] = "открытая сделка отменена (смена инструмента/дат)"
        st = STATE
        st["speed"] = int(c["speed"])
        if st["running"] and not st["done"]:
            st["ex"].step(int(c["speed"]))
            bars = st["ex"].fetch_ohlcv_live(st["symbol"], st["tf"])
            if len(bars) > st["seen"]:
                new = bars[st["seen"]:]
                st["det"].feed(bars)
                prev_close = float(bars[st["seen"] - 1][4]) if st["seen"] > 0 else None
                log_touches(st["det"], new, prev_close)
                settle_paper(new)
                st["seen"] = len(bars)
            st["done"] = st["ex"].replay_done(st["symbol"], st["tf"])
        # publish snapshots for UI callbacks (avoid touching ex there)
        bars = st["ex"].fetch_ohlcv_live(st["symbol"], st["tf"])
        if len(bars):
            LAST_PRICE = float(bars[-1][4])
            LAST_TIME_MS = int(bars[-1][0])
        snap = {"chart_df": build_chart_df(st, int(c["window"]),
                                               PAPER.get("draft") or {}),
                "stats": make_stats(st, frozen=FROZEN),
                "levels_df": levels_to_df(st),
                "trades_df": trades_to_df(),
                "panel": make_panel(st),
                "calc_preview": calc_text(PAPER["equity"], PAPER["risk_pct"],
                                          PAPER.get("draft") or {})}
        # push trade_status only when the auto-close message actually changed,
        # so user-action messages ("⚠ заполни вход и стоп") are not wiped
        if PAPER["last_msg"] != _LAST_TRADE_MSG:
            snap["trade_status"] = PAPER["last_msg"]
            _LAST_TRADE_MSG = PAPER["last_msg"]
        return snap


GUI = None  # set in main(); the tick thread pushes updates through it


def _broadcast(**pairs):
    """Push updated variables to every connected browser (Taipy 3.0+)."""
    if GUI is None:
        return
    try:
        GUI.broadcast_changes(**pairs)
    except AttributeError:  # very old taipy: per-variable fallback
        for k, v in pairs.items():
            try:
                GUI.broadcast_change(k, v)
            except Exception:
                pass
    except Exception:
        pass


def tick_loop():
    """1 tick = 1 second of wall time (the old dcc.Interval, as a thread)."""
    while True:
        _time.sleep(1.0)
        try:
            snap = step_once()
        except Exception as exc:  # never let the thread die
            _broadcast(stats=f"tick error: {exc}")
            continue
        if snap is None:  # e.g. invalid coin text while typing
            _broadcast(stats=STATUS_MSG, panel="—")
            continue
        if FROZEN:
            # frozen view: sim keeps running, figure stays untouched
            snap.pop("chart_df")
        _broadcast(**snap)


# ------------------------------------------------------------------ UI callbacks
def on_change(state: State, var_name: str, var_value):
    if var_name in ("coin", "tf", "start_date", "end_date", "speed", "window"):
        # tick-тред видит ввод ТОЛЬКО через это зеркало
        CONTROLS[var_name] = var_value
    if var_name in ("entry_in", "stop_in", "tp_in", "direction"):
        PAPER["draft"] = read_draft(state.direction, state.entry_in,
                                    state.stop_in, state.tp_in) or {}
        state.calc_preview = calc_text(PAPER["equity"], PAPER["risk_pct"],
                                       PAPER["draft"])
    elif var_name in ("deposit", "risk_pct"):
        state.calc_preview = calc_text(PAPER["equity"], PAPER["risk_pct"],
                                       PAPER.get("draft") or {})
    # coin/tf/dates/speed/window тик читает из CONTROLS-зеркала (см. выше).


def on_pause(state: State):
    with _STATE_LOCK:
        if STATE is not None:
            STATE["running"] = not STATE["running"]


def on_reset(state: State):
    # drop the session; the next tick rebuilds it from disk cache (fast)
    global STATE
    with _STATE_LOCK:
        STATE = None


def on_freeze(state: State):
    global FROZEN
    FROZEN = not FROZEN
    state.frozen_label = ("▶ Продолжить график" if FROZEN
                          else "⏸ Заморозить график")


def on_open(state: State):
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
    draft = read_draft(state.direction, state.entry_in, state.stop_in, state.tp_in)
    if not draft:
        state.trade_status = "⚠ заполни вход и стоп"
        return
    try:
        c = calc_trade(PAPER["equity"], PAPER["risk_pct"], draft["side"],
                       draft["entry"], draft["sl"], draft["tp"])
    except ValueError as exc:
        state.trade_status = f"⚠ {exc}"
        return
    # snapshot from the tick thread — no direct STATE["ex"] access here
    open_ts = LAST_TIME_MS if LAST_TIME_MS is not None else 0
    PAPER["open"] = dict(c, open_ts=open_ts)
    state.trade_status = (f"открыта {'LONG' if c['side'] > 0 else 'SHORT'} "
                          f"{c['size']:.6f} по {c['entry']}, стоп {c['sl']}, "
                          f"риск {c['risk_usd']:.2f} USDT")
    try:
        notify(state, "success", "сделка открыта")
    except Exception:
        pass


def on_close(state: State):
    if not PAPER["open"] or STATE is None:
        state.trade_status = "нет открытой сделки"
        return
    # snapshot from the tick thread — no direct STATE["ex"] access here
    px = LAST_PRICE if LAST_PRICE is not None else PAPER["open"]["entry"]
    now_ms = LAST_TIME_MS if LAST_TIME_MS is not None else 0
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


# ------------------------------------------------------------------ app
# Gui конструируется на уровне модуля: Taipy резолвит переменные страницы
# по модулю вызова — внутри main() все переменные становятся "not available".
GUI = Gui(page=page)


def main(argv=None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8050)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args(argv)

    threading.Thread(target=tick_loop, daemon=True).start()
    print(f"open http://{args.host}:{args.port}/  (Ctrl+C to stop)")
    # use_reloader=False: the tick thread must not be started twice
    GUI.run(host=args.host, port=args.port, use_reloader=False,
            run_browser=False, title="simccxt live levels (taipy)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
