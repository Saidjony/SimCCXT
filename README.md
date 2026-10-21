# simccxt

**Virtual Cryptocurrency Exchange** — a CCXT-compatible exchange simulator.

simccxt is NOT a traditional backtesting framework. It is a *virtual exchange* that fully
implements the CCXT interface, so a strategy cannot tell whether it is connected to
Binance, Bybit, OKX, or the simulator.

is replaceable with

```rust
use simccxt::Exchange;
let exchange = Exchange::new();
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

No Python in the runtime, no Pandas inside the engine: the engine is pure Rust
and the terminal is TypeScript. Market data is fetched by the Rust server
through CCXT's Rust API, so the only toolchain needed is cargo plus a JS
package manager.

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
src/wasm            Web Worker adapter exposing the same Exchange to the browser
src/server          market-data HTTP API, WebSocket session and diary/challenge mirror
```

Resting limit orders fill when the market trades through them: every replayed
bar sweeps crossed resting limits as maker (`Broker::sweep_resting`, settled
from funds locked at placement). Without the sweep they would hang open
forever — nothing else re-matches them.

```
Rust server ──HTTP candles / WebSocket session + diary mirror──▶ React terminal
                                                            ├── Web Worker + Rust/WASM Exchange
                                                            └── Lightweight Charts + drawings
```

