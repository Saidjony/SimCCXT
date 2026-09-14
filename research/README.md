# research_levels.py — market-level research on simccxt

Deterministic Python study of **BTC/USDT price-level strength** driven entirely by the
**existing simccxt Python API** (no Rust changes, no invented methods).

For every detected level it collects *raw* measurements around each time the price
touches the level zone (volume, price reaction, penetration depth, rejection distance,
forward returns at 1/5/10/30/60 s) so you can test hypotheses such as:

> "If trade count near the level is far above normal but price cannot break the level,
> is a reversal actually more likely?"

No synthetic "level strength score" is produced — all base features are exported to CSV.

---

## Pipeline (implemented in `research_levels.py`)

```
1. data loading            feed OHLCV through simccxt.Exchange (load_candles + fetch_ohlcv)
2. level detection         causal swing-high / swing-low pivots  (no look-ahead)
3. interaction detection   price crossing into a configurable % zone around a level = a touch
4. feature extraction      context window 10s before the touch + at-touch features
5. forward outcome        returns at 1/5/10/30/60 s, max penetration/rejection, MFE/MAE
6. results                per-level table + CSV (raw touches + summary)
7. visualization          matplotlib figures
```

## Requirements

- Python **3.12+** via the project `uv` environment (`.venv`, see *Install*)
- the native `simccxt` extension, installed into `.venv` (see *Install*)
- `numpy`, `pandas`, `matplotlib` (used only for analysis/plots, never inside simccxt)

## Install (uv environment in the repo root)

All commands run from `/home/linx/simccxt`:

```bash
uv venv --python 3.12                                  # create .venv (once)
uv pip install --python .venv/bin/python \
    numpy pandas matplotlib plotly                     # analysis libs (once)
uv pip install --python .venv/bin/python -e .          # build + install simccxt (maturin backend)
```

After any Rust change, re-run the last command to refresh the installed extension.
`import simccxt` inside `.venv` always resolves to the installed package
(`.venv/lib/python3*/site-packages/simccxt`) — no `PYTHONPATH` needed.

## Run

```bash
cd /home/linx/simccxt
# demo on deterministic synthetic data (3 days of 1 s bars)
.venv/bin/python research/research_levels.py

# real data
.venv/bin/python research/research_levels.py --csv data/btc_usdt_1s.csv

# tuning
.venv/bin/python research/research_levels.py --level-zone 0.001 --pivot 5 --max-levels 40 \
                           --max-horizon 60 --horizons 1 5 10 30 60
```

The other runnable scripts work the same way:

```bash
.venv/bin/python examples/realtime_simulator.py        # step-by-step live feed from disk
.venv/bin/python examples/levels_demo.py --days 2      # online levels in the live feed
.venv/bin/python examples/levels_web.py --days 2       # interactive Plotly dashboard
```

### Options

| flag                | default | meaning                                                        |
| ------------------- | ------- | -------------------------------------------------------------- |
| `--csv PATH`        | –       | OHLCV csv `timestamp,open,high,low,close,volume` (s or ms)      |
| `--days N`          | 3       | synthetic demo length in days (1 s bars)                        |
| `--seed N`          | 7       | RNG seed (deterministic output)                                  |
| `--pivot N`         | 5       | swing pivot lookback (bars each side, confirmation is causal)   |
| `--level-zone F`    | 0.001   | level zone width as a fraction (0.001 = 0.1%)                   |
| `--min-swing F`     | 0.0008  | minimum pivot prominence (fraction)                             |
| `--max-levels N`    | 40      | cap on analysed levels (evenly sampled in time)                 |
| `--ctx-s N`         | 10      | context window size before the touch (s)                        |
| `--max-horizon N`   | 60      | max forward window (s)                                          |
| `--horizons …`      | 1 5 10 30 60 | forward return horizons (s)                              |
| `--break-px F`      | 0.002   | penetration fraction counted as *breakout* (0.2%)               |
| `--reject-px F`     | 0.001   | rejection distance fraction (0.1%)                              |
| `--outdir PATH`     | `output`| output directory                                                |

