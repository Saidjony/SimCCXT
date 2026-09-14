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

  coin:       BTC/USDT, ETH/USDT, XRP/USDT, SOL/USDT
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
import time as _time

import numpy as np
import pandas as pd

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import levels_demo as LD
import market_data as MD

import simccxt

TFS = {"1m": 60_000, "5m": 300_000, "15m": 900_000, "1h": 3_600_000}
N_BARS = 1200
ZONE = 0.001


def synthetic(symbol: str, tf: str, n: int = N_BARS) -> pd.DataFrame:
    """Deterministic OHLC bars for ANY coin/timeframe (offline fallback).

    Price scale and seed derive from the symbol, so every coin gets its own
    stable synthetic series.
    """
    import hashlib
    h = int(hashlib.md5(symbol.encode()).hexdigest()[:8], 16)
    mag = 10 ** (h % 5)                      # 1 .. 10000 price magnitude
    s0 = float(mag) * (1.0 + (h % 100) / 100.0)
    seed = h % 10_000
    step_s = TFS[tf] // 1000
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
            "follow": True, "last_zoom": None}


STATUS_MSG = "старт: синтетика, выбери даты — данные догрузятся с Binance в кэш data/ohlcv/"
MAX_BARS = 200_000


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

    Data path: disk cache first, Binance download for missing bars only,
    synthetic fallback when offline/unsupported.
    """
    global STATE, STATUS_MSG
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
        t0 = _time.time()
        df, st = MD.load_or_download(symbol, tf, s, e)
        if len(df) == 0:
            raise RuntimeError("пустой диапазон (нет баров)")
        STATE = new_session(symbol, tf, df, source=f"binance(cache)+dl", key=key)
        STATUS_MSG = (f"Binance: из кэша {st['cached']}, докачано {st['downloaded']} "
                      f"за {_time.time() - t0:.1f}с")
    except Exception as exc:  # offline / bad symbol / bad range -> synthetic
        df = synthetic(symbol, tf)
        df["ts_ms"] = to_ms(df["timestamp"])
        STATE = new_session(symbol, tf, df, source="synthetic-fallback", key=key)
        STATUS_MSG = f"загрузка не удалась ({exc}); показан синтетический ряд"
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




# Helpers moved out of Dash callbacks
def make_paper(st):
    """Open-position panel (live P&L) + closed-trades table rows."""
    t = PAPER["open"]
    if t is None:
        panel = (f"счёт {PAPER['equity']:.2f} USDT "
                 f"(депозит {PAPER['deposit']:.2f}, риск {PAPER['risk_pct']}%) — "
                 f"нет открытой сделки")
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

def make_stats(st, frozen=False):
    ex = st["ex"]
    n_lv = len(st["det"].levels)
    n_t = sum(len(lv["touches"]) for lv in st["det"].levels)
    now = pd.to_datetime(ex.now_ms(), unit="ms", utc=True).strftime("%m-%d %H:%M")
    flag = "FINISHED" if st["done"] else ("RUNNING" if st["running"] else "PAUSED")
    if frozen and frozen is not True:
        flag += f" + {frozen}"
    elif frozen:
        flag += " + VIEW FROZEN"
    elif not st.get("follow", True):
        flag += " + VIEW: свой (дабл-клик — снова live)"
    r0, r1 = st["range"]
    rng = (pd.to_datetime(r0, unit="ms", utc=True).strftime("%m-%d") + ".." +
           pd.to_datetime(r1, unit="ms", utc=True).strftime("%m-%d"))
    return (f"{st['symbol']} {st['tf']} [{st['source']} {rng}] | sim-time {now} | "
            f"bars {st['seen']} | levels {n_lv} | touches {n_t} | {flag} || {STATUS_MSG}")

def make_table_rows(st):
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
    return rows

import plotly.graph_objects as go

# ------------------------------------------------------------------ Taipy GUI
import threading
from taipy.gui import Gui, State, get_state_id, invoke_callback, invoke_long_callback

try:
    from taipy.gui import notify
except ImportError:
    notify = None


# The simulator is intentionally kept in one worker.  This preserves the
# original "single-threaded simccxt" rule: the exchange object is never
# accessed concurrently by several simulation workers.
RUN_LOCK = threading.RLock()
RUNNER_STARTED = False
RUNNER_STOP = False


def _empty_snapshot(message=""):
    return {
        "figure": go.Figure(),
        "stats": message,
        "levels": [],
        "closed": [],
        "panel": "—",
        "calc": calc_text(PAPER["equity"], PAPER["risk_pct"], PAPER.get("draft") or {}),
    }


def _snapshot(window=300, frozen=False):
    """Build everything the browser needs from the current simulator state."""
    global STATE
    if STATE is None:
        return _empty_snapshot(STATUS_MSG)

    st = STATE
    panel, closed = make_paper(st)
    stats = make_stats(st, frozen="FROZEN" if frozen else False)
    fig = make_fig(st, int(window or 300))
    return {
        "figure": fig,
        "stats": stats,
        "levels": make_table_rows(st),
        "closed": closed,
        "panel": panel,
        "calc": calc_text(PAPER["equity"], PAPER["risk_pct"], PAPER.get("draft") or {}),
    }


def _apply_snapshot(state: State, snap: dict):
    state.chart = snap["figure"]
    state.stats = snap["stats"]
    state.levels = snap["levels"]
    state.closed = snap["closed"]
    state.open_panel = snap["panel"]
    state.calc_preview = snap["calc"]


def _simulation_step(symbol, tf, speed, window):
    """Advance the simulator. Must run under RUN_LOCK."""
    global STATE

    if STATE is None:
        return

    st = STATE
    ex = st["ex"]

    if st["running"] and not st["done"]:
        ex.step(int(speed or st.get("speed", 2)))
        bars = ex.fetch_ohlcv_live(symbol, tf)

        if len(bars) > st["seen"]:
            new = bars[st["seen"]:]
            det = st["det"]
            det.feed(bars)

            prev_close = (
                float(bars[st["seen"] - 1][4])
                if st["seen"] > 0 else None
            )
            log_touches(det, new, prev_close)
            settle_paper(new)
            st["seen"] = len(bars)

        st["done"] = ex.replay_done(symbol, tf)


def _live_worker(gui, state_id):
    """
    Background Taipy worker.

    Taipy's invoke_callback() sends each completed snapshot back to the
    browser. The simulation itself remains serialized in this worker.
    """
    global RUNNER_STARTED, RUNNER_STOP

    try:
        while not RUNNER_STOP:
            with RUN_LOCK:
                if STATE is not None:
                    speed = STATE.get("speed", 2)
                    window = STATE.get("window", 300)
                    symbol = STATE["symbol"]
                    tf = STATE["tf"]

                    _simulation_step(symbol, tf, speed, window)

                    frozen = False
                    snap = _snapshot(window, frozen=frozen)
                else:
                    snap = _empty_snapshot(STATUS_MSG)

            invoke_callback(gui, state_id, _apply_snapshot, [snap])
            _time.sleep(1.0)
    finally:
        RUNNER_STARTED = False


def _start_runner(state):
    global RUNNER_STARTED, RUNNER_STOP

    if RUNNER_STARTED:
        return

    RUNNER_STOP = False
    RUNNER_STARTED = True

    invoke_long_callback(
        state=state,
        user_function=_live_worker,
        user_function_args=[gui, get_state_id(state)],
        user_status_function=lambda s, status: None,
        user_status_function_args=[],
        period=1000,
    )


def _rebuild_for_state(state):
    """Read UI selections and rebuild the session when the selection changed."""
    global STATE, PAPER

    symbol = (state.coin or "").strip().upper()
    tf = state.tf
    dates = state.dates or [None, None]

    if not valid_symbol(symbol):
        state.stats = "введи монету как BASE/QUOTE, например DOGE/USDT"
        return

    start_date = dates[0] if len(dates) > 0 else None
    end_date = dates[1] if len(dates) > 1 else None

    with RUN_LOCK:
        ensure_session(symbol, tf, start_date, end_date)
        if STATE is not None:
            STATE["speed"] = int(state.speed or 2)
            STATE["window"] = int(state.window or 300)
            STATE["follow"] = True
        PAPER["open"] = None
        PAPER["draft"] = {}

        snap = _snapshot(state.window)
    _apply_snapshot(state, snap)


def on_init(state: State):
    """Taipy per-client initialization."""
    _rebuild_for_state(state)
    _start_runner(state)


def on_change(state: State, var_name: str, var_value):
    """Global Taipy binding callback."""
    if var_name in {"coin", "tf", "dates"}:
        _rebuild_for_state(state)
        return

    if var_name == "speed":
        with RUN_LOCK:
            if STATE is not None:
                STATE["speed"] = int(var_value or 2)
        return

    if var_name == "window":
        with RUN_LOCK:
            if STATE is not None:
                STATE["window"] = int(var_value or 300)
                snap = _snapshot(state.window)
        _apply_snapshot(state, snap)
        return

    if var_name in {"deposit", "risk_pct", "direction", "entry_price",
                    "stop_price", "tp_price"}:
        with RUN_LOCK:
            if var_name == "deposit":
                try:
                    PAPER["deposit"] = float(var_value)
                except (TypeError, ValueError):
                    pass
            elif var_name == "risk_pct":
                try:
                    PAPER["risk_pct"] = float(var_value)
                except (TypeError, ValueError):
                    pass

            PAPER["draft"] = read_draft(
                state.direction,
                state.entry_price,
                state.stop_price,
                state.tp_price,
            ) or {}

            state.calc_preview = calc_text(
                PAPER["equity"],
                PAPER["risk_pct"],
                PAPER["draft"],
            )


def pause_resume(state: State, id=None, payload=None):
    with RUN_LOCK:
        if STATE is not None:
            STATE["running"] = not STATE["running"]
        state.pause_label = (
            "Пауза" if STATE and STATE["running"] else "Продолжить"
        )


def reset_sim(state: State, id=None, payload=None):
    global STATE
    with RUN_LOCK:
        STATE = None
        PAPER["open"] = None
        PAPER["draft"] = {}
        state.pause_label = "Пауза"
    _rebuild_for_state(state)


def toggle_freeze(state: State, id=None, payload=None):
    state.view_frozen = not state.view_frozen
    state.freeze_label = (
        "▶ Продолжить график" if state.view_frozen
        else "⏸ Заморозить график"
    )
    with RUN_LOCK:
        snap = _snapshot(state.window, frozen=state.view_frozen)
    _apply_snapshot(state, snap)


def update_draft(state: State):
    on_change(state, "entry_price", state.entry_price)


def open_trade(state: State, id=None, payload=None):
    if STATE is None:
        state.trade_status = "сначала дождись загрузки данных"
        return

    with RUN_LOCK:
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

        draft = read_draft(
            state.direction,
            state.entry_price,
            state.stop_price,
            state.tp_price,
        )
        if not draft:
            state.trade_status = "⚠ заполни вход и стоп"
            return

        try:
            c = calc_trade(
                PAPER["equity"],
                PAPER["risk_pct"],
                draft["side"],
                draft["entry"],
                draft["sl"],
                draft["tp"],
            )
        except ValueError as exc:
            state.trade_status = f"⚠ {exc}"
            return

        PAPER["open"] = dict(c, open_ts=STATE["ex"].now_ms())
        state.trade_status = (
            f"открыта {'LONG' if c['side'] > 0 else 'SHORT'} "
            f"{c['size']:.6f} по {c['entry']}, стоп {c['sl']}, "
            f"риск {c['risk_usd']:.2f} USDT"
        )


def close_trade(state: State, id=None, payload=None):
    with RUN_LOCK:
        t = PAPER["open"]
        if not t or STATE is None:
            state.trade_status = "нет открытой сделки"
            return

        bars = STATE["ex"].fetch_ohlcv_live(
            STATE["symbol"], STATE["tf"]
        )
        px = float(bars[-1][4]) if len(bars) else t["entry"]
        close_paper(px, STATE["ex"].now_ms(), "MANUAL")
        state.trade_status = PAPER["last_msg"]


def reset_account(state: State, id=None, payload=None):
    with RUN_LOCK:
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
            f"счёт сброшен: {PAPER['equity']:.2f} USDT, "
            f"риск {PAPER['risk_pct']}%"
        )
        state.trade_status = PAPER["last_msg"]


def build_taipy_page():
    from taipy.gui import Markdown

    page = """
