# simccxt

**Virtual Cryptocurrency Exchange** — a CCXT-compatible exchange simulator.

simccxt is NOT a traditional backtesting framework. It is a *virtual exchange* that fully
implements the CCXT interface, so a strategy cannot tell whether it is connected to
Binance, Bybit, OKX, or the simulator.

```python
import ccxt
exchange = ccxt.binance()
```

is replaceable with

```python
import simccxt
exchange = simccxt.binance(...)
```

without changing strategy logic.

## Philosophy

The simulator *emulates the exchange* rather than the strategy. It owns the market, time,
balances, orders, executions, positions, funding, leverage, liquidation, and fees. The
strategy only consumes the API.

Everything is deterministic: no global state, no hidden mutable state, and configurable
seeds — every replay is reproducible.

## Tech Stack

| Layer        | Technology                               |
| ------------ | ---------------------------------------- |
| Core         | Rust                                     |
| Bindings     | PyO3 0.29 (Python 3.12, see below)       |
| Storage      | CSV (in-memory / disk cache)             |
| Concurrency  | Deterministic single-threaded engine     |
| Serialization| serde, serde_json                        |
| Web (Dash)   | Dash 4.x + Plotly                        |

No Pandas inside the engine (analysis only, on the Python side).

## Architecture

The project is a Cargo workspace with independent crates under `src/`:

```
src/core            fundamental types (Symbol, Order, Trade, Position, Balance, ...)
src/clock           controlled time (step/run/seek/pause/resume/reset)
src/market          order book, ticker, funding rate, open interest, candle series
src/storage         storage abstraction (in-memory, CSV)
src/event-bus       synchronous pub/sub (deterministic)
src/replay-engine   chronological replay of historical market data
src/wallet          balances, realized/unrealized PnL, fees, funding
src/order-engine    matching engine (market/limit, partial fills, fees, post-only)
src/portfolio       positions, cross/isolated margin, leverage, liquidation
src/broker          order types + account settlement orchestration
src/exchange        CCXT-compatible facade (fetch_*, create_order, cancel_order)
src/indicators      SMA, EMA, RSI, MACD, Bollinger Bands, ATR
src/feature-engine  VWAP, CVD, delta, aggression, whale activity, ...
src/downloader      normalize + dedup + resume-safe local caching (Binance/Bybit/OKX)
src/data-provider   unified data-source interface
src/python-api      PyO3 bindings (import simccxt)
```

Python side:

```
research/research_levels.py    swing-level market microstructure research
research/README.md             how to run the research, options, outputs
examples/market_data.py        disk cache + Binance download (cache-first)
examples/realtime_simulator.py step-by-step live feed demo
examples/levels_demo.py        online levels inside the live feed
examples/levels_web.py         static Plotly dashboard (offline HTML)
examples/live_dashboard.py     Dash site: live feed + levels + paper trading
examples/levels_finder.py      earliest level-finder prototype (via simccxt)
examples/levels_cluster.py     price-time clustering levels (XRP, no swings)
examples/ccxt_compatibility.py runnable CCXT-style strategy example
data/ohlcv/                    local market-data cache (machine-local, gitignored)
```

## Building & Testing

### Rust

```bash
cargo test --workspace          # run all unit tests
cargo clippy --workspace        # lint
cargo build --release --workspace
```

### Python environment (uv, in the repo root)

```bash
uv venv --python 3.12                                  # create .venv (once)
uv pip install --python .venv/bin/python \
    numpy pandas matplotlib plotly dash playwright     # libs (once)
uv pip install --python .venv/bin/python -e .          # build + install simccxt
```

`import simccxt` inside `.venv` resolves to the installed package — no
`PYTHONPATH` needed. Re-run `uv pip install -e .` after any Rust change.

## Quick Start (Python)