## CSV data format

`timestamp,open,high,low,close,volume` — `timestamp` in seconds or milliseconds
(detected automatically). Rows need not be sorted (sorted internally).

## Output

| File | Contents |
| ---- | -------- |
| `output/touches.csv` | **every** raw measurement per level touch (base-level features, returns at every horizon, MFE/MAE, penetration/rejection) |
| `output/levels_summary.csv` | per-level table |
| `output/plots/1_prices_levels.png` | price + detected levels |
| `output/plots/2_trades_around_level.png` | trade count around level (unavailable → honest note) |
| `output/plots/3_volume_around_level.png` | avg traded volume around touches (offsets −10 … +60 s) |
| `output/plots/4_open_interest.png` | Open Interest (unavailable → honest note) |
| `output/plots/5_forward_returns.png` | distributions of 1/5/10/30/60 s forward returns |

Per-touch `touches.csv` columns:

```
level_price, direction, touch_ts, entry_price, distance_to_level_pct,
ctx_volume, ctx_trade_count, ctx_oi_change, volume_zscore, trade_count_zscore,
ctx_price_change_pct, max_penetration_pct, max_rejection_pct,
max_favorable_excursion, max_adverse_excursion, breakout, rejection,
price_1s…price_60s, return_1s…return_60s
```

## Availability — what the current Python API **cannot** provide

The script is honest about what simccxt's Python API does **not** expose. These columns
are stored as `NaN` and plotted as "not available" — nothing is fabricated:

| Field | Status | Missing API surface |
| ----- | ------ | ------------------- |
| OHLCV | available | `load_candles` / `fetch_ohlcv`, `[ts_ms,o,h,l,c,v]` |
| trade count | **unavailable** | `load_candles` has no `trade_count` column; no `load_trades_agg` / per-candle trade count method |
| Open Interest | **unavailable (series)** | no `set_open_interest(symbol,…)` / `load_oi(...)` in Python; `fetch_open_interest` returns only the current value |
| buy/sell volume, delta | **unavailable** | OHLCV `volume` is aggregate; no aggressor-side breakdown |

So `avg_trades` and `avg_oi_change` in the summary table are `NaN`, and the "trade count"
and "OI" plots are placeholder notes. The data flow for these features would require one of
the missing methods above to be added to the simccxt Python API.

## Method notes (scientific care)

- **Levels are causal.** A swing pivot at bar `i` is only considered *known* after
  bar `i + pivot` exists (`detected_at` = confirmation time). Interactions can only start
  after the level is known ⇒ **look-ahead bias is structurally prevented**.
- **Touch**: price (close) enters the level's zone `[level·(1−zone), level·(1+zone)]` from
  the approach side — from above for a support, from below for a resistance. Repeated
  touches are counted after the price leaves and re-enters the zone.
- **Outcomes are measured strictly after the touch bar** (`(t0, t0+60s]`):
  - `max_penetration_pct` — how far price moved through the level (support: below; resistance: above).
  - `max_rejection_pct` — how far price was pushed back off the level.
  - `MFE / MAE` — max favorable/adverse excursion vs. the entry price (from the entry bar).
  - `breakout` — penetration ≥ `--break-px`; `rejection` — breakout not broken level)
    and price recovered ≥ `--reject-px` off the level. `breakout_rate + rejection_rate + holding = 1`.
  - `return_<h>s` — close(h s after entry) relative to the entry price (%).
- **Trigger context** is `[t0−10s, t0)` only; nothing from the future enters it.
- **Determinism**: with the same data/config the entire run reproduces identical CSV
  (verified: repeated runs hash to the same file).

## Example research question you can now answer

```python
import pandas as pd
df = pd.read_csv("output/touches.csv")
g = df.groupby(df["ctx_volume"] > df["ctx_volume"].median())
print(g.agg(avg_ret60=("return_60s","mean"),
            rejection_rate=("rejection","mean"),
            breakout_rate=("breakout","mean")))
```

Level "strength" is a *measurement*, not a label: rejection probability, penetration
depth, average forward return, volume response, and MFE/MAE are all available per touch.