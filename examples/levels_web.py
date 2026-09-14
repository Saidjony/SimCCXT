#!/usr/bin/env python3
"""
levels_web.py
=============
Interactive Plotly dashboard for simccxt level research (OHLC bars, not candles).

Pipeline: the same live simulator feed as `levels_demo.py`
(`load_candles_from_csv` -> reset/step -> `fetch_ohlcv_live`), then:

  fig1  full-history OHLC bars + all support/resistance lines + touch markers
  fig2  zoom on one level with a DROPDOWN selector (top-N by touches):
        OHLC bars, level line, pivot bar, confirmation bar, touches
  fig3  average volume around touches (bar chart)
  fig4  forward-return histograms (30m / 60m)
  table levels summary (HTML)

Everything is packed into ONE self-contained HTML file (plotly.js inlined,
works offline). Optional `--serve` starts a local HTTP server.

Run:
    python3 examples/levels_web.py [--days 2] [--top 12]
    python3 examples/levels_web.py --serve --port 8000   # then open http://localhost:8000/
"""

import argparse
import http.server
import functools
import os
import sys

import numpy as np
import pandas as pd

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import levels_demo as LD

SYMBOL = "BTC/USDT"
TF = "1m"
TF_MS = 60_000


def ms_to_dt(ms):
    return pd.to_datetime(np.asarray(ms, dtype=np.int64), unit="ms")


def hline_seg(x0, x1, y):
    """One horizontal segment as NaN-separated scatter points."""
    return [x0, x1, None], [y, y, None]


def build_full_figure(bars, levels, res):
    import plotly.graph_objects as go

    t = ms_to_dt(bars[:, 0])
    fig = go.Figure()
    fig.add_trace(go.Ohlc(x=t, open=bars[:, 1], high=bars[:, 2],
                          low=bars[:, 3], close=bars[:, 4],
                          name="BTC/USDT 1m (бары)",
                          increasing_line_color="black",
                          decreasing_line_color="black"))
    for direction, color, label in (("support", "green", "support"),
                                    ("resistance", "red", "resistance")):
        xs, ys = [], []
        for lv in levels:
            if lv["direction"] == direction:
                a, b = hline_seg(t[0], t[-1], lv["price"])
                xs += a
                ys += b
        fig.add_trace(go.Scatter(x=xs, y=ys, mode="lines", name=label,
                                 line=dict(color=color, width=1, dash="dash")))
    if len(res):
        for flag, color, label in ((res["breakout"], "red", "касания: пробой"),
                                   (res["reject"], "green", "касания: отбой"),
                                   (~(res["breakout"] | res["reject"]), "gray",
                                    "касания: удержание")):
            sub = res[flag]
            fig.add_trace(go.Scatter(
                x=ms_to_dt(sub["touch_ts"].to_numpy()), y=sub["entry"], mode="markers",
                name=label, marker=dict(color=color, size=5),
                text=[f"{r['level']:,.0f} {r['direction']}<br>"
                      f"{pd.to_datetime(int(r['touch_ts']), unit='ms')}<br>"
                      f"ret60m {r['ret_3600s']:+.2f}%"
                      for _, r in sub.iterrows()],
                hoverinfo="text"))
    fig.update_layout(title="Цена (OHLC-бары) + уровни + касания",
                      xaxis_title="время", yaxis_title="цена",
                      hovermode="x unified", height=520,
                      xaxis_rangeslider_visible=False)
    return fig