```python
import simccxt

exchange = simccxt.binance()          # interchangeable with ccxt.binance()
exchange.add_market("BTC/USDT", "spot")
exchange.deposit("USDT", 100_000.0)
exchange.deposit("BTC", 1.0)

exchange.load_candles("BTC/USDT", "1m", [
    [1704067200000, 99000.0, 101000.0, 98000.0, 100000.0, 10.0],
    [1704067260000, 100000.0, 102000.0, 99500.0, 101500.0, 12.0],
])

print(exchange.fetch_ohlcv("BTC/USDT", "1m"))
print(exchange.fetch_ticker("BTC/USDT"))
print(exchange.fetch_order_book("BTC/USDT"))

order = exchange.create_order("BTC/USDT", "limit", "buy", 0.01, 90000.0)
print(exchange.fetch_open_orders("BTC/USDT"))
exchange.cancel_order(order["id"], "BTC/USDT")
print(exchange.fetch_balance())
```

See `examples/ccxt_compatibility.py` for a runnable strategy that only uses the CCXT
public interface.

## Implemented CCXT Surface

- `fetch_ohlcv`, `fetch_ticker`, `fetch_order_book`, `fetch_trades`
- `fetch_balance`, `fetch_positions`, `fetch_open_orders`
- `fetch_funding_rate`, `fetch_open_interest`
- `create_order` (market, limit, stop, stop-limit, take-profit, trailing-stop,
  post-only, IOC, FOK, reduce-only)
- `cancel_order`
- Time control: `step`, `step_n`, `run`, `pause`, `resume`, `seek`, `reset`

## Step-by-step simulator (live-feed behaviour from disk)

Like ccxt fetches real data from the internet, simccxt can deliver stored data
step-by-step as if it were a live feed — but sourced from local memory/CSV on
disk, fully deterministic:

```python
import simccxt

ex = simccxt.binance()
ex.add_market("BTC/USDT", "spot")
ex.load_candles_from_csv("BTC/USDT", "1s", "data/btc_1s.csv")  # data from DISK

ex.reset(start_ms)      # point the simulator clock at the first bar
ex.set_step_ms(1000)    # one step = 1 second of market time
ex.resume()

while not ex.replay_done("BTC/USDT", "1s"):
    bars = ex.fetch_ohlcv_live("BTC/USDT", "1s")   # only bars <= now
    print(ex.now_ms(), len(bars), bars[-1][4])
    ex.step(1)          # advance the clock (no real sleeping required)
```

New Python API for the simulator:

- `load_candles_from_csv(symbol, timeframe, path)` — load OHLCV from disk
  (`timestamp,open,high,low,close,volume`, ms/s or ISO-8601 timestamps).
- `reset(time_ms)` / `resume()` / `pause()` / `set_step_ms(ms)`
- `step(n=1)` / `run()` — advance the simulator clock.
- `fetch_ohlcv_live(symbol, timeframe)` — OHLCV rows with `timestamp <= now`
  (a per-step live feed).
- `replay_done(symbol, timeframe)` — true when the last candle was consumed.

### Clock semantics (important)

- `reset()` puts the clock at the given timestamp **and pauses it** — call
  `resume()` before stepping, otherwise `step()` does nothing.
- `fetch_ohlcv()` returns **all** loaded candles (ignores the clock);
  `fetch_ohlcv_live()` returns only candles with `timestamp <= now`.
- `step_ms` defaults to 1000 ms. For 1-minute bars set `set_step_ms(60000)`
  so one step equals one bar.

## Data layer: disk cache + Binance download

`examples/market_data.py` implements "not on the laptop → fetch from the internet":

- Cache files: `data/ohlcv/{SYMBOL}_{timeframe}.csv`
  (e.g. `data/ohlcv/BTCUSDT_1m.csv`), columns
  `timestamp,open,high,low,close,volume` with millisecond timestamps.
- `load_or_download(symbol, tf, start_ms, end_ms)`:
  1. reads the cache file if present;
  2. computes which bars of `[start, end)` are missing on the bar grid;
  3. downloads **only the missing segments** from Binance public klines
     (paginated, no API key), merges + dedupes + sorts, writes the cache back.