# SimCCXT — live levels

<|layout|
<|part|
## График

<|{chart}|chart|figure={chart}|height=780px|rebuild=True|>

<|{stats}|text|mode=pre|>
|>

<|part|
### Данные

**Монета**

<|{coin}|input|change_delay=-1|>

**Таймфрейм**

<|{tf}|selector|lov={tf_options}|dropdown|>

**Период (UTC)**

<|{dates}|date_range|format=yyyy-MM-dd|>

**Скорость (баров/тик)**

<|{speed}|slider|min=1|max=20|step=1|>

**Окно (баров)**

<|{window}|selector|lov={window_options}|dropdown|>

<|{pause_label}|button|on_action=pause_resume|>
<|Заново|button|on_action=reset_sim|>
<|{freeze_label}|button|on_action=toggle_freeze|>

### Виртуальная торговля

Депозит, USDT

<|{deposit}|number|>

Риск на сделку, %

<|{risk_pct}|number|>

Направление

<|{direction}|selector|lov=long;short|dropdown|>

Вход

<|{entry_price}|number|>

Стоп

<|{stop_price}|number|>

Тейк (необяз.)

<|{tp_price}|number|>

<|Открыть сделку|button|on_action=open_trade|>
<|Закрыть по рынку|button|on_action=close_trade|>
<|Сбросить счёт|button|on_action=reset_account|>

