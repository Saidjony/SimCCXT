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
| Realtime API | WebSocket (`src/server`, JSON protocol)  |
| Terminal     | TypeScript + React + Lightweight Charts  |
| Tooling      | Bun (package manager) + Vite             |
| Storage      | CSV (in-memory / disk cache)             |
| Concurrency  | Deterministic single-threaded engine     |
| Serialization| serde, serde_json                        |

No Python in the runtime. No Pandas inside the engine. The optional
`src/python-api` (PyO3) bindings exist only for legacy research scripts in
`examples/` and are not part of the main application.

## Architecture

The project is a Cargo workspace with independent crates under `src/`. It is a
**pure exchange simulator**: Rust is the single source of truth for the market,
time, balances, orders, executions, positions and PnL; the WebSocket server is
only a thin boundary; TypeScript/React is only UI rendering.

```
src/core            fundamental types (Symbol, Order, Trade, Position, Balance, ...)
src/clock           controlled time (step/run/seek/pause/resume/reset)
src/market          order book, ticker, funding rate, open interest, candle series
src/storage         storage abstraction (in-memory, CSV)
src/event-bus       synchronous pub/sub (deterministic)
src/exchange        CCXT-compatible facade (fetch_*, create_order, cancel_order)
src/broker          order types + account settlement orchestration + fill log
                    + resting-limit sweep on every price update
src/order-engine    matching engine (market/limit, partial fills, fees, post-only,
                    IOC / FOK time-in-force)
src/portfolio       positions, cross/isolated margin, leverage, liquidation
src/wallet          balances, realized/unrealized PnL, fees, funding
src/server          WebSocket API for the TypeScript terminal (thin layer, no logic)
src/python-api      optional legacy PyO3 bindings (research scripts only)
```

Resting limit orders fill when the market trades through them: every replayed
bar sweeps crossed resting limits as maker (`Broker::sweep_resting`, settled
from funds locked at placement). Without the sweep they would hang open
forever — nothing else re-matches them.

```
Rust simccxt (engine + WebSocket server)
    │  WebSocket JSON (snapshot once, then deltas)
    ▼
TypeScript + React terminal (frontend/)
    └── TradingView Lightweight Charts (candles, drawings stay in the browser)
```