def build_zoom_figure(bars, levels, res, top_n=12):
    """One figure, per-level trace groups toggled by a dropdown."""
    import plotly.graph_objects as go

    t = ms_to_dt(bars[:, 0])
    order = (res.groupby("level").size().sort_values(ascending=False)
             .head(top_n).index.tolist() if len(res) else [])
    if not order:
        order = [lv["price"] for lv in levels[:top_n]]
    fig = go.Figure()
    buttons = []
    K = 5  # traces per level: ohlc, level, pivot, confirm, touches
    for k, price in enumerate(order):
        lv = next(x for x in levels if abs(x["price"] - price) < 1e-9)
        sub = res[res["level"] == price]
        last_ms = int(sub["touch_ts"].max()) if len(sub) else int(bars[-1, 0])
        i0 = max(0, lv["pivot_idx"] - 40)
        i1 = int(min(len(bars), (last_ms - int(bars[0, 0])) // TF_MS + 16))
        seg = slice(i0, i1)
        lo, hi = float(np.nanmin(bars[seg, 3])), float(np.nanmax(bars[seg, 2]))
        pad = (hi - lo) * 0.05

        fig.add_trace(go.Ohlc(x=t[seg], open=bars[seg, 1], high=bars[seg, 2],
                              low=bars[seg, 3], close=bars[seg, 4],
                              name="бары", increasing_line_color="black",
                              decreasing_line_color="black", visible=(k == 0)))
        pv = ms_to_dt([lv["pivot_ts"]])[0]
        cf = ms_to_dt([lv["confirmed_ts"]])[0]
        fig.add_trace(go.Scatter(x=[t[seg][0], t[seg][-1]],
                                 y=[price, price], mode="lines", name="уровень",
                                 line=dict(color="orange", width=2), visible=(k == 0)))
        fig.add_trace(go.Scatter(x=[pv, pv], y=[lo - pad, hi + pad], mode="lines",
                                 name="пивот-бар", line=dict(color="blue", dash="dot"),
                                 visible=(k == 0)))
        fig.add_trace(go.Scatter(x=[cf, cf], y=[lo - pad, hi + pad], mode="lines",
                                 name="подтверждение", line=dict(color="purple", dash="dash"),
                                 visible=(k == 0)))
        if len(sub):
            fig.add_trace(go.Scatter(
                x=ms_to_dt(sub["touch_ts"].to_numpy()), y=sub["entry"], mode="markers",
                name="касания",
                marker=dict(size=9,
                            color=["red" if b else ("green" if r else "gray")
                                   for b, r in zip(sub["breakout"], sub["reject"])]),
                text=[f"{pd.to_datetime(int(x), unit='ms')}<br>"
                      f"вход {e:,.1f} (дист {d:+.3f}%)<br>"
                      f"проникновение {p * 100:.2f}% / отбой {q * 100:.2f}%<br>"
                      f"исход: {'пробой' if b else ('отбой' if r else 'удержание')}<br>"
                      f"ret60m {v:+.3f}%"
                      for x, e, d, p, q, b, r, v in
                      zip(sub["touch_ts"], sub["entry"], sub["dist"],
                          sub["penetration"], sub["rejection"],
                          sub["breakout"], sub["reject"], sub["ret_3600s"])],
                hoverinfo="text", visible=(k == 0)))
        else:
            fig.add_trace(go.Scatter(x=[], y=[], mode="markers", name="касания",
                                     visible=(k == 0)))

        vis = [False] * (K * len(order))
        for j in range(K):
            vis[k * K + j] = True
        buttons.append(dict(label=f"{price:,.0f} ({lv['direction'][:4]}, {len(sub)})",
                            method="update",
                            args=[{"visible": vis},
                                  {"title": f"Уровень {price:,.1f} "
                                            f"({lv['direction']}) — {len(sub)} касаний"}]))
    fig.update_layout(title="Зум на уровень (выбери уровень в списке)",
                      updatemenus=[dict(buttons=buttons, direction="down",
                                        showactive=True, x=0.0, y=1.12,
                                        xanchor="left", yanchor="top")],
                      xaxis_rangeslider_visible=False, height=520,
                      hovermode="closest")
    return fig


def build_volume_figure(bars, res):
    import plotly.graph_objects as go

    offs = [-30, -15, -5, -1, 0, 1, 5, 15, 30, 60]
    means = []
    v = bars[:, 5]
    tms = bars[:, 0].astype(np.int64)
    for off in offs:
        vals = []
        for t0 in res["touch_ts"].to_numpy():
            j = int(np.searchsorted(tms, int(t0) + off * TF_MS, side="right")) - 1
            if j >= 0:
                vals.append(v[j])
        means.append(float(np.nanmean(vals)) if vals else np.nan)
    fig = go.Figure(go.Bar(x=[str(o) for o in offs], y=means, name="ср. объём"))
    fig.add_vline(x=4.5, line_dash="dot", line_color="grey")
    fig.update_layout(title="Средний объём вокруг касаний (минуты)",
                      xaxis_title="минут до/после касания", yaxis_title="объём",
                      height=380)
    return fig


def build_returns_figure(res):
    import plotly.graph_objects as go
    from plotly.subplots import make_subplots

    fig = make_subplots(rows=2, cols=1,
                        subplot_titles=("доходность 30м после касания, %",
                                        "доходность 60м после касания, %"))
    for row, h in ((1, 1800), (2, 3600)):
        vals = res[f"ret_{h}s"].dropna()
        if len(vals):
            fig.add_trace(go.Histogram(x=vals, nbinsx=40, name=f"{h // 60}м",
                                       showlegend=False), row=row, col=1)
    fig.add_vline(x=0, line_color="black", row=1, col=1)
    fig.add_vline(x=0, line_color="black", row=2, col=1)
    fig.update_layout(title="Распределение доходностей после касаний",
                      height=560, bargap=0.05)
    return fig


def build_page(figs, summary_html: str) -> str:
    divs = [f.to_html(include_plotlyjs=(i == 0), full_html=False,
                       config={"displaylogo": False}) for i, f in enumerate(figs)]
    return f"""<!DOCTYPE html>
<html lang="ru"><head><meta charset="utf-8">
<title>simccxt — уровни поддержки/сопротивления</title>
<style>
body {{ font-family: sans-serif; max-width: 1200px; margin: 0 auto; padding: 16px; }}
h1 {{ font-size: 22px; }} h2 {{ font-size: 17px; margin-top: 28px; }}
table {{ border-collapse: collapse; font-size: 12px; }}
th, td {{ border: 1px solid #ccc; padding: 3px 8px; text-align: right; }}
.note {{ background: #f6f6f6; padding: 10px 14px; border-left: 4px solid #888; }}
</style></head><body>
<h1>simccxt — уровни поддержки/сопротивления (OHLC-бары, живой пошаговый фид)</h1>
<div class="note">Уровни найдены <b>онлайн</b> в пошаговом потоке
<code>fetch_ohlcv_live()</code> (данные с диска, часы симулятора). Наведи курсор,
приближай колёсиком, выбирай уровень из списка. trade count и Open Interest
недоступны в Python API simccxt — поэтому их здесь нет (честно, без выдумок).</div>
<h2>1. Вся история: OHLC-бары + уровни + касания</h2>
{divs[0]}
<h2>2. Зум на уровень</h2>
{divs[1]}
<h2>3. Объём вокруг касаний</h2>
{divs[2]}
<h2>4. Доходности после касаний</h2>
{divs[3]}
<h2>5. Сводка по уровням</h2>
{summary_html}
</body></html>"""


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="simccxt levels Plotly dashboard")
    ap.add_argument("--days", type=int, default=2)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--pivot", type=int, default=5)
    ap.add_argument("--min-swing", type=float, default=0.0005, dest="min_swing")
    ap.add_argument("--zone", type=float, default=0.001)
    ap.add_argument("--top", type=int, default=12, help="уровней в списке/таблице")
    ap.add_argument("--outdir", default=os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                                     "levels_web_out"))
    ap.add_argument("--serve", action="store_true", help="поднять HTTP-сервер с дашбордом")
    ap.add_argument("--port", type=int, default=8000)
    args = ap.parse_args(argv)
    os.makedirs(args.outdir, exist_ok=True)

    det, bars = LD.run_live(args.days, args.seed, pivot=args.pivot,
                            min_swing=args.min_swing, zone=args.zone)
    print(f"live replay: {len(bars):,} bars, {len(det.levels)} levels")
    res = LD.evaluate(det.levels, bars, [60, 300, 600, 1800, 3600],
                      break_px=0.003, reject_px=0.001)
    print(f"touches: {len(res)}")

    fig1 = build_full_figure(bars, det.levels, res)
    fig2 = build_zoom_figure(bars, det.levels, res, top_n=args.top)
    fig3 = build_volume_figure(bars, res) if len(res) else None
    fig4 = build_returns_figure(res) if len(res) else None
    figs = [f for f in (fig1, fig2, fig3, fig4) if f is not None]

    if len(res):
        g = res.groupby(["level", "direction"], as_index=False).agg(
            touches=("entry", "size"), reject_rate=("reject", "mean"),
            breakout_rate=("breakout", "mean"),
            ret30m=("ret_1800s", "mean"), ret60m=("ret_3600s", "mean"))
        g = g.sort_values("touches", ascending=False).head(args.top)
        g["level"] = g["level"].map(lambda v: f"{v:,.1f}")
        for c in ("reject_rate", "breakout_rate"):
            g[c] = (g[c] * 100).map(lambda v: f"{v:.1f}%")
        for c in ("ret30m", "ret60m"):
            g[c] = g[c].map(lambda v: f"{v:+.3f}%")
        summary_html = g.to_html(index=False)
    else:
        summary_html = "<p>касаний не найдено</p>"

    page = build_page(figs, summary_html)
    path = os.path.join(args.outdir, "dashboard.html")
    with open(path, "w", encoding="utf-8") as f:
        f.write(page)
    print(f"dashboard -> {path} ({os.path.getsize(path) // 1024} KB)")

    if args.serve:
        handler = functools.partial(http.server.SimpleHTTPRequestHandler,
                                    directory=args.outdir)
        srv = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), handler)
        print(f"open http://127.0.0.1:{args.port}/dashboard.html  (Ctrl+C to stop)")
        srv.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())