- Repeat runs are instant (cache hit). Date ranges over 200 000 bars are
  refused with a message — shrink the range or use a bigger timeframe.
- Offline / bad symbol / empty range → caller falls back to deterministic
  synthetic data (explicitly reported, never silent).

`data/` is gitignored — cache is machine-local.

## Research: market-level hypotheses (`research/`)

`research/research_levels.py` — one runnable file (see `research/README.md`):

1. **data loading** — OHLCV pushed *through* `simccxt.Exchange`
   (`load_candles` → bulk `fetch_ohlcv`); `--csv` for real data or
   deterministic synthetic demo (`--seed`).
2. **level detection** — causal swing-high / swing-low pivots: a pivot at bar
   `i` becomes known only at bar `i+pivot` (no look-ahead).
3. **interaction detection** — a *touch* = close entering the `±zone`
   (default 0.1%) from the approach side (from above for support, from below
   for resistance); re-entry after exit counts as a new touch.
4. **feature extraction** — 10 s context (volume, volume z-score vs rolling
   baseline, price change); trade count / OI stay NaN (see below).
5. **forward outcomes** — returns at 1/5/10/30/60 s, max penetration /
   rejection, MFE/MAE, breakout/reject flags, measured strictly **after**
   the touch bar.
6. **results** — per-level table + `touches.csv` (every raw measurement) +
   `levels_summary.csv`.
7. **visualization** — matplotlib figures (price+levels, volume around touch,
   forward-return distributions; trade-count/OI figures are honest placeholders).

`examples/levels_cluster.py` — alternative detection without swings and without
any fixed percent rule (XRP/USDT): all closes → sort by price → adaptive
density clustering (`bin_width = 0.5 × median bar range`) → time structure per
cluster (repetitions, lifespan, distinct visits, Δtime/Δprice gaps) → levels as
data-derived `[lo, hi]` zones. Breakout threshold = half zone width (also from
data). Detection is retrospective by construction; outcomes stay causal.

`examples/levels_demo.py` — the same swing research but driven by the **live**
stepped feed (`fetch_ohlcv_live` + causal incremental detector).

### Data availability (honest limits of the Python API)

| Field              | Status                                             |
| ------------------ | -------------------------------------------------- |
| OHLCV              | available (`load_candles` / `fetch_ohlcv`)          |
| trade count        | **unavailable** — no column/method → NaN           |
| Open Interest      | **unavailable as a series** (`fetch_open_interest` returns only the current value; no `set_open_interest`/`load_oi` in Python) → NaN |
| buy/sell volume    | **unavailable** (volume is aggregate) → NaN        |

Nothing is fabricated: unavailable metrics are NaN in CSVs and annotated
placeholders in plots. To support them, the Python API would need an extended
candle schema (or `load_trades_agg`) and `set_open_interest`/`load_oi`.

### Level-strength philosophy

No single "strength score" is produced. The framework exports raw observable
behavior per touch — rejection/breakout flags, penetration depth, forward
returns, volume response, MFE/MAE — and per-level aggregates
(`rejection_rate`, `breakout_rate`, mean returns). Whether that constitutes a
"strong level" is the researcher's decision, testable from `touches.csv`
(e.g. group by volume z-score and compare rejection probability).

## Web dashboards

Both share the engine (live feed + online levels + paper trading) and differ
only in the web layer.

### Dash: `examples/live_dashboard.py`

```bash
.venv/bin/python examples/live_dashboard.py --port 8050
# open http://127.0.0.1:8050/
```

Coin (free text, e.g. `DOGE/USDT`) + timeframe + date range (UTC) + speed +
window; data auto-loads via `market_data` (cache → Binance → synthetic
fallback, shown in the status line). OHLC **bars** (not candles), level lines,
pivot diamonds + confirming-window shading, touch dots, paper-trade
entry/exit markers. Levels table with pivot/confirm timestamps.

### Static Plotly: `examples/levels_web.py`

Builds one self-contained `dashboard.html` (plotly.js inlined, works offline)
with a level-selector dropdown. No server needed.

## Paper trading (live dashboard)