**Sessions (isolation).** Every WebSocket connection gets its own session:
account identity, challenge participation, and diary mirror. Every session
event carries `to` (the session's `client_id`) and each socket forwards only
its own messages; disconnect destroys the session. Challenge statistics are
broadcast to all users.

**Replay execution.** The browser's Web Worker runs the same Rust `Exchange`
compiled to WebAssembly. Each tab owns its replay clock, speed, orders,
positions, fills, and PnL; changing one user's speed does not affect another
user or add per-candle matching work to the server. The server supplies the
selected history through `GET /api/candles` and receives throttled account/fill
mirrors for the live challenge board and diary. After the initial history load,
the worker sends only newly revealed candles and fills; the UI batches chart
updates for large histories instead of rebuilding or retransmitting the full
series on every replay tick. Keep the market-data cache on fast local storage
and warm it before users start replaying. When history is unavailable, replay
stays paused and reports the loading error rather than displaying fabricated
prices. If the WASM engine fails after real history has loaded, the client
reports the error and can fall back to server replay.

The browser's account, positions, open orders, and replay clock are in-memory
and are lost when the page is refreshed. The server's session mirror is
destroyed when its WebSocket disconnects or the server restarts. The browser
separately keeps the latest 500 fills in `localStorage`, so the trade log
survives a reload; that cache does not restore the account or resume replay.

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
                          DrawBar (icon toolbar), drawing engine
frontend/src/chart/primitives  primitives drawn by lightweight-charts itself
                          (drawings, pane legends; no HTML/SVG layer)
frontend/src/websocket    singleton connection (reconnect, watchdog), router
frontend/src/replay       local WASM Worker + engine lifecycle and server mirror
frontend/src/state        tiny external store (React state/context level, no Redux)
frontend/src/types        explicit WebSocket protocol types (no `any`)
frontend/test             pure protocol/math tests (bun test / npm test)
src/wasm                  WASM adapter for the existing Rust exchange engine
```

**Browser replay migration.** The terminal loads the selected candle window
and runs replay/trading commands in a per-tab Rust/WASM engine. The server
remains responsible for market-data loading, connection/session identity,
challenge aggregation, and the read-only diary API. A local account is mirrored
while connected; as with the prior in-memory server session, it is not a
persistent account and is lost when the page disconnects. `npm run build`
compiles the WASM module and requires the Rust `wasm32-unknown-unknown` target
to be installed.

## Terminal features

**Chart workspace.** Three charts: the main chart occupies the upper half,
with two independent charts side by side below. Defaults are 5m, 1h and 1d;
each chart has its own timeframe; criteria are saved per timeframe and shared
between panels showing that timeframe. The lower charts have
independent market-data feeds. Drag the dividers to resize the three panels;
that split persists in the browser. Drawings are projected onto each chart's
nearest candle and price scale. Expand any chart to
fill the workspace and restore the three-panel layout with the same control.
The price scale is reset to auto-fit when new timeframe data arrives. The main
chart retains OHLC bars, volume, drawings, crosshair, zoom/pan and fills markers.

**Open interest + CVD** are panes of the main chart itself (TradingView-style
sub-panes: `chart.addPane()`), not separate charts — one clock, one crosshair,
one zoom and one time axis, split by the library's own draggable dividers. OI is
revealed incrementally with replay candles, so samples the replay clock has not
reached yet stay hidden. The pane unions every OI grid cached for the symbol: a
grid only reaches back a fixed number of steps from its download moment (five
days at 5m), so an older coarse cache can still cover the history the replay
starts on where a fresh fine one cannot — an empty pane would otherwise be the
only thing a fine grid shows. CVD is computed client-side over the visible
series.
Each pane owns its legend chip and price axis, so indicator updates can never
push or jitter the price pane. `Follow: ON` scrolls to the newest bar as replay
advances; `OFF` preserves the manually selected view. Toggle panes with the
`OI` / `CVD` buttons: the indicator block starts at 120 px (split evenly between
the visible panes) and keeps whatever heights you drag its dividers to until the
set of panes changes. If a venue does not expose open-interest history, the pane
reports that there is no OI data.

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
are labeled and connected on the chart; active entry and stop prices are drawn
as horizontal levels. The paper-trading order form is in the right panel under the
collapsible Challenge card.

**Topbar.** Left to right: logo, venue picker, connection badge, nick, then the
chart timeframe picker, the ⌨ hotkeys hint, the warm-up progress, the replay
transport and the date-window button. The two popovers (hotkeys, dates) are
fixed-positioned: the topbar is a single scrolling row, so a menu positioned
inside it would be clipped to the bar's height and never open.

**Background market-data warm-up.** On connect, and whenever the watchlist,
venue or replay dates change, the frontend sends one `preload` command with
every watchlist symbol × every timeframe. Rust warms the disk caches for that
whole list on a separate thread, so clicking any coin or any timeframe later
resolves from disk instead of waiting for a download. It is a `preload` and
never a burst of `subscribe`s: a subscribe switches the session's live series
and answers with a full snapshot, so a burst would fight the chart on screen.
The badge in the topbar shows progress (`12/24`), turns ✓ when done and ⚠ if
some pairs failed (an unknown pair, or no network). Candles are warmed first
and open interest afterwards, so charts become switchable before the OI
pane. The loader yields the shared CCXT path to the engine between requests:
a user action never waits behind more than the download in flight, and cache
hits never take that path at all.

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
- zone rectangle — 2 clicks, filled rectangle primitive;
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
blocked without a stop or when it is on the wrong side of the entry. Paper
trades use linear positions, so BUY and SELL can open long and short positions.
A protective reduce-only stop is armed after each entry fill (including delayed
limit/trigger fills), canceled when the position is closed, and can be canceled
from Orders. Replay evaluates stop and limit triggers against candle ranges,
using low-before-high for bullish candles and high-before-low for bearish ones.
Positions and fills include detailed risk/execution data.

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

## Step-by-step replay (live-feed behaviour from disk)

simccxt delivers stored data step-by-step as if it were a live feed — sourced
from local memory/CSV on disk, fully deterministic. The terminal drives it
from the topbar (▶ play, +1 step, ⏮ reset) or with the keyboard
(`Space` / `→` / `Home`); the `Даты` popover sets the window that all charts
share, and the topbar timeframe picker chooses the interval.

### Clock semantics (important)

- `reset` puts the clock at the reveal boundary **and pauses it** — the
  transport resumes playback from there.
- A `snapshot` carries only bars at or before the clock; later bars arrive as
  `candle` deltas as the replay advances.
- Each chart has its own series and clock granularity (`step_ms` follows the
  selected timeframe), so one tick equals one bar of that chart.

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
(`join` + `risk_usdt`), `preload` (warm the caches for watchlist × timeframes),
`ping`.

Rust → frontend events: `welcome`, `snapshot` (historical candles + account +
replay state, once), then only deltas: `candle` (new/updated bar),
`market_update`, `account_update`, `order_update`, `trade`, `replay_state`,
plus `login_ok`, `challenge_ok`, `preload_progress` / `preload_done` and the
`challenge_board` broadcast (no `to`: every socket receives it).
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

In the terminal the same loop is: pick a coin and a timeframe (topbar), press
▶, click BUY, and watch the position, the fills and the PnL update — all of
them computed in Rust and pushed over the WebSocket.

## Data layer: disk cache + Rust CCXT

The Rust server itself implements "not on disk → fetch from the internet"
(`src/server/src/market_data.rs`: `ensure_series` / `download_series`) using
CCXT's unified exchange API, so the terminal loads no data itself:

- Order per `exchange@symbol@timeframe`: **already in memory → disk cache → CCXT
  download → synthetic fallback** (synthetic is always logged, never silent).
- Binance cache files retain `data/ohlcv/{SYMBOL}_{timeframe}.csv`;
  Bybit and OKX caches have an exchange prefix.
- Download: CCXT Rust is built with only the `binance`, `bybit` and `okx`
  features; no API keys are required for public OHLCV.
- Open interest is requested through CCXT where the selected venue supports
  unified open-interest history. Samples are cached per exchange and symbol;
  every grid cached for the symbol is merged into one windowed series (finer
  samples win on equal timestamps) and the legend names the grid the chart
  asked for while that grid still contributes.
- Online unsupported/invalid markets return a clear error instead of synthetic
  data. `--offline` disables downloads and uses deterministic synthetic data.
- `Engine::new` defaults to `allow_net=false`, so unit tests remain offline.
- Historical cache format: `ts_ms,open,high,low,close,volume` with millisecond
  timestamps.
  (explicitly reported in the server log).
- The `preload` command warms these same caches for the whole watchlist in the
  background; after it has run, repeat runs are instant (cache hit).
- A venue with no open-interest history is remembered on disk
  (`*_oi_*_none` marker), so that failed request is not retried on every
  subscribe.

`data/` is gitignored — cache is machine-local.

### Remote access through a VPS

The Rust listeners bind to `127.0.0.1` on ports `9001` (WebSocket) and `9002`
(diary HTTP API), not to a public interface. For a private server behind a
VPS, use reverse SSH forwarding to VPS loopback and terminate TLS on a VPS
reverse proxy; forward both ports because the browser uses the HTTP API as
well as WebSocket. Do not expose the raw ports directly: this protocol has no
login/authentication, and the diary HTTP API permits cross-origin reads.
Restrict access at the proxy/firewall until authentication and rate limiting
are added.

For 5–6 simultaneous sessions, replay computation is serialized but generally
light compared with uncached downloads and browser chart work. Load testing
must use the same history size, replay speed, cache state, VPS network path,
and browser hardware as production; there is no fixed CPU/RAM guarantee from
the number of connected users alone.

## Subtleties and gotchas (hard-won, read before debugging)

1. **The Rust engine is single-threaded by design** (`Rc<RefCell>` event bus).
    `simccxt-server` therefore owns the `Exchange` on one dedicated OS thread
    and talks to it through a message channel; WebSocket tasks never touch the
    engine directly. CCXT downloads are shared between that engine thread and
    the preloader thread, so a priority gate gives the network to the engine
    first and takes it per request — a click never queues behind a whole
    warm-up batch.
2. **CSV timestamps.** The Rust server accepts epoch seconds/milliseconds
    (auto-detected) and ISO-8601 (`2026-03-01 00:00:00+00:00`).
3. **Determinism.** Same data + config + seed ⇒ byte-identical outputs.
    Synthetic fallback data is seeded; download caches make reruns
    offline-instant.
4. **Environment.** Never commit `target/`, `data/`,
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
| a coin takes long to open | its series is not cached yet — the topbar badge shows the warm-up progress; after `✓` every listed pair is local |
| topbar badge shows ⚠ | some pairs failed (unknown symbol, or the venue has no data for it) — the log line names them; the rest still work |
| Live download slow/fails | check server logs and exchange/API availability; online download errors are surfaced instead of substituting synthetic data (synthetic bars are used only in offline mode) |

## License

MIT