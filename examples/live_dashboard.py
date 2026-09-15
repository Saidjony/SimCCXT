#!/usr/bin/env python3
"""
live_dashboard.py
=================
Real-time updating website for simccxt level research (Plotly Dash).

Pick a coin + timeframe + DATE RANGE; the simccxt simulator clock advances on
every tick and the Plotly chart updates live: new OHLC bars stream in from
disk-backed data, swing levels are detected online (no look-ahead), touches
are logged.

Data path ("not in laptop memory -> fetch from internet"):
  data/ohlcv/{SYM}_{tf}.csv cache is checked first; only missing bars of the
  selected [start, end) range are downloaded from Binance public klines.
  No internet / bad range -> deterministic synthetic fallback (status line says).

  coin:       any BASE/QUOTE text input (e.g. DOGE/USDT), validated server-side
  timeframe:  1m / 5m / 15m / 1h  (simulator step = 1 bar)
  dates:      start/end (UTC); ranges > 200k bars are refused with a message
  speed:      simulator steps per tick (1 tick = 1 second wall time)
  window:     how many recent bars to display

Run:
    .venv/bin/python examples/live_dashboard.py [--port 8050]
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
import zones as ZN

import simccxt

TFS = {"1m": 60_000, "5m": 300_000, "15m": 900_000, "1h": 3_600_000}
N_BARS = 1200
ZONE = 0.001
# Per-tick render budget (the server is single-threaded, tick = 1 s):
# draw only the most-touched levels / latest touch markers, list more rows
# in the table than on the chart.
MAX_DRAW_LEVELS = 30
MAX_DRAW_TOUCHES = 500
MAX_TABLE_ROWS = 50
# Accumulation zones: draw the top few by touches (rectangles).
MAX_DRAW_ZONES = 5


# Display scale for the offline synthetic fallback ONLY (real data always
# wins when reachable). Approximate 2026 spot prices so e.g. BTC doesn't
# render at ~1.3 when Binance is unreachable.
REF_PRICE = {
    "BTC": 100000.0, "ETH": 3500.0, "SOL": 200.0, "BNB": 650.0,
    "XRP": 2.2, "DOGE": 0.20, "ADA": 0.80, "TRX": 0.27, "TON": 5.0,
    "AVAX": 30.0, "LINK": 18.0, "LTC": 100.0, "NEAR": 5.0, "DOT": 6.0,
}


def synthetic(symbol: str, tf: str, n: int = N_BARS,
              start_ms: int | None = None) -> pd.DataFrame:
    """Deterministic OHLC bars for ANY coin/timeframe (offline fallback).

    Price scale comes from REF_PRICE for known coins (hash-based otherwise);
    timestamps anchor at `start_ms` when given so the axis matches the
    requested range even without network.
    """
    import hashlib
    base = symbol.split("/")[0].upper() if "/" in symbol else symbol.upper()
    h = int(hashlib.md5(symbol.encode()).hexdigest()[:8], 16)
    s0 = REF_PRICE.get(base)
    if s0 is None:
        mag = 10 ** (h % 5)                      # 1 .. 10000 price magnitude
        s0 = float(mag) * (1.0 + (h % 100) / 100.0)
    seed = h % 10_000
    step_ms = TFS[tf]
    step_s = step_ms // 1000
    rng = np.random.default_rng(seed)
    t = np.arange(n) / (n / 4.0)                       # ~4 waves per load
    logp = (np.log(s0) + 0.02 * t
            + 0.05 * np.sin(2 * np.pi * t / 4.0)
            + 0.015 * np.sin(2 * np.pi * t * 3.0)
            + np.cumsum(rng.normal(0.0, 0.0006 * (step_s / 60.0) ** 0.5, n)))
    close = np.exp(logp)
    rng2 = np.random.default_rng(seed + 1)
    half = 0.0003 + 0.0012 * np.abs(rng2.normal(0.0, 1.0, n))
    open_ = np.concatenate(([close[0]], close[:-1]))
    freq = {"1m": "min", "5m": "5min", "15m": "15min", "1h": "h"}[tf]
    if start_ms is not None:
        anchor = pd.to_datetime((int(start_ms) // step_ms) * step_ms,
                                unit="ms", utc=True)
    else:
        anchor = pd.Timestamp("2026-05-01", tz="UTC")
    ts = pd.date_range(anchor, periods=n, freq=freq, tz="UTC")
    return pd.DataFrame({
        "timestamp": ts, "open": open_,
        "high": np.maximum(open_, close) * (1.0 + half),
        "low": np.minimum(open_, close) * (1.0 - half),
        "close": close, "volume": rng.lognormal(0.0, 0.4, n) * 2.0,
    })


def to_ms(ts: pd.Series) -> np.ndarray:
    naive = ts.dt.tz_convert("UTC").dt.tz_localize(None)
    return (naive.astype("datetime64[ns]").astype("int64") // 10**6).to_numpy()


def _local_tz():
    """OS local timezone (whatever the machine is configured with)."""
    return dt.datetime.now().astimezone().tzinfo


def ms_local(ms):
    """Epoch ms -> datetimes in OS local time (axis, tables, stats)."""
    return pd.to_datetime(ms, unit="ms", utc=True).tz_convert(_local_tz())


# ------------------------------------------------------------------ session
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
            "speed": 2, "window": 300,
            "follow": True, "last_zoom": None,
            "zones": [], "strong": set()}


STATUS_MSG = "старт: синтетика, выбери даты — данные догрузятся с Binance в кэш data/ohlcv/"
MAX_BARS = 200_000

# Background Binance downloads.
# The web server is single-threaded (the Rust engine requires it), so a
# synchronous download inside a tick callback would freeze the whole page.
# The worker thread below only moves plain data (urllib + pandas + cache
# file) and never touches the `Exchange` object — the session itself is
# always (re)built on the server thread via new_session().
DL = {"key": None, "thread": None, "stats": None, "error": None}
DL_TIMEOUT = 20  # seconds per Binance page when downloading for the dashboard


def _download_worker(symbol, tf, s, e, key):
    """Thread body: download missing bars into the disk cache (no Exchange)."""
    try:
        _, stats = MD.load_or_download(symbol, tf, s, e, timeout=DL_TIMEOUT)
        DL["stats"] = stats
    except Exception as exc:  # noqa: BLE001 — reported via status line
        DL["error"] = str(exc)


def _start_download(symbol, tf, s, e, key) -> bool:
    """Spawn a background download for key; False if a worker is busy."""
    t = DL["thread"]
    if t is not None and t.is_alive():
        return False
    DL.update(key=key, thread=threading.Thread(
        target=_download_worker, args=(symbol, tf, s, e, key), daemon=True),
        stats=None, error=None)
    DL["thread"].start()
    return True


def _poll_download():
    """Adopt a finished background download (runs on the server thread)."""
    global STATE, STATUS_MSG
    t = DL["thread"]
    if t is None or t.is_alive() or DL["key"] is None:
        return
    t.join()
    key, stats, error = DL["key"], DL["stats"], DL["error"]
    DL.update(key=None, thread=None, stats=None, error=None)
    if STATE is None or key != STATE.get("key"):
        return  # user already moved to other dates/coin; cache still useful
    symbol, tf, s, e = key
    if error is not None:
        STATUS_MSG = f"Binance недоступен ({error}); показана синтетика"
        return
    try:
        window, covered = MD.read_window(symbol, tf, s, e)
    except Exception as exc:  # noqa: BLE001 — corrupt cache etc.
        STATUS_MSG = f"кэш не читается ({exc}); показана синтетика"
        return
    if len(window) == 0:
        STATUS_MSG = "Binance вернул пустой диапазон; показана синтетика"
        return
    STATE = new_session(symbol, tf, window,
                        source="binance(cache)+dl", key=key)
    n_dl = stats.get("downloaded", 0) if stats else 0
    STATUS_MSG = (f"Binance: докачано {n_dl}, всего {len(window)} баров"
                  + ("" if covered else " (покрытие частичное)"))


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
    """Rebuild the session if the desired (coin, tf, dates) key changed.

    Never blocks on the network (the server thread must stay responsive):
      * disk cache covers the whole range -> session from cache, instantly;
      * otherwise -> synthetic session instantly + missing bars download
        in a background thread; the next ticks adopt it via _poll_download().
    """
    global STATE, STATUS_MSG
    _poll_download()
    if not valid_symbol(symbol):
        # don't consume the key: user may still be typing
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
        window, covered = MD.read_window(symbol, tf, s, e)
    except Exception as exc:  # noqa: BLE001 — corrupt cache etc.
        window, covered = None, False
        STATUS_MSG = f"кэш не читается ({exc}); показана синтетика"
    if covered and len(window):
        STATE = new_session(symbol, tf, window, source="cache", key=key)
        STATUS_MSG = f"кэш: {len(window)} баров за диапазон, без сети"
        return True
    df = synthetic(symbol, tf, start_ms=s)
    df["ts_ms"] = to_ms(df["timestamp"])
    STATE = new_session(symbol, tf, df, source="synthetic", key=key)
    if _start_download(symbol, tf, s, e, key):
        STATUS_MSG = ("показываю синтетику; недостающие бары качаются "
                      "с Binance в фоне…")
    else:
        STATUS_MSG = "показываю синтетику; докачка уже идёт, жду её конца…"
    return True


STATE = None


def log_touches(det, new_bars, prev_close, zone=ZONE):
    """Log touches for newly arrived bars.

    `prev_close` is the close of the bar right before `new_bars[0]` in the
    full series, so the first bar of every batch is checked too (otherwise
    entries on batch boundaries are silently missed).
    """
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
# Virtual trading: deposit + risk-per-trade are set once; everything else
# (risk $, stop distance, position size, live P&L) is computed automatically.
PAPER = {"deposit": 10000.0, "risk_pct": 1.0, "equity": 10000.0,
         "open": None, "pending": None, "closed": [], "draft": {},
         "last_msg": ""}


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
           "notional": size * entry, "lev": size * entry / equity if equity else 0.0}
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
    """Fill pending limit orders, then auto-close the open trade on SL/TP.

    Limit fill: LONG fills when a bar's low touches the entry (buy below
    market), SHORT when a bar's high touches it. SL has priority on closes.
    """
    p = PAPER.get("pending")
    if p is not None:
        for b in new_bars:
            ts, hi, lo = int(b[0]), b[2], b[3]
            filled = (lo <= p["entry"]) if p["side"] > 0 else (hi >= p["entry"])
            if filled:
                PAPER["open"] = dict(p, open_ts=ts)
                PAPER["pending"] = None
                PAPER["last_msg"] = (
                    f"лимитная {'LONG' if p['side'] > 0 else 'SHORT'} "
                    f"сработала по {p['entry']:,.4f}")
                break
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
    """Draft/open/pending trade levels for chart drawing (or None)."""
    t = PAPER["open"]
    if t:
        return {"kind": "open", "entry": t["entry"], "sl": t["sl"],
                "tp": t["tp"]}
    d = PAPER.get("draft") or {}
    if d.get("entry") and d.get("sl"):
        return {"kind": "draft", "entry": d["entry"], "sl": d["sl"],
                "tp": d.get("tp")}
    p = PAPER.get("pending")
    if p:
        return {"kind": "pending", "entry": p["entry"], "sl": p["sl"],
                "tp": p.get("tp")}
    return None


# ------------------------------------------------------------------ app
def build_app():
    import dash
    from dash import dcc, html, Input, Output, State, dash_table
    import plotly.graph_objects as go

    today = dt.datetime.now(dt.timezone.utc).date()
    default_end = today.isoformat()
    default_start = (today - dt.timedelta(days=2)).isoformat()

    app = dash.Dash(__name__, title="simccxt live levels")

    app.layout = html.Div([
        html.Div([
            html.Div([
                dcc.Graph(id="chart", style={"height": "88vh"},
                          config={"responsive": True, "displaylogo": False}),
            ], style={"flex": "1", "minWidth": "0"}),
            html.Div([
                html.H4("Данные", style={"margin": "0 0 4px", "fontSize": "14px"}),
                html.Label("Монета"),
                dcc.Input(id="coin", value="BTC/USDT", placeholder="DOGE/USDT",
                          debounce=True, style={"width": "100%"}),
                html.Label("Таймфрейм", style={"marginTop": "6px"}),
                dcc.Dropdown(id="tf", options=list(TFS), value="1m",
                             clearable=False, style={"width": "100%"}),
                html.Label("Период (UTC)", style={"marginTop": "6px"}),
                dcc.DatePickerRange(id="dates", start_date=default_start,
                                    end_date=default_end,
                                    display_format="YYYY-MM-DD"),
                html.Label("Скорость (баров/тик)", style={"marginTop": "6px"}),
                dcc.Slider(id="speed", min=1, max=20, step=1, value=2,
                           marks={1: "1", 5: "5", 10: "10", 20: "20"},
                           updatemode="drag"),
                html.Label("Окно (баров)", style={"marginTop": "6px"}),
                dcc.Dropdown(id="window", options=[100, 200, 300, 500, 1200],
                             value=300, clearable=False, style={"width": "100%"}),
                html.Div([
                    html.Button("Пауза / Продолжить", id="pause", n_clicks=0),
                    html.Button("Заново", id="reset", n_clicks=0),
                    html.Button("⏸ Заморозить график", id="freeze", n_clicks=0,
                                title="остановить обновление картинки"),
                ], style={"display": "flex", "gap": "6px", "flexWrap": "wrap",
                          "marginTop": "8px"}),
                html.Div(id="stats",
                         style={"margin": "10px 0 0", "fontFamily": "monospace",
                                "fontSize": "12px", "whiteSpace": "pre-wrap"}),
                html.Div("смена монеты / таймфрейма / дат автоматически догружает "
                         "данные: сначала кэш data/ohlcv/, недостающее — с Binance.",
                         style={"fontSize": "11px", "color": "#555",
                                "marginTop": "6px"}),
                html.H4("Виртуальная торговля",
                        style={"margin": "12px 0 4px", "fontSize": "14px"}),
                html.Label("Депозит, USDT"),
                dcc.Input(id="deposit", type="number", value=10000, debounce=True,
                          style={"width": "100%"}),
                html.Label("Риск на сделку, %", style={"marginTop": "6px"}),
                dcc.Input(id="risk_pct", type="number", value=1.0, debounce=True,
                          style={"width": "100%"}),
                html.Button("Сбросить счёт", id="reset_acct", n_clicks=0,
                            style={"marginTop": "6px", "width": "100%"}),
                html.Label("Направление", style={"marginTop": "6px"}),
                dcc.Dropdown(id="direction", options=["long", "short"],
                             value="long", clearable=False, style={"width": "100%"}),
                html.Label("Вход", style={"marginTop": "6px"}),
                dcc.Input(id="entry_price", type="number", debounce=True,
                          placeholder="цена входа", style={"width": "100%"}),
                html.Label("Стоп", style={"marginTop": "6px"}),
                dcc.Input(id="stop_price", type="number", debounce=True,
                          placeholder="стоп-лосс", style={"width": "100%"}),
                html.Label("Тейк (необяз.)", style={"marginTop": "6px"}),
                dcc.Input(id="tp_price", type="number", debounce=True,
                          placeholder="тейк-профит", style={"width": "100%"}),
                html.Div([
                    html.Button("Открыть по рынку", id="open_btn", n_clicks=0),
                    html.Button("Лимитная", id="limit_btn", n_clicks=0),
                    html.Button("Закрыть по рынку", id="close_btn", n_clicks=0),
                    html.Button("Снять лимитную", id="cancel_limit_btn",
                                n_clicks=0),
                ], style={"display": "flex", "gap": "6px", "flexWrap": "wrap",
                           "marginTop": "8px"}),
                html.Div(id="calc_preview",
                         style={"marginTop": "6px", "fontFamily": "monospace",
                                "fontSize": "12px", "whiteSpace": "pre-wrap"}),
                html.Div(id="trade_status",
                         style={"marginTop": "4px", "fontSize": "12px",
                                "color": "#064", "whiteSpace": "pre-wrap"}),
                html.Div(id="open_panel",
                         style={"marginTop": "6px", "fontFamily": "monospace",
                                "fontSize": "12px", "background": "#f6f6f6",
                                "padding": "6px 10px", "whiteSpace": "pre-wrap"}),
            ], style={"width": "300px", "flexShrink": "0"}),
        ], style={"display": "flex", "gap": "12px", "alignItems": "flex-start"}),
        html.H3("Сделки", style={"fontSize": "15px", "marginTop": "14px"}),
        dash_table.DataTable(
            id="closed_table", columns=[
                {"name": "открыта", "id": "open_ts"},
                {"name": "закрыта", "id": "close_ts"},
                {"name": "напр.", "id": "dir"},
                {"name": "вход", "id": "entry"},
                {"name": "выход", "id": "exit"},
                {"name": "стоп", "id": "sl"},
                {"name": "размер", "id": "size"},
                {"name": "P&L, $", "id": "pnl"},
                {"name": "P&L, %", "id": "pnl_pct"},
                {"name": "причина", "id": "reason"},
                {"name": "счёт", "id": "equity"}],
            style_cell={"fontSize": "12px", "padding": "2px 8px",
                        "fontFamily": "monospace"},
            page_size=10, style_table={"overflowX": "auto"}),
        html.H3("Уровни: откуда взялся каждый (пивот → подтверждение → касания)",
                style={"fontSize": "15px", "marginTop": "14px"}),
        dash_table.DataTable(
            id="levels_table",
            columns=[{"name": "уровень", "id": "level"},
                     {"name": "тип", "id": "dir"},
                     {"name": "пивот-бар", "id": "pivot"},
                     {"name": "подтверждён", "id": "confirmed"},
                     {"name": "баров в окне", "id": "window"},
                     {"name": "касания", "id": "touches"}],
            style_cell={"fontSize": "12px", "padding": "2px 8px",
                        "fontFamily": "monospace"},
            page_size=15, sort_action="native",
            style_table={"overflowX": "auto"}),
        html.H3("Зоны накопления: кластеры близких уровней с массой касаний",
                style={"fontSize": "15px", "marginTop": "14px"}),
        dash_table.DataTable(
            id="zones_table",
            columns=[{"name": "зона", "id": "zone"},
                     {"name": "уровней", "id": "levels"},
                     {"name": "касаний", "id": "touches"},
                     {"name": "ширина %", "id": "width"},
                     {"name": "период", "id": "period"},
                     {"name": "объём x", "id": "vol"},
                     {"name": "статус", "id": "status"}],
            style_cell={"fontSize": "12px", "padding": "2px 8px",
                        "fontFamily": "monospace"},
            page_size=10, sort_action="native",
            style_table={"overflowX": "auto"}),
        dcc.Interval(id="tick", interval=1000, n_intervals=0),
    ], style={"padding": "12px"})

    @app.callback(Output("chart", "figure"), Output("stats", "children"),
                   Output("levels_table", "data"),
                   Output("zones_table", "data"),
                   Output("open_panel", "children"),
                  Output("closed_table", "data"),
                  Input("tick", "n_intervals"),
                  State("coin", "value"), State("tf", "value"),
                  State("speed", "value"), State("window", "value"),
                  State("dates", "start_date"), State("dates", "end_date"),
                  State("chart", "relayoutData"), State("freeze", "n_clicks"))
    def on_tick(_, symbol, tf, speed, window, start_date, end_date, zoom,
                freeze_clicks):
        global STATE
        rebuilt = ensure_session(symbol, tf, start_date, end_date)
        if STATE is None:
            # e.g. invalid coin text while typing: show the hint, draw nothing
            return {}, STATUS_MSG, [], "—", []
        if rebuilt and (PAPER["open"] or PAPER.get("pending")):
            PAPER["open"] = None
            PAPER["pending"] = None
            PAPER["last_msg"] = "сделки отменены (смена инструмента/дат)"
        frozen = ((freeze_clicks or 0) % 2 == 1) and not rebuilt
        st, ex, det = STATE, STATE["ex"], STATE["det"]
        # follow mode: fresh data → follow live; user zoom → hold the view
        # (double-click autosize → follow live again); rebuild → follow live
        if rebuilt:
            st["follow"] = True
        elif zoom != st.get("last_zoom"):
            st["follow"] = not (isinstance(zoom, dict)
                                and "xaxis.range[0]" in zoom
                                and "xaxis.autosize" not in zoom)
        st["last_zoom"] = zoom
        if st["running"] and not st["done"]:
            ex.step(int(speed))
            bars = ex.fetch_ohlcv_live(symbol, tf)
            if len(bars) > st["seen"]:
                new = bars[st["seen"]:]
                det.feed(bars)
                prev_close = float(bars[st["seen"] - 1][4]) if st["seen"] > 0 else None
                log_touches(det, new, prev_close)
                settle_paper(new)
                st["seen"] = len(bars)
                st["zones"] = ZN.find_zones(det.levels, bars, TFS[st["tf"]])
                st["strong"] = ZN.find_strong_levels(det.levels)
            st["done"] = ex.replay_done(symbol, tf)
        panel, table = make_paper(st)
        if frozen or (not st["follow"] and not rebuilt):
            # frozen/custom view: never touch the figure (zoom/pan fully
            # stable); the sim keeps running and stats/tables keep updating
            mode = "FROZEN" if frozen else "CUSTOM"
            return dash.no_update, make_stats(st, frozen=mode), \
                make_table_rows(st), make_zones_rows(st), panel, table
        fig = make_fig(st, window)
        return fig, make_stats(st), make_table_rows(st), make_zones_rows(st), \
            panel, table

    def fmt_ts(ms):
        return ms_local(ms).strftime("%m-%d %H:%M")

    def make_paper(st):
        """Open-position panel (live P&L) + closed-trades table rows."""
        t = PAPER["open"]
        if t is None:
            panel = (f"счёт {PAPER['equity']:.2f} USDT "
                     f"(депозит {PAPER['deposit']:.2f}, риск {PAPER['risk_pct']}%) — "
                     f"нет открытой сделки")
            p = PAPER.get("pending")
            if p:
                panel += (f" | лимитная "
                          f"{'LONG' if p['side'] > 0 else 'SHORT'} "
                          f"{p['size']:.6f} ждёт {p['entry']:,.4f} "
                          f"(стоп {p['sl']:,.4f})")
        else:
            bars = np.array(st["ex"].fetch_ohlcv_live(st["symbol"], st["tf"]))
            last = float(bars[-1, 4]) if len(bars) else t["entry"]
            pnl = t["side"] * (last - t["entry"]) * t["size"]
            # MFE/MAE over bars since the trade was opened
            ts = bars[:, 0].astype(np.int64) if len(bars) else np.array([])
            held = bars[ts >= t["open_ts"]] if len(bars) else bars
            hi = float(held[:, 2].max()) if len(held) else last
            lo = float(held[:, 3].min()) if len(held) else last
            mfe = max(t["side"] * (hi - t["entry"]), t["side"] * (last - t["entry"]), 0.0) \
                * t["size"]
            mae = min(t["side"] * (lo - t["entry"]), t["side"] * (last - t["entry"]), 0.0) \
                * t["size"]
            panel = (f"ОТКРЫТА {'LONG' if t['side'] > 0 else 'SHORT'} {t['size']:.6f} "
                     f"(~{t['notional']:.1f} USDT, плечо-экв. x{t['lev']:.2f}) | "
                     f"вход {t['entry']} | стоп {t['sl']} | "
                     f"P&L {pnl:+.2f} USDT ({pnl / t['notional'] * 100:+.3f}%) | "
                     f"MFE {mfe:+.2f} / MAE {mae:+.2f}")
        rows = []
        for c in reversed(PAPER["closed"][-50:]):
            rows.append({
                "open_ts": fmt_ts(c["open_ts"]), "close_ts": fmt_ts(c["close_ts"]),
                "dir": c["dir"], "entry": f"{c['entry']:,.4f}",
                "exit": f"{c['exit']:,.4f}", "sl": f"{c['sl']:,.4f}",
                "size": f"{c['size']:.6f}", "pnl": f"{c['pnl']:+.2f}",
                "pnl_pct": f"{c['pnl_pct']:+.3f}%", "reason": c["reason"],
                "equity": f"{c['equity']:.2f}"})
        return panel, rows

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

    @app.callback(Output("chart", "figure", allow_duplicate=True),
                  Output("calc_preview", "children"),
                  Input("entry_price", "value"), Input("stop_price", "value"),
                  Input("tp_price", "value"), Input("direction", "value"),
                  State("window", "value"), State("chart", "relayoutData"),
                  State("freeze", "n_clicks"),
                  prevent_initial_call=True)
    def on_draft(entry, stop, tp, direction, window, zoom, freeze_clicks):
        PAPER["draft"] = read_draft(direction, entry, stop, tp) or {}
        calc = calc_text(PAPER["equity"], PAPER["risk_pct"], PAPER["draft"])
        if STATE is None or ((freeze_clicks or 0) % 2 == 1):
            return dash.no_update, calc
        fig = make_fig(STATE, window or 300)
        if isinstance(zoom, dict) and "xaxis.range[0]" in zoom \
                and "xaxis.autosize" not in zoom:
            fig.update_xaxes(range=[zoom["xaxis.range[0]"], zoom["xaxis.range[1]"]],
                             autorange=False)
        return fig, calc

    @app.callback(Output("trade_status", "children", allow_duplicate=True),
                  Input("open_btn", "n_clicks"),
                  State("deposit", "value"), State("risk_pct", "value"),
                  State("direction", "value"), State("entry_price", "value"),
                  State("stop_price", "value"), State("tp_price", "value"),
                  prevent_initial_call=True)
    def on_open(_, deposit, risk_pct, direction, entry, stop, tp):
        if STATE is None:
            return "сначала дождись загрузки данных"
        if PAPER["open"]:
            return "сделка уже открыта — сначала закрой её"
        try:
            PAPER["deposit"] = float(deposit or PAPER["deposit"])
            PAPER["risk_pct"] = float(risk_pct if risk_pct not in (None, "") else 1.0)
        except (TypeError, ValueError):
            return "⚠ депозит и риск должны быть числами"
        draft = read_draft(direction, entry, stop, tp)
        if not draft:
            return "⚠ заполни вход и стоп"
        try:
            c = calc_trade(PAPER["equity"], PAPER["risk_pct"], draft["side"],
                           draft["entry"], draft["sl"], draft["tp"])
        except ValueError as exc:
            return f"⚠ {exc}"
        PAPER["open"] = dict(c, open_ts=STATE["ex"].now_ms())
        return (f"открыта {'LONG' if c['side'] > 0 else 'SHORT'} {c['size']:.6f} "
                f"по {c['entry']}, стоп {c['sl']}, риск {c['risk_usd']:.2f} USDT")

    @app.callback(Output("trade_status", "children", allow_duplicate=True),
                   Input("limit_btn", "n_clicks"),
                   State("deposit", "value"), State("risk_pct", "value"),
                   State("direction", "value"), State("entry_price", "value"),
                   State("stop_price", "value"), State("tp_price", "value"),
                   prevent_initial_call=True)
    def on_limit(_, deposit, risk_pct, direction, entry, stop, tp):
        """Place a pending limit order (fills when price touches entry)."""
        if STATE is None:
            return "сначала дождись загрузки данных"
        if PAPER["open"]:
            return "сделка уже открыта — сначала закрой её"
        if PAPER.get("pending"):
            return "лимитная уже стоит — сначала сними её"
        try:
            PAPER["deposit"] = float(deposit or PAPER["deposit"])
            PAPER["risk_pct"] = float(risk_pct if risk_pct not in (None, "") else 1.0)
        except (TypeError, ValueError):
            return "⚠ депозит и риск должны быть числами"
        draft = read_draft(direction, entry, stop, tp)
        if not draft:
            return "⚠ заполни вход и стоп"
        try:
            c = calc_trade(PAPER["equity"], PAPER["risk_pct"], draft["side"],
                           draft["entry"], draft["sl"], draft["tp"])
        except ValueError as exc:
            return f"⚠ {exc}"
        bars = STATE["ex"].fetch_ohlcv_live(STATE["symbol"], STATE["tf"])
        last = float(bars[-1][4]) if bars else draft["entry"]
        if c["side"] > 0 and c["entry"] >= last:
            return (f"⚠ лимитная покупка ставится НИЖЕ текущей цены "
                    f"(сейчас {last:,.4f}); для входа сразу жми "
                    f"«Открыть по рынку»")
        if c["side"] < 0 and c["entry"] <= last:
            return (f"⚠ лимитная продажа ставится ВЫШЕ текущей цены "
                    f"(сейчас {last:,.4f}); для входа сразу жми "
                    f"«Открыть по рынку»")
        PAPER["pending"] = dict(c, created_ts=STATE["ex"].now_ms())
        PAPER["draft"] = {}
        return (f"лимитная {'LONG' if c['side'] > 0 else 'SHORT'} "
                f"{c['size']:.6f} ждёт {c['entry']}, стоп {c['sl']}, "
                f"риск {c['risk_usd']:.2f} USDT")

    @app.callback(Output("trade_status", "children", allow_duplicate=True),
                   Input("cancel_limit_btn", "n_clicks"),
                   prevent_initial_call=True)
    def on_cancel_limit(_):
        if not PAPER.get("pending"):
            return "нет лимитной заявки"
        PAPER["pending"] = None
        PAPER["last_msg"] = "лимитная заявка снята"
        return PAPER["last_msg"]

    @app.callback(Output("trade_status", "children", allow_duplicate=True),
                   Input("close_btn", "n_clicks"),
                   prevent_initial_call=True)
    def on_close(_):
        t = PAPER["open"]
        if not t or STATE is None:
            return "нет открытой сделки"
        bars = STATE["ex"].fetch_ohlcv_live(STATE["symbol"], STATE["tf"])
        px = float(bars[-1][4]) if bars else t["entry"]
        close_paper(px, STATE["ex"].now_ms(), "MANUAL")
        return PAPER["last_msg"]

    @app.callback(Output("trade_status", "children", allow_duplicate=True),
                  Input("reset_acct", "n_clicks"),
                  State("deposit", "value"), State("risk_pct", "value"),
                  prevent_initial_call=True)
    def on_reset_acct(_, deposit, risk_pct):
        try:
            PAPER["deposit"] = float(deposit or 10000.0)
            PAPER["risk_pct"] = float(risk_pct if risk_pct not in (None, "") else 1.0)
        except (TypeError, ValueError):
            return "⚠ депозит и риск должны быть числами"
        PAPER["equity"] = PAPER["deposit"]
        PAPER["open"] = None
        PAPER["pending"] = None
        PAPER["closed"] = []
        PAPER["last_msg"] = (f"счёт сброшен: {PAPER['equity']:.2f} USDT, "
                             f"риск {PAPER['risk_pct']}%")
        return PAPER["last_msg"]

    @app.callback(Output("freeze", "children"),
                  Input("freeze", "n_clicks"))
    def on_freeze_label(n):
        return "▶ Продолжить график" if ((n or 0) % 2 == 1) else "⏸ Заморозить график"

    @app.callback(Output("tick", "disabled", allow_duplicate=True),
                  Input("pause", "n_clicks"), prevent_initial_call=True)
    def on_pause(_):
        if STATE is None:  # reset pressed, session not rebuilt yet
            return False
        STATE["running"] = not STATE["running"]
        return False  # keep ticking; callback no-ops while paused

    @app.callback(Output("tick", "disabled", allow_duplicate=True),
                  Input("reset", "n_clicks"),
                  prevent_initial_call=True)
    def on_reset(_):
        # drop the session; the next tick rebuilds it from disk cache (fast)
        global STATE
        STATE = None
        return False

    def make_stats(st, frozen=False):
        ex = st["ex"]
        n_lv = len(st["det"].levels)
        n_t = sum(len(lv["touches"]) for lv in st["det"].levels)
        n_z = len(st.get("zones") or [])
        n_s = len(st.get("strong") or [])
        now = ms_local(ex.now_ms()).strftime("%m-%d %H:%M")
        flag = "FINISHED" if st["done"] else ("RUNNING" if st["running"] else "PAUSED")
        if frozen and frozen is not True:
            flag += f" + {frozen}"
        elif frozen:
            flag += " + VIEW FROZEN"
        elif not st.get("follow", True):
            flag += " + VIEW: свой (дабл-клик — снова live)"
        r0, r1 = st["range"]
        rng = (ms_local(r0).strftime("%m-%d") + ".." +
               ms_local(r1).strftime("%m-%d"))
        return (f"{st['symbol']} {st['tf']} [{st['source']} {rng}] | sim-time {now} "
                f"(местн.) | "
                f"bars {st['seen']} | levels {n_lv} | touches {n_t} | "
                f"zones {n_z} | strong {n_s} | {flag} || {STATUS_MSG}")

    def make_zones_rows(st):
        rows = []
        for z in (st.get("zones") or [])[:MAX_TABLE_ROWS]:
            rows.append({
                "zone": f"{z['y0']:,.4f}–{z['y1']:,.4f}",
                "levels": z["n_levels"],
                "touches": z["n_touches"],
                "width": f"{z['width_pct'] * 100:.2f}%",
                "period": f"{fmt_ts(z['t0'])}..{fmt_ts(z['t1'])}",
                "vol": f"{z['vol_ratio']:.2f}x",
                "status": z["status"],
            })
        return rows

    def make_table_rows(st):
        rows = []
        levels = sorted(st["det"].levels,
                        key=lambda l: len(l["touches"]), reverse=True)
        if len(levels) > MAX_TABLE_ROWS:
            levels = levels[:MAX_TABLE_ROWS]
        for lv in levels:
            rows.append({
                "level": f"{lv['price']:,.4f}",
                "dir": "поддержка" if lv["direction"] == "support" else "сопротивление",
                "pivot": fmt_ts(lv["pivot_ts"]),
                "confirmed": fmt_ts(lv["confirmed_ts"]),
                "window": len(lv.get("confirming_bars") or []),
                "touches": len(lv["touches"]),
            })
        return rows

    def make_fig(st, window):
        import plotly.graph_objects as go
        ex = st["ex"]
        bars = np.array(ex.fetch_ohlcv_live(st["symbol"], st["tf"]))
        # Render budget: the tick runs every second on a single-threaded
        # server, so the figure must stay small. Only the most-touched
        # levels are drawn (the table below lists more); touches markers
        # are capped to the latest few hundred.
        draw_levels = sorted(st["det"].levels,
                             key=lambda l: len(l["touches"]),
                             reverse=True)[:MAX_DRAW_LEVELS]
        fig = go.Figure()
        if len(bars):
            seg = bars[max(0, len(bars) - window):]
            t = ms_local(seg[:, 0])
            fig.add_trace(go.Ohlc(x=t, open=seg[:, 1], high=seg[:, 2],
                                  low=seg[:, 3], close=seg[:, 4],
                                  name=f"{st['symbol']} (бары)",
                                  increasing_line_color="black",
                                  decreasing_line_color="black"))
            # accumulation zones (rectangles): price band + first..last touch.
            # Active zones stretch to the latest bar; strong (lonely +
            # respected) levels below get a thicker line.
            t_end = int(seg[-1, 0])
            for z in (st.get("zones") or [])[:MAX_DRAW_ZONES]:
                if z["t1"] < int(seg[0, 0]) or z["t0"] > t_end:
                    continue
                x1 = max(z["t1"], t_end) if z["status"] == "активна" else z["t1"]
                fig.add_shape(type="rect",
                              x0=ms_local(z["t0"]), x1=ms_local(x1),
                              y0=z["y0"], y1=z["y1"],
                              fillcolor="gold", opacity=0.12,
                              line=dict(color="gold", width=1,
                                        dash="dash"))
            strong = st.get("strong") or set()
            for lv in draw_levels:
                fig.add_hline(y=lv["price"],
                              line_color="green" if lv["direction"] == "support" else "red",
                              line_dash="dash",
                              line_width=2 if lv["price"] in strong else 1,
                              opacity=0.8)
            # pivot bars (diamonds) + confirming windows (shading):
            # dots show exactly which bars produced each level
            px, py, ptext = [], [], []
            for lv in draw_levels:
                cb = lv.get("confirming_bars") or []
                if cb and cb[-1] >= int(seg[0, 0]):
                    fig.add_vrect(x0=ms_local(cb[0]),
                                  x1=ms_local(cb[-1]),
                                  fillcolor="blue", opacity=0.07, line_width=0)
                if lv["pivot_ts"] >= int(seg[0, 0]):
                    px.append(ms_local(lv["pivot_ts"]))
                    py.append(lv["price"])
                    ptext.append(
                        f"уровень {lv['price']:,.4f} ({lv['direction']})<br>"
                        f"пивот-бар {ms_local(lv['pivot_ts'])}<br>"
                        f"подтверждён {ms_local(lv['confirmed_ts'])}<br>"
                        f"окно подтверждения: {len(cb)} баров<br>"
                        f"касаний: {len(lv['touches'])}")
            if px:
                fig.add_trace(go.Scatter(x=px, y=py, mode="markers",
                                         name="пивот-бары",
                                         marker=dict(symbol="diamond", size=10,
                                                     color="blue"),
                                         text=ptext, hoverinfo="text"))
            # paper-trading overlay: entry / stop / take-profit + risk zone
            ov = paper_overlay()
            if ov:
                e, s = ov["entry"], ov["sl"]
                if ov["kind"] == "pending":
                    fig.add_hline(y=e, line_color="purple", line_width=2,
                                  line_dash="dash",
                                  annotation_text="лимит",
                                  annotation_position="right")
                    fig.add_hline(y=s, line_color="red", line_width=1,
                                  line_dash="dot",
                                  annotation_text="стоп",
                                  annotation_position="right")
                    fig.add_hrect(y0=min(e, s), y1=max(e, s),
                                  fillcolor="purple", opacity=0.06,
                                  line_width=0)
                    if ov.get("tp"):
                        tp_px = ov["tp"]
                        fig.add_hline(y=tp_px, line_color="green",
                                      line_width=1, line_dash="dot",
                                      annotation_text="тейк",
                                      annotation_position="right")
                else:
                    fig.add_hline(y=e, line_color="blue", line_width=2,
                                  annotation_text="вход", annotation_position="right")
                    fig.add_hline(y=s, line_color="red", line_width=2, line_dash="dot",
                                  annotation_text="стоп", annotation_position="right")
                    fig.add_hrect(y0=min(e, s), y1=max(e, s),
                                  fillcolor="red", opacity=0.07, line_width=0)
                    if ov.get("tp"):
                        tp_px = ov["tp"]
                        fig.add_hline(y=tp_px, line_color="green", line_width=2,
                                      line_dash="dot",
                                      annotation_text="тейк", annotation_position="right")
                        fig.add_hrect(y0=min(e, tp_px), y1=max(e, tp_px),
                                      fillcolor="green", opacity=0.07, line_width=0)
            xs, ys = [], []
            for lv in draw_levels:
                for tc in lv["touches"]:
                    if tc["ts"] >= int(seg[0, 0]):
                        xs.append(ms_local(tc["ts"]))
                        ys.append(tc["price"])
            if len(xs) > MAX_DRAW_TOUCHES:  # keep only the latest markers
                xs, ys = xs[-MAX_DRAW_TOUCHES:], ys[-MAX_DRAW_TOUCHES:]
            if xs:
                fig.add_trace(go.Scatter(x=xs, y=ys, mode="markers", name="касания",
                                         marker=dict(color="orange", size=6)))
            # paper trades: entry marker for the open trade + entry->exit
            # segments for closed trades (green = profit, red = loss)
            t0seg = int(seg[0, 0])
            if PAPER["open"]:
                tr = PAPER["open"]
                if tr["open_ts"] >= t0seg:
                    fig.add_trace(go.Scatter(
                        x=[ms_local(tr["open_ts"])], y=[tr["entry"]],
                        mode="markers", name="вход (открыта)",
                        marker=dict(
                            symbol="triangle-up" if tr["side"] > 0 else "triangle-down",
                            size=14, color="blue"),
                        hoverinfo="text",
                        text=(f"ВХОД {'LONG' if tr['side'] > 0 else 'SHORT'}<br>"
                              f"цена {tr['entry']:,.4f}<br>размер {tr['size']:.6f}<br>"
                              f"стоп {tr['sl']:,.4f}")))
            first_seg = True
            for c in PAPER["closed"][-30:]:
                if c["open_ts"] < t0seg:
                    continue
                col = "green" if c["pnl"] >= 0 else "red"
                fig.add_trace(go.Scatter(
                    x=[ms_local(c["open_ts"]),
                       ms_local(c["close_ts"])],
                    y=[c["entry"], c["exit"]], mode="lines+markers",
                    name="сделки" if first_seg else None,
                    showlegend=first_seg,
                    line=dict(color=col, width=2, dash="dot"),
                    marker=dict(size=9, color=col),
                    hoverinfo="text",
                    text=[f"ВХОД {c['dir']}<br>{c['entry']:,.4f}<br>"
                          f"{ms_local(c['open_ts'])}",
                          f"ВЫХОД {c['exit']:,.4f}<br>"
                          f"P&L {c['pnl']:+.2f} USDT ({c['reason']})<br>"
                          f"{ms_local(c['close_ts'])}"]))
                first_seg = False
            last = seg[-1]
            fig.add_annotation(x=t[-1], y=last[4], text=f"{last[4]:,.1f}",
                               showarrow=False, bgcolor="yellow")
        fig.update_layout(xaxis_rangeslider_visible=False, height=800,
                          hovermode="x unified", showlegend=True,
                          uirevision="live",
                          legend=dict(x=0.01, y=0.99, xanchor="left",
                                      yanchor="top",
                                      bgcolor="rgba(255,255,255,0.75)"),
                          margin=dict(l=50, r=10, t=10, b=30))
        return fig

    return app


def main(argv=None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8050)
    ap.add_argument("--host", default="0.0.0.0")
    args = ap.parse_args(argv)
    app = build_app()
    print(f"open http://{args.host}:{args.port}/  (Ctrl+C to stop)")
    # NOTE: single-threaded server is required — PyExchange is unsendable
    # (the Rust engine is single-threaded by design), so Dash callbacks must
    # run on the same thread that created the Exchange object.
    app.run(host=args.host, port=args.port, debug=False,
            threaded=False, processes=1)
    return 0


if __name__ == "__main__":
    sys.exit(main())