**Sessions (isolation).** Every WebSocket connection gets its own session: a
private `Exchange` (own 1M USDT paper account, orders, positions), own series,
own replay clock/speed, own push cursors. Every session event carries `to`
(the session's `client_id`) and each socket forwards only its own messages;
disconnect destroys the session. Two browsers = two users = zero shared state —
except the opt-in challenge board (nick + stats only, no `to`, for everyone).

The workspace deliberately contains **no research crates** — no indicators,
feature-engineering, clustering (KMeans), support/resistance levels, zones,
strategies, or ML. Everything between "load history" and "get state" runs in
Rust.

Terminal side:

```
frontend/src/app          TradingTerminal + App bootstrap
frontend/src/components   CoinList (right-panel watchlist: add/remove/flag +
                          ChallengeCard (nick + opt-in leaderboard), ReplayControls,
                          OrderPanel (deal ticket), Panels (full-width
                          Account/Positions/Orders/Trades tabs)
frontend/src/trade        criteria catalogue (30 ids + short overlay labels)
frontend/src/chart        three-panel timeframe workspace, market charts,
                          DrawBar (icon toolbar), HTML-overlaid drawings
frontend/src/websocket    singleton connection (reconnect, watchdog), router
frontend/src/state        tiny external store (React state/context level, no Redux)
frontend/src/types        explicit WebSocket protocol types (no `any`)
frontend/test             pure protocol/math tests (bun test / npm test)
```

## Terminal features

**Chart workspace.** Three charts: the main chart occupies the upper half,
with two independent charts side by side below. Defaults are 5m, 1h and 1d;
each chart has its own timeframe; criteria are saved per timeframe and shared
between panels showing that timeframe. The lower charts have
independent market-data feeds. Drag the dividers to resize the panels and the
main chart's indicator pane; layout sizes persist in the browser. Drawings are
projected onto each chart's nearest candle and price scale. Expand any chart to
fill the workspace and restore the three-panel layout with the same control.
The price scale is reset to auto-fit when new timeframe data arrives. The main
chart retains OHLC bars, volume, drawings, crosshair, zoom/pan and fills markers.

**Open interest + CVD** are separate resizable charts below the price chart.
Both indicators synchronize to the main chart by timestamp. OI is revealed
incrementally with replay candles; CVD is computed client-side over the visible
series. Toggle either with the `OI` / `CVD` buttons. If a venue does not expose
open-interest history, that pane reports that there is no OI data.

**Volume profile** (right side of the chart). Fixed-range horizontal volume
over the VISIBLE bars: 64 price levels, POC flagged with its price. Recomputed
on pan/zoom/data changes and positioned through the chart's current price
scale, so it tracks both visible bars and price-axis changes; clicks pass
through to the chart.

**Exchange and coin selection.** Choose Binance (default), Bybit or OKX in the
top bar. The Rust server uses CCXT's unified API for exchange-specific OHLCV and
available OI history, with exchange-scoped disk caches. Unsupported pairs or
timeframes are reported instead of silently displaying synthetic candles.
Paper orders and replay remain handled by the simulator.

**Coin list** (top card of the right panel, collapsible via the header).
Add coins (`BASE/QUOTE` input, Enter or `+`), remove them (`×`), flag them
(flag icon → the coin moves into the pinned "флаг" group above the rest).
Clicking a row switches the main chart's series on the selected exchange in Rust; removing the active
coin jumps to the next one. The list and its collapsed state persist per
browser (localStorage).

**Account, positions, orders and trades** are in the full-width terminal below
the charts; scroll down to view them. The account tab includes selected
criteria grouped by symbol and timeframe. Order/trade tables include trigger,
execution, fee, maker/taker and timestamp details. Filled entries and exits
are labeled on the chart; active entry and stop prices are drawn as horizontal
levels. The paper-trading order form is in the right panel under the
collapsible Challenge card.

**Replay date window** is configured from the compact controls in the top bar.
Choose the first visible-history date, the boundary after which bars reveal
one-by-one, and the last date (defaults to now). The same absolute range is
loaded independently for each chart's timeframe and exchange. Date ranges are
cached separately; a request is limited to 100,000 bars per chart.
`Space` toggles play/pause, `→` steps one bar, `Home` resets to the reveal
boundary, `Esc` cancels drawing, and `Delete` removes the selected drawing.

**Drawings** (icon toolbar, top-right inside the chart; all local, never trade
signals; drag to move with bar magnet, right-click / `Del` to delete,
`Esc` to disarm):

- ray level — 1 click, horizontal ray from the clicked bar, extends per tick;
- replay barrier — 1 click: resets the Rust clock to that bar, bars right of
  it hide and reveal one by one on play (dragging the line re-points it);
- zone rectangle — 2 clicks, HTML overlay;
- position long/short — 3 clicks (entry, stop, take-profit): TP/SL bands with
  reward/risk label; direction follows the stop side;
- ruler rectangle — 2 clicks: `% · bars · points`, auto-fades in 4 s, plain
  click restores the last measurement.

Multi-click placement uses manual mouseup detection (the chart's own click
event swallows rapid clicks and silently ate drawing points).

**Criteria** (right-panel card, collapsible via the header). 30 setup
checkboxes with a search filter; selected ones render as chips in the
chart's top-left overlay (max 8 + `+N`, clicks pass through to the chart).
Selection and collapsed state persist per browser (localStorage); pure UI,
never a trade signal.

**Challenge** (right-panel card + topbar badge). Passwordless nick: the server
assigns a random nick per session (`Быстрый-Бык-42`), typing your own overrides
it. The «участвовать» checkbox opts in: nick, fixed risk, capital, equity,
realized/unrealized PnL, R, fill count and symbol appear on the shared board
every participant sees. R = (realized + unrealized) / fixed risk; risk is fixed
for the current session's challenge round. Unrealized PnL and ranks refresh
with each replay bar. It lives in server memory only. Nick and the opt-in
persist per browser and restore on connect.

**Deal ticket** (`Сделка` card). Everything in USDT, no base-asset quantities:
capital + fixed risk per trade, entry-type dropdown (Рынок/Лимит/Стоп) with
conditional price/trigger fields, always-required stop-loss, BUY/SELL, one
preview line (`≈ qty · риск $`). Size = `risk / |entry − stop|`; entry is
blocked without a stop or when it is on the wrong side of the entry. A
protective reduce-only stop is armed after each entry fill (including delayed
limit/trigger fills), canceled when the position is closed, and can be canceled
from Orders. Positions and fills include detailed risk/execution data.

Legacy Python side (optional, not the app):

```
examples/_smoke_slice.py      vertical-slice smoke test (replay -> buy/sell -> PnL -> fills)
examples/ccxt_compatibility.py  runnable CCXT-style example
examples/realtime_simulator.py  step-by-step live feed demo
examples/market_data.py         stdlib-only disk cache + Binance download (cache-first)
```

```
data/ohlcv/                    local market-data cache (machine-local, gitignored)
```

## Building & Testing

### Rust (engine + WebSocket server)

```bash
cargo test --workspace          # run all unit tests
cargo clippy --workspace        # lint
cargo build --release --workspace
```

### Terminal (TypeScript + React, Bun preferred)

```bash
cd frontend
bun install                     # or: npm install
bun run dev                     # or: npm run dev  -> http://127.0.0.1:5173/
bun run build                   # or: npm run build
bun test                        # or: npm test
```

### Python environment (optional, legacy research scripts only)

```bash
uv venv --python 3.12                                  # create .venv (once)
uv pip install --python .venv/bin/python -e .          # build + install simccxt
```

`import simccxt` inside `.venv` resolves to the installed package — no
`PYTHONPATH` needed. Re-run `uv pip install -e .` after any Rust change.
The main app never needs this: it runs without Python entirely.

## Quick Start (terminal)

Terminal 1 — Rust engine + WebSocket API:

```bash
cargo run -p simccxt-server -- --symbol BTC/USDT --timeframe 1m
# listening on ws://127.0.0.1:9001/
```

Terminal 2 — TypeScript terminal:

```bash
cd frontend && bun install && bun run dev
# open http://127.0.0.1:5173/
```

The vertical slice works end to end: Rust replay → WebSocket → React →
Lightweight Charts → BUY button → Rust order execution → position/account
update → React. Press ▶, wait for bars, hit BUY, watch the position and PnL —
all computed in Rust.

## Quick Start (Python, optional legacy bindings)

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
- `create_order` (market, limit, stop, stop-limit, take-profit, take-profit-limit,
  trailing-stop, post-only, IOC, FOK, reduce-only)
- `cancel_order`
- `fetch_fills(symbol=None, limit=None)` — the account's own executed trades
  (fills produced by Rust execution), newest first
- `is_paused()` — whether the simulator clock is paused
- Time control: `step`, `run`, `pause`, `resume`, `seek`, `reset`

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

- `account_snapshot()` — compact one-call state for the UI: balances, equity,
  PnL, positions, open orders (all computed in Rust).
- `market_snapshot(symbol)` — last price / ticker for one symbol.
- `fetch_fills(symbol=None, limit=None)` — account's own trades (fills).
- `close_position(symbol, qty=None)` — market-counter a position to flat.
- `load_candles_from_csv(symbol, timeframe, path)` — load OHLCV from disk
  (`timestamp,open,high,low,close,volume`, ms/s or ISO-8601 timestamps).
- `reset(time_ms)` / `resume()` / `pause()` / `is_paused()` / `set_step_ms(ms)`
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

## WebSocket protocol (Rust ↔ TypeScript)

The terminal speaks to `simccxt-server` over one WebSocket
(`src/server/src/main.rs` documents the protocol; `frontend/src/types/protocol.ts`
mirrors it with explicit types, no `any`).

**One connection = one isolated session** (see Architecture): its own account,
series and replay clock. Every Rust → frontend session event carries `to`
(the session's `client_id`); the per-socket writer drops messages addressed
to other clients, so concurrent users never observe each other's state.

Frontend → Rust commands: `subscribe`, `place_order`, `cancel_order`,
`close_position`, `replay` (`play` / `pause` / `step` / `reset` / `set_speed` /
`set_step`), `login` (nick, empty = keep server-assigned), `challenge`
(`join` + `risk_usdt`), `ping`.

Rust → frontend events: `welcome`, `snapshot` (historical candles + account +
replay state, once), then only deltas: `candle` (new/updated bar),
`market_update`, `account_update`, `order_update`, `trade`, `replay_state`,
plus `login_ok`, `challenge_ok` and the `challenge_board` broadcast (no `to`:
every socket receives it).
Keep-alive: `heartbeat` every 10 s while quiet (paused replay), and `resync`
if a client lags behind (it re-subscribes for a fresh snapshot).

The chart applies `snapshot` via `setData()` and each `candle` via `update()` —
the full dataset is never resent per tick. Mouse interaction, zoom, pan and
drawings stay in the browser; trading commands are the only roundtrips.

## Vertical slice (replay → trade → PnL)

The whole loop is Rust-owned; the terminal only renders. The canonical
end-to-end check is now the WebSocket slice:

```bash
cargo run -p simccxt-server -- --symbol BTC/USDT --timeframe 1m
# subscribe -> snapshot -> place market BUY -> order_update + account_update
```

The legacy Python version of the same loop (`examples/_smoke_slice.py`) is kept
for the optional bindings:

```python
import simccxt
ex = simccxt.binance()
ex.add_market("BTC/USDT", "spot"); ex.deposit("USDT", 100_000.0)
ex.load_candles("BTC/USDT", "1m", rows)   # historical data
ex.reset(rows[0][0]); ex.resume(); ex.set_step_ms(60_000)
ex.step(); ex.step()                      # Rust replay sets a reference price
order = ex.create_order("BTC/USDT", "market", "buy", 0.5, None)  # Rust executes
print(ex.account_snapshot())              # position + PnL (Rust-computed)
print(ex.fetch_fills(None, None))         # account's own fills
```

## Data layer: disk cache + Rust CCXT

The Rust server itself implements "not on disk → fetch from the internet"
(`src/server/src/market_data.rs`: `ensure_series` / `download_series`) using
CCXT's unified exchange API, so the terminal needs no Python to load data:

- Order per `exchange@symbol@timeframe`: **already in memory → disk cache → CCXT
  download → synthetic fallback** (synthetic is always logged, never silent).
- Binance cache files retain `data/ohlcv/{SYMBOL}_{timeframe}.csv`;
  Bybit and OKX caches have an exchange prefix.
- Download: CCXT Rust is built with only the `binance`, `bybit` and `okx`
  features; no API keys are required for public OHLCV.
- Open interest is requested through CCXT where the selected venue supports
  unified open-interest history. Samples are cached per exchange and symbol.
- Online unsupported/invalid markets return a clear error instead of synthetic
  data. `--offline` disables downloads and uses deterministic synthetic data.
- `Engine::new` defaults to `allow_net=false`, so unit tests remain offline.
- Historical cache format: `ts_ms,open,high,low,close,volume` with millisecond
  timestamps.
  (explicitly reported in the server log).
- `examples/market_data.py` (stdlib-only legacy helper) can prefill the same
  cache files for date ranges; repeat runs are instant (cache hit).

`data/` is gitignored — cache is machine-local.

## Subtleties and gotchas (hard-won, read before debugging)

1. **The Rust engine is single-threaded by design** (`Rc<RefCell>` event bus).
    `simccxt-server` therefore owns the `Exchange` on one dedicated OS thread
    and talks to it through a message channel; WebSocket tasks never touch the
    engine directly. (The legacy PyO3 `Exchange` class is likewise `unsendable`.)
2. **CSV timestamps.** The Rust server accepts epoch seconds/milliseconds
    (auto-detected) and ISO-8601 (`2026-03-01 00:00:00+00:00`).
3. **Determinism.** Same data + config + seed ⇒ byte-identical outputs.
    Synthetic fallback data is seeded; download caches make reruns
    offline-instant.
4. **Environment.** Never commit `.venv/`, `target/`, `data/`,
    `frontend/node_modules/`, `frontend/dist/` or generated `*_out/` dirs
    (gitignored). Never run two servers on one port; find the exact PID via a
    `/proc` scan and `kill` it (pattern `pkill -f` once killed the invoking
    shell itself).

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `Address already in use` | stale server alive — find exact PID via `/proc` scan, `kill` it (avoid `pkill -f`, it matched our own shell once) |
| terminal shows `connecting…` | `simccxt-server` not running — start it first (`cargo run -p simccxt-server`); on localhost the terminal always dials `ws://127.0.0.1:9001` (override with `VITE_WS_URL`) |
| terminal shows `connected` but `data Ns ago` keeps growing | link is silent past the 10 s heartbeat — the watchdog reconnects automatically; check the server log for `disconnected (send-failed)` |
| rapid clicks drop drawing points | fixed: placement uses manual mouseup detection, the chart's click event swallows fast clicks |
| chart stays at 1 bar | clock starts paused after reset — press ▶ (play) |
| `fetch_ohlcv_live` returns nothing after `reset` (legacy Python API) | clock starts paused — call `resume()` before `step()` |
| Live download slow/fails | check server logs and exchange/API availability; online download errors are surfaced instead of substituting synthetic data (synthetic bars are used only in offline mode) |
| `import simccxt` resolves to a stray `simccxt.so` | delete repo-root copies; the venv-installed package is canonical (legacy bindings only) |

## License

MIT