Deposit + risk-per-trade are set **once**; everything else is computed:

```
risk $      = equity × risk%
stop distance = |entry − stop|
size (base) = risk $ / stop distance
notional    = size × entry  (leverage-equiv = notional / equity)
```

- Entry / stop (+ optional take-profit) are drawn on the chart (blue/red/green
  lines + risk-zone shading); typing them shows a live sizing preview.
- Live P&L, MFE/MAE while open; auto-close on stop touch (stop has priority)
  or take-profit touch; manual close at market.
- Closed trades table: open/close time, direction, entry/exit/stop, size,
  P&L $/%, reason (STOP/TP/MANUAL), running equity. Next trade risks a % of
  the **current** equity. Changing coin/dates cancels an open trade with a note.

## Subtleties and gotchas (hard-won, read before debugging)

1. **The Rust engine is single-threaded by design** (`Rc<RefCell>` event bus).
   The PyO3 `Exchange` class is `unsendable`: touching it from any thread
   other than the creating one panics. Consequence: web servers **must run
   single-threaded** (`threaded=False` for Dash/Flask dev server), and all
   sim calls must happen in one thread — background button callbacks may only
   exchange plain data (intents/snapshots), never the `Exchange` object.
2. **Dash specifics.** A 1-second full-figure replace interrupts zoom gestures
   and can snap the view back: the app therefore has *follow mode* (explicit
   user zoom freezes figure updates until double-click autosize) plus a manual
   freeze button. `relayoutData` must be read as State and re-applied.
3. **pandas 2 vs 3 datetimes.** pandas 3 uses microsecond `datetime64[us]`;
   code converting to epoch ms must force nanoseconds first
   (`astype("datetime64[ns]")`), otherwise timestamps land in 1970. Also
   avoid `NaT` in datetime columns sent to web charts — use float
   millisecond columns with NaN instead.
4. **CSV timestamps.** `load_candles_from_csv` accepts epoch seconds,
   milliseconds (auto-detected) and ISO-8601 (`2026-03-01 00:00:00+00:00`).
   Pandas writes datetimes as strings — the Rust parser handles them.
5. **Volume-profile windowing.** Forward-looking aggregates must clamp to
   available bars (`bar covers target` check), otherwise touches near the end
   of data silently reuse the last bar and bias the curve.
6. **Determinism.** Same data + config + seed ⇒ byte-identical outputs
   (verified by checksum across runs). Synthetic generators are seeded;
   download caches make reruns offline-instant.
7. **Level detection choices.** Swing pivots need a prominence threshold
   (`min_swing`) and dedup tolerance, otherwise 1-second noise yields
   thousands of micro-levels; the touch detector must check the first bar of
   every batch against the *previous* batch's last close, or entries on batch
   boundaries (and everything at speed=1) are silently missed.
8. **Environment.** `uv.lock` is committed for reproducibility. Never commit
   `.venv/`, `target/`, `data/`, or generated `*_out/` dirs (gitignored).
   After any Rust change: `uv pip install --python .venv/bin/python -e .`.
   Never run two dashboard servers on one port; kill by exact PID (pattern
   `pkill -f` once killed the invoking shell itself mid-command).

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `Address already in use` | stale server alive — find exact PID via `/proc` scan, `kill` it (avoid `pkill -f`, it matched our own shell once) |
| Dash `500` on button callbacks | missing `allow_duplicate=True` on one of the writers, or touching `Exchange` from a worker thread (check single-threaded server) |
| Touches look random / missing | batch-boundary bars skipped (pass global prev close), or duplicate levels (dedup tolerance), or breakout threshold too small vs volatility |
| `fetch_ohlcv_live` returns nothing after `reset` | clock starts paused — call `resume()` before `step()` |
| Binance download slow/fails | normal for large ranges (paginated, ~0.15 s politeness delay); falls back to synthetic with a status note |
| `import simccxt` resolves to a stray `simccxt.so` | delete repo-root copies; the venv-installed package is canonical |

## License

MIT