<|{calc_preview}|text|mode=pre|>
<|{trade_status}|text|mode=pre|>
<|{open_panel}|text|mode=pre|>
|>
|>

## Сделки

<|{closed}|table|width=100%|>

## Уровни: пивот → подтверждение → касания

<|{levels}|table|width=100%|>
"""

    return Markdown(page)


# ------------------------------------------------------------------ Plotly figure
# This is deliberately outside the Dash app. Taipy's chart control accepts a
# native plotly.graph_objects.Figure through its `figure` property.
def make_fig(st, window):
    ex = st["ex"]
    bars = np.array(ex.fetch_ohlcv_live(st["symbol"], st["tf"]))

    fig = go.Figure()

    if len(bars):
        seg = bars[max(0, len(bars) - int(window)):]
        t = pd.to_datetime(seg[:, 0], unit="ms")

        fig.add_trace(go.Ohlc(
            x=t,
            open=seg[:, 1],
            high=seg[:, 2],
            low=seg[:, 3],
            close=seg[:, 4],
            name=f"{st['symbol']} (бары)",
            increasing_line_color="black",
            decreasing_line_color="black",
        ))

        for lv in st["det"].levels:
            fig.add_hline(
                y=lv["price"],
                line_color=(
                    "green"
                    if lv["direction"] == "support"
                    else "red"
                ),
                line_dash="dash",
                line_width=1,
                opacity=0.8,
            )

        px, py, ptext = [], [], []

        for lv in st["det"].levels:
            cb = lv.get("confirming_bars") or []

            if cb and cb[-1] >= int(seg[0, 0]):
                fig.add_vrect(
                    x0=pd.to_datetime(cb[0], unit="ms"),
                    x1=pd.to_datetime(cb[-1], unit="ms"),
                    fillcolor="blue",
                    opacity=0.07,
                    line_width=0,
                )

            if lv["pivot_ts"] >= int(seg[0, 0]):
                px.append(pd.to_datetime(lv["pivot_ts"], unit="ms"))
                py.append(lv["price"])
                ptext.append(
                    f"уровень {lv['price']:,.4f} "
                    f"({lv['direction']})<br>"
                    f"пивот-бар "
                    f"{pd.to_datetime(lv['pivot_ts'], unit='ms')}<br>"
                    f"подтверждён "
                    f"{pd.to_datetime(lv['confirmed_ts'], unit='ms')}<br>"
                    f"окно подтверждения: {len(cb)} баров<br>"
                    f"касаний: {len(lv['touches'])}"
                )

        if px:
            fig.add_trace(go.Scatter(
                x=px,
                y=py,
                mode="markers",
                name="пивот-бары",
                marker=dict(symbol="diamond", size=10, color="blue"),
                text=ptext,
                hoverinfo="text",
            ))

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
                    xs.append(pd.to_datetime(tc["ts"], unit="ms"))
                    ys.append(tc["price"])

        if xs:
            fig.add_trace(go.Scatter(
                x=xs,
                y=ys,
                mode="markers",
                name="касания",
                marker=dict(color="orange", size=6),
            ))

        t0seg = int(seg[0, 0])

        if PAPER["open"]:
            tr = PAPER["open"]
            if tr["open_ts"] >= t0seg:
                fig.add_trace(go.Scatter(
                    x=[pd.to_datetime(tr["open_ts"], unit="ms")],
                    y=[tr["entry"]],
                    mode="markers",
                    name="вход (открыта)",
                    marker=dict(
                        symbol=(
                            "triangle-up"
                            if tr["side"] > 0
                            else "triangle-down"
                        ),
                        size=14,
                        color="blue",
                    ),
                    hoverinfo="text",
                    text=(
                        f"ВХОД "
                        f"{'LONG' if tr['side'] > 0 else 'SHORT'}<br>"
                        f"цена {tr['entry']:,.4f}<br>"
                        f"размер {tr['size']:.6f}<br>"
                        f"стоп {tr['sl']:,.4f}"
                    ),
                ))

        first_seg = True

        for c in PAPER["closed"][-30:]:
            if c["open_ts"] < t0seg:
                continue

            col = "green" if c["pnl"] >= 0 else "red"

            fig.add_trace(go.Scatter(
                x=[
                    pd.to_datetime(c["open_ts"], unit="ms"),
                    pd.to_datetime(c["close_ts"], unit="ms"),
                ],
                y=[c["entry"], c["exit"]],
                mode="lines+markers",
                name="сделки" if first_seg else None,
                showlegend=first_seg,
                line=dict(color=col, width=2, dash="dot"),
                marker=dict(size=9, color=col),
                hoverinfo="text",
                text=[
                    f"ВХОД {c['dir']}<br>{c['entry']:,.4f}<br>"
                    f"{pd.to_datetime(c['open_ts'], unit='ms')}",
                    f"ВЫХОД {c['exit']:,.4f}<br>"
                    f"P&L {c['pnl']:+.2f} USDT ({c['reason']})<br>"
                    f"{pd.to_datetime(c['close_ts'], unit='ms')}",
                ],
            ))
            first_seg = False

        last = seg[-1]
        fig.add_annotation(
            x=t[-1],
            y=last[4],
            text=f"{last[4]:,.1f}",
            showarrow=False,
            bgcolor="yellow",
        )

    fig.update_layout(
        xaxis_rangeslider_visible=False,
        height=780,
        hovermode="x unified",
        showlegend=True,
        uirevision="live",
        margin=dict(l=50, r=10, t=10, b=30),
    )
    return fig


# Initial Taipy variables
today = dt.datetime.now(dt.timezone.utc).date()
coin = "BTC/USDT"
tf = "1m"
tf_options = list(TFS)
dates = [
    today - dt.timedelta(days=2),
    today,
]
speed = 2
window = 300
window_options = [100, 200, 300, 500, 1200]

chart = go.Figure()
stats = "загрузка..."
levels = []
closed = []
open_panel = "—"
calc_preview = "введи вход и стоп — размер посчитается сам"
trade_status = ""
deposit = 10000.0
risk_pct = 1.0
direction = "long"
entry_price = None
stop_price = None
tp_price = None
pause_label = "Пауза"
freeze_label = "⏸ Заморозить график"
view_frozen = False

gui = Gui(build_taipy_page())


def main(argv=None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8050)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args(argv)

    print(f"open http://{args.host}:{args.port}/  (Ctrl+C to stop)")
    gui.run(
        title="simccxt live levels",
        host=args.host,
        port=args.port,
        debug=False,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
