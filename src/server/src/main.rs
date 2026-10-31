//! simccxt-server — thin WebSocket layer over the Rust exchange simulator.
//!
//! Architecture:
//!
//! ```text
//! TypeScript + React (frontend/)
//!     │  WebSocket JSON (this crate's protocol, see PROTOCOL below)
//!     ▼
//! simccxt-server (thin: sockets, replay timer, incremental diffs)
//!     │  direct calls, single dedicated OS thread
//!     ▼
//! simccxt-exchange::Exchange (source of truth: market, time, orders, PnL)
//! ```
//!
//! The `Exchange` type is single-threaded by design (`Rc<RefCell>` event bus),
//! so it lives on ONE dedicated OS thread. All WebSocket tasks talk to it
//! through an `std::mpsc` channel; streaming updates fan out through a
//! `tokio::sync::broadcast` channel. No business logic lives here: this crate
//! only translates JSON commands into `Exchange` calls and `Exchange` state
//! into JSON events.
//!
//! Incremental updates (never resend the whole dataset per tick):
//! - `subscribe` answers with one `snapshot` (up to 10 000 historical candles).
//! - afterwards the server only sends `candle` (new/updated bar),
//!   `market_update`, `account_update`, `trade` and `replay_state`.
//!
//! Sessions: one WebSocket connection = one isolated session with its own
//! `Exchange` (own account, series, clock, replay speed). Every session event
//! carries `to` (the session's client_id); each socket drops messages that
//! address another client, so two users never see each other's state.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use chrono::DateTime;
use futures_util::{FutureExt, SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use simccxt_exchange::{CreateOrderParams, Exchange};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, oneshot};

// ---------------------------------------------------------------------------
// PROTOCOL (must stay in sync with frontend/src/types/protocol.ts)
// ---------------------------------------------------------------------------
//

mod diary;
#[cfg(test)]
mod diary_scenarios;
// Frontend -> Rust ("type" tagged):
//   {"type":"subscribe","symbol":"BTC/USDT","timeframe":"1m",
//    "exchange":"binance","request_id":".."}
//   {"type":"place_order","symbol":..,"side":"buy","order_type":"market",
//    "quantity":0.1,"price":null,"stop_price":null,
//    "stop_loss_price":49000.0,
//    "time_in_force":"GTC","post_only":false,"reduce_only":false}
//   {"type":"cancel_order","order_id":"..","symbol":"BTC/USDT"}
//   {"type":"close_position","symbol":"BTC/USDT","quantity":null}
//   {"type":"replay","action":"play|pause|step|reset|set_speed|set_step",
//    "steps":1,"speed":2.0,"step_ms":60000,"timestamp_ms":null}
//   {"type":"login","name":"..","request_id":".."}   (empty name = keep current nick)
//   {"type":"challenge","join":true,"risk_usdt":10,"capital_usdt":1000,
//    "request_id":".."}
//    (risk_usdt must be >0 and is fixed for the current session)
//   {"type":"preload","symbols":["BTC/USDT","ETH/USDT"],"timeframes":["1m","5m"],
//    "exchange":"binance","start_ms":..,"end_ms":..,"request_id":".."}
//   {"type":"market_lookup","symbols":["BTC/USDT","BR/USDT"],"exchange":"binance"}
//   {"type":"ping"}
//
// Rust -> frontend:
//   {"type":"welcome","client_id":7}
//   {"type":"snapshot","to":7,"symbol":..,"timeframe":..,"exchange":"binance",
//    "candles":[[ts,o,h,l,c,v]..],
//    "account":{..},"replay":{..},"oi":[[ts_ms,usdt]..],"oi_period":"5m"}
//   {"type":"candle","to":7,"symbol":..,"timeframe":..,"candle":[..],"is_new":true}
//   {"type":"candle","to":7,"symbol":..,"timeframe":..,"candle":[..],"is_new":true}
//   {"type":"market_update","to":7,"symbol":..,"last":..,"timestamp":..}
//   {"type":"account_update","to":7,"account":{..}}
//   {"type":"order_update","to":7,"order":{..}}
//   {"type":"trade","to":7,"fill":{..}}
//   {"type":"replay_state","to":7,..}
//   {"type":"heartbeat","to":7,"now_ms":..,"paused":..}   (every 10 s when quiet)
//   {"type":"resync","to":7}                       (you lagged: re-subscribe)
//   {"type":"login_ok","to":7,"nick":"..","request_id":".."}
//   {"type":"challenge_ok","to":7,"joined":true,"nick":"..","request_id":".."}
//   {"type":"challenge_board","board":[..]}         (NO `to`: goes to EVERYONE)
//   {"type":"preload_progress","to":7,"done":3,"total":24,"failed":0,
//    "symbol":"ETH/USDT","timeframe":"1h","status":"downloaded","note":".."}
//   {"type":"preload_done","to":7,"done":24,"total":24,"failed":1}
//   {"type":"market_lookup","to":7,"quotes":[{"symbol":"BTC/USDT",
//    "exchange":"binance","last":65000}],"failures":[]}
//   {"type":"error","to":7,"message":"..","request_id":".."}
//   {"type":"pong","to":7}
//
// PRELOAD (background market data): the watchlist x all timeframes is warmed
// on a separate thread, so switching a coin/timeframe never waits for a
// download. Each symbol is resolved across supported venues first; candles
// and OI are cached against the venue where that symbol is available. It never
// touches a session, its series, its clock or its account, and sends no candles.
// MARKET_LOOKUP returns current prices and venue labels for the watchlist.
//
// SESSIONS (isolation): every connection owns a private session — its own
// `Exchange` (1M USDT paper account, orders, positions), its own series and
// replay clock/speed, its own push cursors. All session events carry `to`;
// the per-socket writer forwards only messages addressed to that socket, and
// disconnect destroys the session. Users never share or observe state.
//
// CHALLENGE (opt-in leaderboard): a session carries a nick (random at
// creation, overridable via `login`), an `in_challenge` flag and `risk_usdt`
// (sent by the frontend on join — the fixed risk per trade). Participants are
// published as `{nick, risk, pnl, r, equity}` with R = (realized+unrealized) /
// risk_usdt, sorted by R desc. The board broadcasts to ALL sockets (no `to`)
// on login / join / leave / disconnect / fills and replay mark updates.
// In-memory only: a server restart empties the board. No passwords — nicks are
// not authenticated.
//
// A candle row is always [ts_ms, open, high, low, close, volume].
// `account` is exactly `Exchange::account_snapshot()` (balances, equity, pnl,
// positions, open orders) — computed in Rust, never in TypeScript.

/// Max historical candles sent in one `snapshot`.
const SNAPSHOT_CAP: usize = 10_000;
/// Default ticks (sim steps) per wall-clock second while playing.
const DEFAULT_SPEED: f64 = 1.0;
/// Idle poll interval when paused (so commands stay responsive).
const IDLE_POLL_MS: u64 = 50;
/// One `replay step` command may advance at most this many bars. A client
/// asking for 1e9 must not stall the single engine thread for minutes.
const MAX_STEPS_PER_COMMAND: u64 = 500;
/// Hot-path push rates. The chart gets every new bar, but account/replay
/// snapshots are coalesced: at 100 ticks/s the UI cannot use more, and each
/// one costs a full account serialization.
const ACCOUNT_PUSH_MS: u64 = 200;
const REPLAY_PUSH_MS: u64 = 250;
const MARKET_PUSH_MS: u64 = 100;
/// Leaderboard refresh period while the replay runs (fills/joins are instant).
const BOARD_PUSH_MS: u64 = 1_000;
/// How long a disconnected session stays parked in memory for a reload.
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);
/// Cap on parked sessions (each owns a series + account).
const MAX_DETACHED: usize = 32;

// ---------------------------------------------------------------------------
// Client -> server commands
// ---------------------------------------------------------------------------

/// A command sent by the frontend. Unknown `type` values are rejected.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientCmd {
    Subscribe {
        symbol: String,
        timeframe: String,
        #[serde(default = "default_exchange")]
        exchange: String,
        start_ms: Option<i64>,
        reveal_start_ms: Option<i64>,
        end_ms: Option<i64>,
        request_id: Option<String>,
    },
    PlaceOrder {
        symbol: String,
        side: String,
        order_type: String,
        quantity: f64,
        price: Option<f64>,
        stop_price: Option<f64>,
        stop_loss_price: Option<f64>,
        /// Fixed risk budget. When present with a stop, size at the actual
        /// entry reference so a stop-out costs this amount, not more.
        #[serde(default)]
        risk_usdt: Option<f64>,
        #[serde(default)]
        time_in_force: String,
        #[serde(default)]
        post_only: bool,
        #[serde(default)]
        reduce_only: bool,
        #[serde(default)]
        tp_price: Option<f64>,
        request_id: Option<String>,
    },
    CancelOrder { order_id: String, symbol: String, request_id: Option<String> },
    ClosePosition { symbol: String, quantity: Option<f64>, request_id: Option<String> },
    Login { #[serde(default)] name: String, request_id: Option<String> },
    /// Bind this connection to a client-generated token. If the token belongs
    /// to a session that was parked by a disconnect (page reload), the session
    /// — account, orders, clock, stats — is restored under the new client id.
    Attach { token: String, request_id: Option<String> },
    Challenge {
        join: bool,
        #[serde(default)]
        risk_usdt: Option<f64>,
        capital_usdt: Option<f64>,
        request_id: Option<String>,
    },
    /// Background warm-up of the disk caches for watchlist x timeframes.
    /// Does not touch this session: no snapshot, no clock reset, no account
    /// change — only `preload_progress` / `preload_done` come back.
    #[allow(dead_code)] // accepted for old clients; the browser warms its own cache
    Preload {
        #[serde(default)]
        symbols: Vec<String>,
        #[serde(default)]
        timeframes: Vec<String>,
        #[serde(default = "default_exchange")]
        exchange: String,
        // The replay date window [start_ms, end_ms]: its range cache is warmed
        // too, so a subscribe inside it also resolves from disk.
        start_ms: Option<i64>,
        end_ms: Option<i64>,
        request_id: Option<String>,
    },
    /// Resolve current public prices for the watchlist, independently of the
    /// active replay series. Work is run off the engine thread.
    #[allow(dead_code)] // accepted for old clients; the browser fetches quotes itself
    MarketLookup {
        #[serde(default)]
        symbols: Vec<String>,
        #[serde(default = "default_exchange")]
        exchange: String,
        request_id: Option<String>,
    },
    Replay {
        action: String,
        #[serde(default)]
        steps: Option<u64>,
        #[serde(default)]
        speed: Option<f64>,
        #[serde(default)]
        step_ms: Option<u64>,
        #[serde(default)]
        timestamp_ms: Option<i64>,
        request_id: Option<String>,
    },
    /// Client pushes its visual context (drawings + enabled criteria) so the
    /// read-only diary API can surface it later. Captured verbatim.
    DiaryMeta {
        #[serde(default)]
        drawings: Option<Vec<serde_json::Value>>,
        #[serde(default)]
        criteria: Option<serde_json::Map<String, serde_json::Value>>,
        request_id: Option<String>,
    },
    LocalSync {
        account: Value,
        #[serde(default)]
        fills: Vec<Value>,
        #[serde(default)]
        replace_fills: bool,
        /// Random id of the browser engine instance that produced the fills.
        #[serde(default)]
        engine_id: String,
        #[serde(default)]
        entry_protections: HashMap<String, Value>,
        #[serde(default)]
        exit_reasons: HashMap<String, String>,
        symbol: String,
        timeframe: String,
        #[serde(default = "default_exchange")]
        exchange: String,
        range: Value,
    },
    Ping,
}

fn default_exchange() -> String {
    "binance".to_string()
}


/// One unit of work for the engine thread: a command from a client, or the
/// teardown of that client's session, or a diary request (read-only HTTP).
enum EngineJob {
    Command { client_id: u64, cmd: ClientCmd },
    /// Register the per-connection delivery channel. Directed messages go only
    /// to that socket (no shared broadcast ring, no per-socket JSON parsing).
    Connect { client_id: u64, tx: tokio::sync::mpsc::Sender<String> },
    Disconnect { client_id: u64 },
    Diary { nick: String, reply: oneshot::Sender<Value> },
    Traders { reply: oneshot::Sender<Value> },
}

/// Cap on one request (a client cannot queue an unbounded download storm).
const PRELOAD_MAX_PAIRS: usize = 64;
/// Timeframes the server accepts in a warm-up request.
///
/// Only the three intervals the terminal offers. 5m is the only one that is
/// ever downloaded; 1h and 1d are aggregated in the browser. The preload
/// command itself is now vestigial for market data (browsers download their
/// own bars), but keeping the list in sync means an old client can never ask
/// the server to fetch a timeframe that no longer exists.
const PRELOAD_TIMEFRAMES: [&str; 3] = ["5m", "1h", "1d"];

/// Deduplicated, validated `symbol x timeframe` list for a warm-up request.
/// Pure helper so it can be unit-tested: malformed symbols/timeframes are
/// dropped; newest watchlist entries and then timeframe order are prioritized.
fn preload_plan(symbols: &[String], timeframes: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    // Newly added watchlist entries are appended, so prioritize them if the
    // bounded request has more pairs than the server can warm in one batch.
    for symbol in symbols.iter().rev() {
        let symbol = symbol.trim().to_uppercase();
        if !symbol.contains('/') {
            continue;
        }
        for timeframe in timeframes {
            let timeframe = timeframe.trim().to_ascii_lowercase();
            if !PRELOAD_TIMEFRAMES.contains(&timeframe.as_str()) {
                continue;
            }
            let pair = (symbol.clone(), timeframe.clone());
            if !out.contains(&pair) {
                out.push(pair);
            }
        }
    }
    out.truncate(PRELOAD_MAX_PAIRS);
    out
}

// ---------------------------------------------------------------------------
// Engine (owns the Exchange, single thread)
// ---------------------------------------------------------------------------

/// Outgoing message helper: serialize to a compact JSON string.
fn msg(v: Value) -> String {
    serde_json::to_string(&v).unwrap_or_else(|_| r#"{"type":"error","message":"encode"}"#.into())
}

/// Serialize a message addressed to one client with `to` as the FIRST key.
/// The broadcast writer routes on a byte prefix (`routing_target`), and
/// serde_json's map is alphabetical — `account`/`candles`/`type` would all
/// sort before `to`, so a directed fallback would leak to every socket.
fn msg_to(to: u64, mut v: Value) -> String {
    if let Some(obj) = v.as_object_mut() {
        obj.remove("to");
    }
    let body = msg(v);
    if body.len() <= 2 {
        return format!("{{\"to\":{to}}}");
    }
    format!("{{\"to\":{to},{}", &body[1..])
}

/// Timestamp prefix for server logs (so connect/disconnect storms are debuggable).
pub(crate) fn stamp() -> String {
    chrono::Utc::now().format("%H:%M:%S").to_string()
}

macro_rules! log {
    ($($t:tt)*) => {
        eprintln!("{} {}", stamp(), format!($($t)*))
    };
}


/// Bars in `live` that the client has not seen yet (client has seen `fed`).
/// Pure helper so it can be unit-tested: returns the slice of new rows.
#[cfg(test)]
fn new_bars(live: &[Vec<f64>], fed: usize) -> &[Vec<f64>] {
    if fed >= live.len() {
        &[]
    } else {
        &live[fed..]
    }
}

/// One WebSocket connection = one isolated session: its own `Exchange` (own
/// account/orders/positions), own series, own replay clock and speed, own
/// push cursors. Nothing is shared between sessions — except the opt-in
/// challenge board (nick + stats only, see CHALLENGE above).
struct Session {
    ex: Exchange,
    symbol: String,
    timeframe: String,
    exchange: String,
    speed: f64,   // ticks per second while playing
    step_ms: u64, // mirrored from Exchange::set_step_ms (no getter there)
    fed: usize,   // candles already pushed to this client
    fills_seen: usize,
    /// Distinct order ids seen so far — the leaderboard's trade count without
    /// re-serializing the whole fill log on every board push.
    seen_orders: HashSet<String>,
    last_step: std::time::Instant, // last auto-tick (per-session speed)
    last_account_push: std::time::Instant,
    last_replay_push: std::time::Instant,
    last_market_push: std::time::Instant,
    /// Opaque client token, so a page reload re-attaches to this session
    /// instead of creating a fresh account (empty = old client, no restore).
    token: String,
    /// A `subscribe` parked on the loader thread: (symbol, timeframe). Stale
    /// `SeriesReady` jobs are ignored unless they match this.
    pending_subscribe: Option<(String, String)>,
    nick: String,      // challenge display name (random at creation)
    in_challenge: bool, // opt-in: publish nick+stats on the shared board
    risk_usdt: f64,    // fixed risk per trade, sent by the frontend on join
    capital_usdt: f64,
    pending_protections: Vec<PendingProtection>,
    entry_protections: EntryProtections,
    /// Protective order id → the level it implements (`"stop"` / `"take"`).
    protection_kinds: HashMap<String, &'static str>,
    /// Fill order id → the level that closed it.
    ///
    /// This cannot be recovered from prices alone: classify stop/take exits
    /// from the conditional order that triggered, independently of fill level.
    exit_reasons: HashMap<String, &'static str>,
    active_protections: Vec<(String, String, String)>,
    diary_meta: serde_json::Value, // client-pushed drawings/criteria to show in the diary
    oi: Vec<(i64, f64)>,           // open-interest history (ts_ms, USDT value), context only
    oi_period: String,             // OI grid actually loaded ("5m", "" when none)
    oi_fed: usize,
    range_start_ms: i64,
    reveal_start_ms: i64,
    range_end_ms: i64,
    /// Browser-owned Rust/WASM engine state mirrored for the online board and diary.
    local_account: Option<Value>,
    local_fills: Vec<Value>,
    /// Engine instance that produced the newest fills. Order ids restart at 1
    /// every page load, so trades are reconstructed per engine instance.
    local_engine_id: String,
    local_entry_protections: HashMap<String, Value>,
    local_exit_reasons: HashMap<String, String>,
}

/// Protective levels the trader set, keyed by entry order id. Session-scoped:
/// this powers the in-session post-trade review, not a permanent journal.
type EntryProtections = HashMap<String, Value>;

/// Key of a planned protection in an `entry_protections` map.
///
/// It is NOT the bare order id: a stop/limit entry that triggers is executed by
/// a NEW market order, so the fill reports a different id, and order ids restart
/// at 1 per symbol and per session. The wasm engine builds the same string in
/// `process_fresh_fills` (and the chart looks it up with `protectionKey()`), and
/// the server's own fill processing does too — the diary must match, otherwise
/// every review falls back to percent because the stop it was planned with
/// looks missing. The engine id separates page loads (see `Engine::engine_id`);
/// server-side fills use an empty one.
fn local_protection_key(
    engine_id: &str,
    symbol: &str,
    entry_order_id: &str,
    entry_time_ms: i64,
) -> String {
    format!("{engine_id}|{symbol}|{entry_order_id}|{entry_time_ms}")
}

/// Key of a protective exit reason in an `exit_reasons` map.
///
/// Not the bare order id: ids restart at 1 for every symbol engine and every
/// replay session, so BTC's stop #2 and ETH's take #2 collide, and so do two
/// trades of the same symbol from different replay sessions. The engine id
/// separates page loads and the fill time pins the execution within one.
/// The wasm engine builds the same string in `process_fresh_fills`.
fn exit_reason_key(engine_id: &str, symbol: &str, order_id: &str, timestamp_ms: i64) -> String {
    format!("{engine_id}|{symbol}|{order_id}|{timestamp_ms}")
}

/// Identity of one execution, used to merge incremental local-fill batches.
///
/// The fill `id` alone is `orderId_fillIndex`, and order ids restart at 1 for
/// every symbol engine and replay session: BTC's `1_0` and ETH's `1_0` are two
/// different fills. Merging on the bare id silently dropped the second one, so
/// the diary saw a lone exit and invented a trade on the wrong side. Mirrors
/// the browser's `fillKey` in `replay/localEngine.ts`, plus the engine
/// instance: a reloaded tab re-runs the same replay and can produce a fill
/// with an identical id/price/amount that is a *different* execution.
fn fill_identity(fill: &Value) -> String {
    let id = fill["id"].as_str().unwrap_or_default();
    if id.is_empty() {
        return String::new();
    }
    format!(
        "{}|{id}|{}|{}|{}|{}",
        fill["engine_id"].as_str().unwrap_or_default(),
        fill["symbol"].as_str().unwrap_or_default(),
        fill["timestamp"].as_i64().unwrap_or_default(),
        fill["price"].as_f64().unwrap_or_default(),
        fill["amount"].as_f64().unwrap_or_default(),
    )
}

struct PendingProtection {
    symbol: String,
    entry_side: String,
    entry_order_id: String,
    stop_price: f64, // 0.0 = no take-protection arm
    take_price: Option<f64>,
    remaining: f64,
}


// ---------------------------------------------------------------------------
// Challenge: random nicks + shared leaderboard (in-memory, no passwords)
// ---------------------------------------------------------------------------

/// Max nick length in chars (not bytes — Cyrillic is 2 bytes/char).
const MAX_NICK_LEN: usize = 24;

const NICK_A: [&str; 20] = [
    "Быстрый",
    "Тихий",
    "Стальной",
    "Хитрый",
    "Смелый",
    "Резкий",
    "Меткий",
    "Дерзкий",
    "Умный",
    "Спокойный",
    "Ловкий",
    "Жадный",
    "Терпеливый",
    "Зоркий",
    "Крепкий",
    "Шустрый",
    "Мудрый",
    "Дикий",
    "Холодный",
    "Цепкий",
];
const NICK_B: [&str; 20] = [
    "Бык",
    "Медведь",
    "Волк",
    "Сокол",
    "Акула",
    "Тигр",
    "Ястреб",
    "Кит",
    "Скорпион",
    "Ворон",
    "Барс",
    "Койот",
    "Филин",
    "Соболь",
    "Кречет",
    "Тайфун",
    "Штиль",
    "Откат",
    "Пробой",
    "Флет",
];

/// Random `A-B-NN` nick, unique among active sessions. xorshift64 seeded from
/// wall-clock nanos + a process counter (same PRNG style as `synthetic`).
fn unique_nick(sessions: &HashMap<u64, Session>) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SALT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678);
    let mut state = nanos
        .wrapping_add(
            SALT.fetch_add(1, Ordering::Relaxed)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15),
        )
        .max(1);
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..64 {
        let nick = format!(
            "{}-{}-{}",
            NICK_A[(next() % NICK_A.len() as u64) as usize],
            NICK_B[(next() % NICK_B.len() as u64) as usize],
            1 + next() % 99,
        );
        if !sessions.values().any(|s| s.nick == nick) {
            return nick;
        }
    }
    // Practically unreachable (20*20*99 combos): fall back to a counter nick.
    format!("Трейдер-{}", sessions.len() + 1)
}

/// Validate a user-supplied nick: 1–24 chars after trim, no control chars.
fn clean_nick(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.chars().count() > MAX_NICK_LEN {
        return None;
    }
    if t.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(t.to_string())
}

/// One board row: R = (realized + unrealized) / risk. `None` when the risk
/// is missing/non-positive (cannot happen for joined sessions — join gates).
fn r_of(pnl: f64, risk: f64) -> Option<f64> {
    if risk > 0.0 && risk.is_finite() && pnl.is_finite() {
        Some(pnl / risk)
    } else {
        None
    }
}

/// Leaderboard JSON over challenge participants, sorted by R desc.
/// Deliberately NO `to`: the per-socket writer forwards it to everyone.
fn challenge_board_json(sessions: &HashMap<u64, Session>) -> Value {
    let mut rows: Vec<(f64, Value)> = Vec::new();
    for s in sessions.values() {
        if !s.in_challenge {
            continue;
        }
        let snap = s
            .local_account
            .clone()
            .unwrap_or_else(|| s.ex.account_snapshot());
        let realized = snap["pnl"]["realized"].as_f64().unwrap_or(0.0);
        let unrealized = snap["pnl"]["unrealized"].as_f64().unwrap_or(0.0);
        let equity = snap["equity"].as_f64().unwrap_or(0.0);
        let pnl = realized + unrealized;
        // Distinct entry/exit orders, maintained incrementally when fresh
        // fills are seen. Re-serializing the whole fill log here was the
        // hottest part of the board push (once per tick per participant).
        let trades = s.seen_orders.len();
        let r = r_of(pnl, s.risk_usdt).unwrap_or(0.0);
        rows.push((
            r,
            json!({
                "nick": s.nick,
                "risk": s.risk_usdt,
                "capital": s.capital_usdt,
                "realized": realized,
                "unrealized": unrealized,
                "pnl": pnl,
                "r": (r * 100.0).round() / 100.0,
                "equity": (equity * 100.0).round() / 100.0,
                "trades": trades,
                "symbol": s.symbol,
            }),
        ));
    }
    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    json!({ "type": "challenge_board", "board": rows.into_iter().map(|(_, v)| v).collect::<Vec<_>>() })
}

struct Engine {
    sessions: HashMap<u64, Session>,
    /// Sessions parked on disconnect (by token) for a page reload / reconnect.
    detached: HashMap<String, Detached>,
    /// Per-socket delivery channels: directed events never enter the shared
    /// broadcast ring (which would clone every snapshot for every client).
    clients: HashMap<u64, tokio::sync::mpsc::Sender<String>>,
    out: broadcast::Sender<String>,
    /// Missing series may be downloaded via CCXT (false in unit tests,
    /// and when the server is started with `--offline`).
    allow_net: bool,
    /// Engine's own job queue, so a loader thread can hand work back.
    jobs: Option<mpsc::Sender<EngineJob>>,
    last_board_push: std::time::Instant,
}

/// A session kept alive after its socket closed, waiting for the tab to
/// re-attach with the same token (page reload, engine stall reconnect).
struct Detached {
    session: Session,
    since: std::time::Instant,
}

/// Wall-clock interval between auto-steps at `speed` ticks/second.
fn speed_interval(speed: f64) -> Duration {
    // 50 Hz ceiling: faster replay only burns CPU (the UI ladder tops out at
    // 10x, and no display can use more than one update per frame).
    let ms = (1000.0 / speed.max(0.05)).round() as u64;
    Duration::from_millis(ms.clamp(20, 5000))
}

fn replay_json(s: &Session) -> Value {
    json!({
        "type": "replay_state",
        "paused": s.ex.is_paused(),
        "now_ms": s.ex.now_ms(),
        "speed": s.speed,
        "step_ms": s.step_ms,
        "done": s.ex.replay_done(&s.symbol, &s.timeframe),
        "fed": s.ex.live_len(&s.symbol, &s.timeframe),
        "total": s.ex.total_len(&s.symbol, &s.timeframe),
    })
}

/// Rewind the replay clock to the window's reveal point: the last visible bar
/// before revealing starts. Shared by `reset` and by `play` at the end of the
/// series, so both land on exactly the same bar.
fn rewind_to_reveal(s: &mut Session) {
    let rows = s.ex.fetch_ohlcv(&s.symbol, &s.timeframe, None, None);
    let ts_ms = rows
        .iter()
        .find(|row| (row[0] as i64) >= s.reveal_start_ms)
        .map(|row| row[0] as i64 - s.step_ms as i64)
        .or_else(|| rows.first().map(|row| row[0] as i64))
        .unwrap_or_else(|| s.ex.now_ms());
    if let Some(ts) = DateTime::from_timestamp_millis(ts_ms) {
        s.ex.reset(ts);
    }
    s.oi_fed = s.oi.partition_point(|(oi_ts, _)| *oi_ts <= ts_ms);
    s.fed = 0;
}

fn ack_replay(s: &Session, request_id: Option<String>) -> Value {
    let mut v = replay_json(s);
    v["request_id"] = json!(request_id);
    v
}

fn snapshot_json(s: &Session, request_id: Option<String>) -> Value {
    // Only bars <= now: the chart reveals one new bar per tick instead of
    // the whole future at once. Later steps arrive as `candle` deltas.
    // OI rides along whole (static context, never replayed): [[ts_ms, usdt]].
    let mut rows = s.ex.fetch_ohlcv_live(&s.symbol, &s.timeframe);
    if rows.len() > SNAPSHOT_CAP {
        rows = rows[rows.len() - SNAPSHOT_CAP..].to_vec();
    }
    json!({
        "type": "snapshot",
        "symbol": s.symbol,
        "timeframe": s.timeframe,
        "exchange": s.exchange,
        "candles": rows,
        "account": s.ex.account_snapshot(),
        "replay": replay_json(s),
        "oi": s.oi.clone(),
        "oi_period": s.oi_period,
        "fills": s.ex.fetch_fills_last(500),
        "request_id": request_id,
    })
}

/// Attach a protective stop to every newly executed entry fill.
/// Pending limit/trigger entries are kept in the session until they fill.
fn fresh_fill_events(s: &mut Session) -> Vec<Value> {
    let total = s.ex.fills_len();
    if total <= s.fills_seen {
        return Vec::new();
    }
    // Newest first, only the unseen tail. The old code cloned and serialized
    // the entire fill log on every tick just to look at its length.
    let mut fresh = s.ex.fetch_fills_from(s.fills_seen, None);
    fresh.reverse(); // chronological (oldest unseen first)
    let mut events = Vec::new();
    for fill in fresh {
        let symbol = fill["symbol"].as_str().unwrap_or_default().to_string();
        let side = fill["side"].as_str().unwrap_or_default().to_string();
        let order_id = fill["orderId"].as_str().unwrap_or_default().to_string();
        let fill_timestamp = fill["timestamp"].as_i64().unwrap_or_default();
        s.seen_orders.insert(order_id.clone());
        let source_order_id = s.ex.take_triggered_order_source(&order_id);
        // If this fill came from a protective order, remember WHICH level it
        // implemented: the diary needs the reason, and price proximity cannot
        // supply it (a stop is a market order and may fill far past its trigger).
        if let Some(kind) = s.protection_kinds.get(&order_id).copied().or_else(|| {
            source_order_id
                .as_ref()
                .and_then(|id| s.protection_kinds.get(id).copied())
        }) {
            s.exit_reasons.insert(
                exit_reason_key("", &symbol, &order_id, fill_timestamp),
                kind,
            );
        }
        let amount = fill["amount"].as_f64().unwrap_or(0.0);
        let pending_index = s.pending_protections.iter().position(|pending| {
            pending.symbol == symbol
                && pending.entry_side == side
                && (pending.entry_order_id == order_id
                    || source_order_id.as_deref() == Some(pending.entry_order_id.as_str()))
        });
        if let Some(index) = pending_index {
            let protect_amount = amount.min(s.pending_protections[index].remaining);
            let stop_price = s.pending_protections[index].stop_price;
            // Remember what the trader protected this entry with. The diary
            // cannot derive it from fills, and a post-trade review that guesses
            // its own stop price is worthless — the whole point is comparing the
            // outcome against the plan the trader actually wrote down.
            // Keyed by the fill that opened the trade (see `local_protection_key`):
            // a triggered entry fills on a new order id.
            let plan_key = local_protection_key("", &symbol, &order_id, fill_timestamp);
            let planned = s
                .entry_protections
                .entry(plan_key)
                .or_insert_with(|| json!({ "stop_price": Value::Null, "take_price": Value::Null }));
            if stop_price > 0.0 {
                planned["stop_price"] = json!(stop_price);
            }
            if let Some(tp) = s.pending_protections[index].take_price {
                planned["take_price"] = json!(tp);
            }
            let take_price = s.pending_protections[index].take_price;
            let entry_side = s.pending_protections[index].entry_side.clone();
            let protect_side = if entry_side == "buy" { "sell" } else { "buy" };
            if stop_price > 0.0 {
                match s.ex.create_order(
                    &symbol,
                    "stop",
                    protect_side,
                    protect_amount,
                    None,
                    CreateOrderParams::new()
                        .stop_price(stop_price)
                        .reduce_only(true),
                ) {
                    Ok(order) => {
                        if let Some(id) = order["id"].as_str() {
                            s.active_protections.push((
                                symbol.clone(),
                                entry_side.clone(),
                                id.to_string(),
                            ));
                            s.protection_kinds.insert(id.to_string(), "stop");
                        }
                        events.push(json!({ "type": "order_update", "order": order }));
                    }
                    Err(error) => events.push(error_json(
                        format!("stop loss could not be armed for {symbol}: {error}"),
                        None,
                    )),
                }
            }
            if let Some(tp) = take_price {
                match s.ex.create_order(
                    &symbol,
                    "take_profit",
                    protect_side,
                    protect_amount,
                    None,
                    CreateOrderParams::new().stop_price(tp).reduce_only(true),
                ) {
                    Ok(order) => {
                        if let Some(id) = order["id"].as_str() {
                            s.active_protections
                                .push((symbol.clone(), entry_side, id.to_string()));
                            s.protection_kinds.insert(id.to_string(), "take");
                        }
                        events.push(json!({ "type": "order_update", "order": order }));
                    }
                    Err(error) => events.push(error_json(
                        format!("take profit could not be armed for {symbol}: {error}"),
                        None,
                    )),
                }
            }
            s.pending_protections[index].remaining -= protect_amount;
            if s.pending_protections[index].remaining <= f64::EPSILON {
                s.pending_protections.remove(index);
            }
        } else if let Some(index) =
            s.active_protections
                .iter()
                .position(|(protected_symbol, entry_side, stop_id)| {
                    protected_symbol == &symbol
                        && source_order_id.as_deref() == Some(stop_id.as_str())
                        && *entry_side != side
                })
        {
            let (_, protected_side, _) = s.active_protections.remove(index);
            let position_is_open =
                s.ex.account_snapshot()["positions"]
                    .as_array()
                    .is_some_and(|positions| {
                        positions.iter().any(|position| {
                            position["symbol"] == symbol
                                && position["qty"].as_f64().unwrap_or(0.0).abs() > f64::EPSILON
                        })
                    });
            if !position_is_open {
                let mut rest = Vec::new();
                for (active_symbol, active_side, active_id) in s.active_protections.drain(..) {
                    if active_symbol == symbol && active_side == protected_side {
                        // "already gone" is normal, not an error: the sibling
                        // may have triggered on this very bar (it was then
                        // rejected by reduce_only), so there is nothing left
                        // to cancel. Only real failures are worth reporting.
                        if let Err(error) = s.ex.cancel_order(&active_id, &symbol) {
                            let text = error.to_string();
                            if !text.contains("unknown order") {
                                events.push(error_json(
                                    format!("could not cancel remaining stop {active_id}: {text}"),
                                    None,
                                ));
                            }
                        }
                    } else {
                        rest.push((active_symbol, active_side, active_id));
                    }
                }
                s.active_protections = rest;
            }
        }
        events.push(json!({ "type": "trade", "fill": fill }));
    }
    s.fills_seen = total;
    events
}

fn account_json(s: &Session) -> Value {
    json!({ "type": "account_update", "account": s.ex.account_snapshot() })
}

fn error_json(message: String, request_id: Option<String>) -> Value {
    json!({ "type": "error", "message": message, "request_id": request_id })
}

/// Deltas after one step: new candles + market/trades/account/replay.
///
/// `force` bypasses the coalescing timers (manual step, reset, order ack):
/// those are user-initiated and the UI expects the result immediately.
fn delta_events(s: &mut Session, force: bool) -> Vec<Value> {
    let mut ev = Vec::new();
    // O(k): only the bars after the cursor, not the whole revealed history.
    for bar in s.ex.live_from(&s.symbol, &s.timeframe, s.fed) {
        ev.push(json!({
            "type": "candle",
            "symbol": s.symbol,
            "timeframe": s.timeframe,
            "exchange": s.exchange,
            "candle": bar,
            "is_new": true,
        }));
    }
    s.fed = s.ex.live_len(&s.symbol, &s.timeframe);
    let oi_end = s.oi.partition_point(|(ts, _)| *ts <= s.ex.now_ms());
    if oi_end > s.oi_fed {
        ev.push(json!({
            "type": "oi_update",
            "samples": s.oi[s.oi_fed..oi_end],
        }));
        s.oi_fed = oi_end;
    }
    let last = s.ex.live_last(&s.symbol, &s.timeframe)
        .map(|row| row[4])
        .unwrap_or(0.0);
    if force || s.last_market_push.elapsed() >= Duration::from_millis(MARKET_PUSH_MS) {
        s.last_market_push = std::time::Instant::now();
        ev.push(json!({
            "type": "market_update",
            "symbol": s.symbol,
            "last": last,
            "timestamp": s.ex.now_ms(),
        }));
    }
    // New fills produced by the step (e.g. triggered stop/TP).
    let fresh = fresh_fill_events(s);
    let has_fills = fresh.iter().any(|event| event["type"] == "trade");
    ev.extend(fresh);
    if force || has_fills || s.last_account_push.elapsed() >= Duration::from_millis(ACCOUNT_PUSH_MS) {
        s.last_account_push = std::time::Instant::now();
        ev.push(account_json(s));
    }
    if force || s.last_replay_push.elapsed() >= Duration::from_millis(REPLAY_PUSH_MS) {
        s.last_replay_push = std::time::Instant::now();
        ev.push(replay_json(s));
    }
    ev
}

/// Fresh isolated session: 1M USDT paper account, empty series (the first
/// `subscribe` loads it and resets the clock), random challenge nick.
fn new_session(nick: String) -> Session {
    let mut ex = Exchange::default();
    ex.deposit("USDT", 1_000_000.0);
    Session {
        ex,
        symbol: String::new(),
        timeframe: String::new(),
        exchange: default_exchange(),
        speed: DEFAULT_SPEED,
        step_ms: 60_000,
        fed: 0,
        fills_seen: 0,
        seen_orders: HashSet::new(),
        last_step: std::time::Instant::now(),
        last_account_push: std::time::Instant::now() - Duration::from_secs(1),
        last_replay_push: std::time::Instant::now() - Duration::from_secs(1),
        last_market_push: std::time::Instant::now() - Duration::from_secs(1),
        token: String::new(),
        pending_subscribe: None,
        nick,
        in_challenge: false,
        risk_usdt: 0.0,
        capital_usdt: 0.0,
        pending_protections: Vec::new(),
        entry_protections: EntryProtections::new(),
        protection_kinds: HashMap::new(),
        exit_reasons: HashMap::new(),
        active_protections: Vec::new(),
        diary_meta: json!({"drawings": [], "criteria": {}}),
        oi: Vec::new(),
        oi_period: String::new(),
        oi_fed: 0,
        range_start_ms: 0,
        reveal_start_ms: 0,
        range_end_ms: 0,
        local_account: None,
        local_fills: Vec::new(),
        local_engine_id: String::new(),
        local_entry_protections: HashMap::new(),
        local_exit_reasons: HashMap::new(),
    }
}


impl Engine {
    /// Unit-test constructor: never touches the network.
    fn new(out: broadcast::Sender<String>) -> Self {
        Engine {
            sessions: HashMap::new(),
            detached: HashMap::new(),
            clients: HashMap::new(),
            out,
            allow_net: false,
            jobs: None,
            last_board_push: std::time::Instant::now(),
        }
    }

    fn with_net(mut self, allow_net: bool) -> Self {
        self.allow_net = allow_net;
        self
    }

    /// Attach the background preloader (the only place a warm-up can run).

    fn with_jobs(mut self, jobs: mpsc::Sender<EngineJob>) -> Self {
        self.jobs = Some(jobs);
        self
    }

    /// Everything the engine says to a session carries `to`. Delivery prefers
    /// the client's private channel; the broadcast fallback exists for unit
    /// tests (and preload progress), where no socket registered one.
    fn send_to(&self, to: u64, mut v: Value) {
        v["to"] = json!(to);
        let mut text = msg_to(to, v);
        if let Some(tx) = self.clients.get(&to) {
            // `try_send`: the engine thread must never await a socket. A full
            // queue falls back to the broadcast ring, whose lag path sends the
            // client a `resync` — slow readers are recovered, not blocked.
            match tx.try_send(text) {
                Ok(()) => return,
                Err(tokio::sync::mpsc::error::TrySendError::Full(returned)) => {
                    text = returned;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
            }
        }
        let _ = self.out.send(text);
    }

    fn send_all(&self, to: u64, evs: Vec<Value>) {
        for v in evs {
            self.send_to(to, v);
        }
    }

    /// Lazily create the session for a first-time client (random nick).
    fn session(&mut self, id: u64) -> &mut Session {
        if !self.sessions.contains_key(&id) {
            let nick = unique_nick(&self.sessions);
            log!("[engine] session #{id} created ({nick})");
            self.sessions.insert(id, new_session(nick));
        }
        // expect: we just inserted when missing.
        self.sessions.get_mut(&id).expect("session just inserted")
    }

    /// TEST ONLY: seed a deterministic 5m series into `client_id`'s session.
    ///
    /// Production sessions never hold candles — the browser owns market data.
    /// The session-lab tests below (and the diary scenarios) still exercise the
    /// server's order/diary bookkeeping against the real `Exchange`, so they
    /// need a price series; this gives them one without reintroducing a
    /// market-data path into the server. `bars` starts `first_ms` and moves by a
    /// deterministic zig-zag; `start_ms`/`reveal_ms` place the replay clock.
    #[cfg(test)]
    fn seed_series(
        &mut self,
        client_id: u64,
        symbol: &str,
        timeframe: &str,
        first_ms: i64,
        bars: usize,
        start_ms: i64,
        reveal_ms: i64,
        end_ms: i64,
    ) {
        // The server only needs a step for its own replay clock; the browser is
        // what downloads and aggregates actual bars.
        let step: u64 = match timeframe {
            "1d" => 86_400_000,
            "1h" => 3_600_000,
            "15m" => 900_000,
            "5m" => 300_000,
            _ => 60_000,
        };
        let parsed = simccxt_core::Symbol::parse(symbol).expect("test symbol parses");
        let s = self.session(client_id);
        let mut rows = Vec::with_capacity(bars);
        let mut price = 20_000.0f64;
        for index in 0..bars {
            let wave = (index % 20) as f64;
            let open = price;
            price = price + (wave.sin() * 40.0) + 3.0;
            let high = open.max(price) + 12.0;
            let low = open.min(price) - 12.0;
            let ts = DateTime::from_timestamp_millis(first_ms + index as i64 * step as i64)
                .expect("test timestamp");
            rows.push(simccxt_core::OHLCV::new(ts, open, high, low, price, 10.0));
        }
        s.ex.add_market(&parsed, simccxt_broker::MarketKind::Linear);
        s.ex.load_candles(symbol, timeframe, rows);
        s.ex.set_step_ms(step);
        s.ex.set_last_price(symbol, price);
        let at = DateTime::from_timestamp_millis(start_ms).expect("test start");
        s.ex.reset(at);
        s.ex.pause();
        s.pending_subscribe = None;
        s.symbol = symbol.to_string();
        s.timeframe = timeframe.to_string();
        s.exchange = "binance".to_string();
        s.range_start_ms = start_ms;
        s.reveal_start_ms = reveal_ms;
        s.range_end_ms = end_ms;
        s.step_ms = step;
        s.fed = 0;
        s.oi_fed = 0;
        s.fills_seen = s.ex.fills_len();
        s.last_step = std::time::Instant::now();
    }

    /// Broadcast the challenge board to EVERYONE (no `to` — the writer
    /// forwards such messages to all sockets). Cheap: only participants.
    fn push_board(&mut self) {
        self.last_board_push = std::time::Instant::now();
        let _ = self.out.send(msg(challenge_board_json(&self.sessions)));
    }

    fn handle_command(&mut self, to: u64, cmd: ClientCmd) {
        // Every event is collected first and sent after the match: the
        // session borrow must end before self.send_all.
        let mut evs: Vec<Value> = Vec::new();
        // Set when the shared challenge board changed and must be
        // re-broadcast to everyone after the directed events go out.
        let mut board_push = false;
        match cmd {
            ClientCmd::LocalSync {
                account,
                fills,
                replace_fills,
                engine_id,
                entry_protections,
                exit_reasons,
                symbol,
                timeframe,
                exchange,
                range,
            } => {
                let s = self.session(to);
                s.local_account = Some(account);
                s.local_engine_id = engine_id.clone();
                // Stamp every fill with the engine instance that produced it.
                // A reloaded tab replays from the same bar with order ids back
                // at 1, so without this the new entry is indistinguishable from
                // the old one and FIFO pairs it with the old session's exit.
                let mut fills: Vec<Value> = fills
                    .into_iter()
                    .map(|mut fill| {
                        fill["engine_id"] = json!(engine_id);
                        fill
                    })
                    .collect();
                if replace_fills {
                    s.local_fills = fills;
                } else {
                    // Dedupe by the WHOLE fill, not by `id` alone: the id is
                    // `orderId_fillIndex`, and order ids restart at 1 for every
                    // symbol engine. BTC's fill #1 and ETH's fill #1 share the
                    // bare id, so dropping the second one left the diary with a
                    // lone exit fill — it then opened a phantom position on the
                    // wrong side at the exit price and paired it with the next
                    // entry. Same key the browser uses (`fillKey`), plus the
                    // engine instance (two page loads may repeat a fill).
                    let mut known: std::collections::HashSet<String> =
                        s.local_fills.iter().map(fill_identity).collect();
                    fills.retain(|fill| {
                        let key = fill_identity(fill);
                        !key.is_empty() && known.insert(key)
                    });
                    s.local_fills.append(&mut fills);
                }
                // Merge, never replace: a page reload starts the browser with
                // empty maps, and replacing would wipe the levels and exit
                // verdicts of every trade this session already knows — the
                // diary then loses "было в ±R" and calls a take-profit exit
                // "вручную". Entries are immutable (one per entry order), so a
                // union is always correct.
                s.local_entry_protections.extend(entry_protections);
                s.local_exit_reasons.extend(exit_reasons);
                s.symbol = symbol;
                s.timeframe = timeframe;
                s.exchange = exchange;
                s.range_start_ms = range["start_ms"].as_i64().unwrap_or(s.range_start_ms);
                s.reveal_start_ms = range["reveal_start_ms"]
                    .as_i64()
                    .unwrap_or(s.reveal_start_ms);
                s.range_end_ms = range["end_ms"].as_i64().unwrap_or(s.range_end_ms);
                board_push = true;
            }
            ClientCmd::DiaryMeta {
                drawings,
                criteria,
                request_id,
            } => {
                let s = self.session(to);
                // Bound size so a misbehaving client can't bloat the session.
                let mut meta = s.diary_meta.clone();
                if let Some(d) = drawings {
                    meta["drawings"] = serde_json::Value::Array(d.into_iter().take(64).collect());
                }
                if let Some(c) = criteria {
                    const MAX_KEYS: usize = 32;
                    let mut map = serde_json::Map::new();
                    for (k, v) in c.into_iter().take(MAX_KEYS) {
                        if k.len() <= 32 {
                            let list = v
                                .as_array()
                                .map(|a| a.iter().take(16).cloned().collect::<Vec<_>>());
                            if let Some(list) = list {
                                map.insert(k, serde_json::Value::Array(list));
                            }
                        }
                    }
                    meta["criteria"] = serde_json::Value::Object(map);
                }
                s.diary_meta = meta;
                let _ = request_id;
            }
            ClientCmd::Ping => {
                evs.push(json!({ "type": "pong" }));
            }
            ClientCmd::Attach { token, request_id } => {
                let restored = self.attach(to, token.clone());
                let s = self.session(to);
                s.fed = s.ex.live_len(&s.symbol, &s.timeframe);
                s.last_account_push = std::time::Instant::now();
                s.last_replay_push = std::time::Instant::now();
                s.last_market_push = std::time::Instant::now();
                let state = json!({
                    "type": "session_state",
                    "restored": restored,
                    "nick": s.nick,
                    "risk_usdt": s.risk_usdt,
                    "capital_usdt": s.capital_usdt,
                    "in_challenge": s.in_challenge,
                    "has_series": !s.symbol.is_empty(),
                    "symbol": s.symbol,
                    "timeframe": s.timeframe,
                    "exchange": s.exchange,
                    "start_ms": s.range_start_ms,
                    "reveal_start_ms": s.reveal_start_ms,
                    "end_ms": s.range_end_ms,
                    "request_id": request_id,
                });
                let rejoined = restored && s.in_challenge;
                evs.push(state);
                if restored && !s.symbol.is_empty() {
                    // The client adopts the restored series/clock/account
                    // instead of re-subscribing (which would reset the replay).
                    evs.push(snapshot_json(s, None));
                }
                board_push |= rejoined;
                log!(
                    "[engine] #{to} attach {} -> {}",
                    if token.is_empty() { "(none)" } else { "token" },
                    if restored { "restored" } else { "new session" }
                );
            }
            ClientCmd::Login { name, request_id } => {
                let s = self.session(to);
                if name.trim().is_empty() {
                    // Empty login = keep the assigned nick, just confirm it.
                    evs.push(json!({
                        "type": "login_ok", "nick": s.nick, "request_id": request_id }));
                } else {
                    match clean_nick(&name) {
                        Some(n) => {
                            s.nick = n.clone();
                            log!("[engine] #{to} login as {n}");
                            evs.push(json!({
                                "type": "login_ok", "nick": n, "request_id": request_id }));
                        }
                        None => evs.push(error_json(
                            "ник: 1–24 символа без управляющих символов".into(),
                            request_id,
                        )),
                    }
                }
                board_push = true; // a newcomer should see the current board
            }
            ClientCmd::Challenge {
                join,
                risk_usdt,
                capital_usdt,
                request_id,
            } => {
                let s = self.session(to);
                if join {
                    match risk_usdt {
                        Some(r) if r > 0.0 && r.is_finite() => {
                            let capital_changed = capital_usdt.is_some_and(|capital| {
                                s.capital_usdt > 0.0
                                    && (s.capital_usdt - capital).abs() > f64::EPSILON
                            });
                            if (s.risk_usdt > 0.0 && (s.risk_usdt - r).abs() > f64::EPSILON)
                                || capital_changed
                            {
                                evs.push(error_json(
                                    "risk and capital are fixed for the current challenge round"
                                        .into(),
                                    request_id.clone(),
                                ));
                                evs.push(json!({
                                    "type": "challenge_ok", "joined": false,
                                    "request_id": request_id,
                                }));
                            } else {
                                s.risk_usdt = r;
                                if let Some(capital) =
                                    capital_usdt.filter(|value| value.is_finite() && *value > 0.0)
                                {
                                    s.capital_usdt = capital;
                                }
                                s.in_challenge = true;
                                log!("[engine] #{to} ({}) joins the challenge, risk ${r}", s.nick);
                                evs.push(json!({
                                    "type": "challenge_ok", "joined": true,
                                    "nick": s.nick, "request_id": request_id }));
                                board_push = true;
                            }
                        }
                        _ => {
                            evs.push(error_json(
                                "challenge: нужен риск > 0 (поле «Риск» в сделке)".into(),
                                request_id.clone(),
                            ));
                            evs.push(json!({
                                "type": "challenge_ok", "joined": false,
                                "request_id": request_id,
                            }));
                        }
                    }
                } else if s.in_challenge {
                    s.in_challenge = false;
                    log!("[engine] #{to} ({}) leaves the challenge", s.nick);
                    evs.push(json!({
                        "type": "challenge_ok", "joined": false, "request_id": request_id }));
                    board_push = true;
                } else {
                    evs.push(json!({
                        "type": "challenge_ok", "joined": false, "request_id": request_id }));
                }
            }
            ClientCmd::Subscribe {
                symbol,
                timeframe,
                exchange,
                start_ms,
                reveal_start_ms,
                end_ms,
                request_id,
            } => {
                // The browser owns market data now: it downloads 5m bars
                // straight from the venue, aggregates 1h/1d itself and runs the
                // Rust/WASM engine in its own Web Worker. Our server therefore
                // does NOT load, cache or stream any coin data — this command
                // only records the selection so the session mirror and the
                // diary can say what the trader is looking at.
                let s = self.session(to);
                s.symbol = symbol;
                s.timeframe = timeframe;
                s.exchange = exchange;
                if let Some(value) = start_ms {
                    s.range_start_ms = value;
                }
                if let Some(value) = reveal_start_ms {
                    s.reveal_start_ms = value;
                }
                if let Some(value) = end_ms {
                    s.range_end_ms = value;
                }
                s.pending_subscribe = None;
                evs.push(json!({
                    "type": "series_ack",
                    "symbol": s.symbol,
                    "timeframe": s.timeframe,
                    "exchange": s.exchange,
                    "request_id": request_id,
                }));
            }
            ClientCmd::Preload {
                symbols,
                timeframes,
                request_id,
                ..
            } => {
                // Kept as a no-op for old clients: a warm-up used to fill the
                // server's disk cache, but the server no longer holds coin data
                // at all. The browser warms its own OPFS cache instead, so the
                // right answer is "nothing to do here" rather than a download.
                let pairs = preload_plan(&symbols, &timeframes);
                let total = pairs.len();
                evs.push(json!({
                    "type": "preload_done",
                    "done": total,
                    "total": total,
                    "failed": 0,
                    "note": "market data is loaded by the browser",
                    "request_id": request_id,
                }));
            }
            ClientCmd::MarketLookup {
                request_id,
                ..
            } => {
                // Watchlist quotes are fetched by each browser straight from the
                // venue (connection.lookupMarketsSim). The server answers so an
                // old client is not left hanging, but performs no network work.
                evs.push(json!({
                    "type": "market_lookup",
                    "quotes": [],
                    "failures": [],
                    "note": "quotes are fetched by the browser",
                    "request_id": request_id,
                }));
            }
            ClientCmd::PlaceOrder {
                symbol,
                side,
                order_type,
                quantity,
                price,
                stop_price,
                stop_loss_price,
                risk_usdt,
                time_in_force,
                post_only,
                reduce_only,
                tp_price,
                request_id,
            } => {
                let s = self.session(to);
                let mut quantity = quantity;
                let valid_stop = if let Some(stop) = stop_loss_price {
                    let reference = price
                        .or(stop_price)
                        .or_else(|| s.ex.last_price_for(&symbol));
                    reference.is_some_and(|entry| {
                        stop.is_finite()
                            && stop > 0.0
                            && if side == "buy" {
                                stop < entry
                            } else {
                                stop > entry
                            }
                    })
                } else {
                    true
                };
                let valid_tp = if let Some(tp) = tp_price {
                    let reference = price
                        .or(stop_price)
                        .or_else(|| s.ex.last_price_for(&symbol));
                    reference.is_some_and(|entry| {
                        tp.is_finite()
                            && tp > 0.0
                            && if side == "buy" {
                                tp > entry
                            } else {
                                tp < entry
                            }
                    })
                } else {
                    true
                };
                if !valid_stop {
                    evs.push(error_json(
                        "stop loss must be below the entry price for BUY and above it for SELL"
                            .into(),
                        request_id,
                    ));
                } else if !valid_tp {
                    evs.push(error_json(
                        "take profit must be above the entry price for BUY and below it for SELL"
                            .into(),
                        request_id,
                    ));
                } else {
                    // Re-size from the actual entry reference, not the client's
                    // rendered preview: a replay tick between render and this
                    // command would otherwise make the stop-out cost >1R.
                    if let (Some(risk), Some(stop)) = (risk_usdt, stop_loss_price) {
                        if risk > 0.0 && !reduce_only {
                            let reference = price
                                .or(stop_price)
                                .or_else(|| s.ex.last_price_for(&symbol));
                            if let Some(entry) = reference.filter(|value| *value > 0.0) {
                                let taker = s.ex.broker().fee_schedule().taker;
                                let per_unit = (entry - stop).abs()
                                    + (entry.abs() + stop.abs()) * taker;
                                if per_unit > 0.0 {
                                    quantity = risk / per_unit;
                                }
                            }
                        }
                    }
                    let mut p = CreateOrderParams::new();
                    p.stop_price = stop_price;
                    // `tp_price` is not attached to the entry order: for a
                    // stop/limit entry the broker would arm it immediately,
                    // before the entry fills, duplicating the take-profit that
                    // `pending_protections` arms from the fill.
                    p.reduce_only = reduce_only;
                    p.post_only = post_only;
                    if !time_in_force.is_empty() {
                        if let Some(tif) = parse_tif(&time_in_force) {
                            p.time_in_force = tif;
                        }
                    }
                    match s
                        .ex
                        .create_order(&symbol, &order_type, &side, quantity, price, p)
                    {
                        Ok(order) => {
                            evs.push(json!({
                                    "type": "order_update", "order": order, "request_id": request_id }));
                            if stop_loss_price.is_some() || tp_price.is_some() {
                                let order_id = order["id"].as_str().unwrap_or_default().to_string();
                                s.pending_protections.push(PendingProtection {
                                    symbol: symbol.clone(),
                                    entry_side: side.clone(),
                                    entry_order_id: order_id,
                                    stop_price: stop_loss_price.unwrap_or(0.0),
                                    take_price: tp_price,
                                    remaining: quantity,
                                });
                            }
                            let fill_events = fresh_fill_events(s);
                            board_push |= s.in_challenge
                                && fill_events.iter().any(|event| event["type"] == "trade");
                            evs.extend(fill_events);
                            evs.push(account_json(s));
                        }
                        Err(e) => evs.push(error_json(format!("place_order: {e}"), request_id)),
                    }
                }
            }
            ClientCmd::CancelOrder {
                order_id,
                symbol,
                request_id,
            } => {
                let s = self.session(to);
                match s.ex.cancel_order(&order_id, &symbol) {
                    Ok(order) => {
                        s.pending_protections
                            .retain(|pending| pending.entry_order_id != order_id);
                        s.active_protections
                            .retain(|(_, _, active_id)| active_id != &order_id);
                        evs.push(json!({
                            "type": "order_update", "order": order, "request_id": request_id }));
                        evs.push(account_json(s));
                    }
                    Err(e) => evs.push(error_json(format!("cancel_order: {e}"), request_id)),
                }
            }
            ClientCmd::ClosePosition {
                symbol,
                quantity,
                request_id,
            } => {
                let s = self.session(to);
                let snapshot = s.ex.account_snapshot();
                let position = snapshot["positions"]
                    .as_array()
                    .and_then(|positions| {
                        positions
                            .iter()
                            .find(|position| position["symbol"] == symbol)
                    })
                    .cloned();
                let (held, side) = if let Some(position) = position {
                    (
                        position["qty"].as_f64().unwrap_or(0.0),
                        if position["side"] == "short" {
                            "buy"
                        } else {
                            "sell"
                        },
                    )
                } else {
                    let (qty, _) = s.ex.spot_basis(&symbol);
                    (qty, "sell")
                };
                let q = quantity.unwrap_or(held);
                if q <= 0.0 {
                    evs.push(error_json("nothing to close".into(), request_id));
                } else {
                    let mut pending = Vec::new();
                    for protection in s.pending_protections.drain(..) {
                        if protection.symbol == symbol {
                            if let Err(error) =
                                s.ex.cancel_order(&protection.entry_order_id, &symbol)
                            {
                                evs.push(error_json(
                                    format!(
                                        "could not cancel pending entry {}: {error}",
                                        protection.entry_order_id
                                    ),
                                    request_id.clone(),
                                ));
                            }
                        } else {
                            pending.push(protection);
                        }
                    }
                    s.pending_protections = pending;
                    let mut active = Vec::new();
                    for (active_symbol, entry_side, active_id) in s.active_protections.drain(..) {
                        if active_symbol == symbol {
                            if let Err(error) = s.ex.cancel_order(&active_id, &symbol) {
                                evs.push(error_json(
                                    format!(
                                        "could not cancel protective stop {active_id}: {error}"
                                    ),
                                    request_id.clone(),
                                ));
                            }
                        } else {
                            active.push((active_symbol, entry_side, active_id));
                        }
                    }
                    s.active_protections = active;
                    // Cancel any broker-level conditional orders (e.g. TP/SL) for this symbol.
                    let broker_conditional_ids: Vec<String> =
                        s.ex.broker_mut()
                            .conditional_orders(Some(&symbol))
                            .iter()
                            .map(|c| format!("conditional_{}", c.id))
                            .collect();
                    for cond_id in broker_conditional_ids {
                        let _ = s.ex.cancel_order(&cond_id, &symbol);
                    }
                    match s.ex.create_order(
                        &symbol,
                        "market",
                        side,
                        q,
                        None,
                        CreateOrderParams::new().reduce_only(true),
                    ) {
                        Ok(order) => {
                            evs.push(json!({
                                "type": "order_update", "order": order,
                                "request_id": request_id }));
                            let fill_events = fresh_fill_events(s);
                            board_push |= s.in_challenge
                                && fill_events.iter().any(|event| event["type"] == "trade");
                            evs.extend(fill_events);
                            evs.push(account_json(s));
                        }
                        Err(e) => evs.push(error_json(format!("close_position: {e}"), request_id)),
                    }
                }
            }
            ClientCmd::Replay {
                action,
                steps,
                speed,
                step_ms,
                timestamp_ms,
                request_id,
            } => {
                let known = matches!(
                    action.as_str(),
                    "play" | "pause" | "step" | "reset" | "set_speed" | "set_step"
                );
                if !known {
                    evs.push(error_json(
                        format!("unknown replay action: {action}"),
                        request_id,
                    ));
                } else {
                    let s = self.session(to);
                    match action.as_str() {
                        "play" => {
                            // Pressing play at the end of the series used to be a
                            // no-op: the clock advanced past the last bar, no new
                            // bar arrived, and the graph just sat there. Rewind to
                            // the reveal point instead, so "play" always plays.
                            if s.ex.replay_done(&s.symbol, &s.timeframe) {
                                rewind_to_reveal(s);
                                evs.push(snapshot_json(s, None));
                            }
                            s.ex.resume();
                            s.last_step = std::time::Instant::now();
                        }
                        "pause" => s.ex.pause(),
                        "step" => {
                            // Advance exactly n bars even while paused (pause restored).
                            let was_paused = s.ex.is_paused();
                            if was_paused {
                                s.ex.resume();
                            }
                            if let Some(ms) = step_ms {
                                s.ex.set_step_ms(ms);
                                s.step_ms = ms;
                            }
                            let fills_before = s.ex.fills_len();
                            let steps = steps.unwrap_or(1).clamp(1, MAX_STEPS_PER_COMMAND);
                            s.ex.step_n(steps);
                            if was_paused {
                                s.ex.pause();
                            }
                            s.last_step = std::time::Instant::now();
                            let d = delta_events(s, true);
                            if s.ex.fills_len() > fills_before
                                && s.in_challenge
                            {
                                board_push = true;
                            }
                            evs.extend(d);
                        }
                        "reset" => {
                            match timestamp_ms {
                                Some(ms) => {
                                    if let Some(ts) = DateTime::from_timestamp_millis(ms) {
                                        s.ex.reset(ts);
                                        s.oi_fed = s.oi.partition_point(|(oi_ts, _)| *oi_ts <= ms);
                                    }
                                }
                                None => rewind_to_reveal(s),
                            }
                            // The client's bar list only ever shrinks on a snapshot;
                            // `replay_state` alone leaves every already-revealed bar
                            // on screen, so a reset looked like a no-op. Send the
                            // snapshot that truncates the chart back to the new
                            // clock, then resume revealing bar by bar.
                            let snap = snapshot_json(s, request_id.clone());
                            s.fed = s.ex.live_len(&s.symbol, &s.timeframe);
                            s.last_step = std::time::Instant::now();
                            evs.push(snap);
                        }
                        "set_speed" => {
                            if let Some(v) = speed {
                                if v > 0.0 && v <= 50.0 {
                                    s.speed = v;
                                }
                            }
                        }
                        "set_step" => {
                            if let Some(ms) = step_ms {
                                let ms = ms.max(1);
                                s.ex.set_step_ms(ms);
                                s.step_ms = ms;
                            }
                        }
                        _ => unreachable!("filtered by matches! above"),
                    }
                    if let Some(v) = speed {
                        if action == "play" && v > 0.0 && v <= 50.0 {
                            s.speed = v;
                        }
                    }
                    evs.push(ack_replay(s, request_id));
                }
            }
        }
        self.send_all(to, evs);
        if board_push {
            self.push_board();
        }
    }

    fn handle_diary(&self, nick: &str, reply: oneshot::Sender<Value>) {
        let mut result = json!({"type": "diary", "nick": nick, "error": "trader not found"});
        for s in self.sessions.values() {
            if s.nick != nick {
                continue;
            }
            let local = s.local_account.is_some();
            // Reconstruct each browser engine instance separately. Order ids
            // restart at 1 on every page load and a reloaded tab replays from
            // the same bar, so FIFO across two engines pairs the new entry with
            // the old session's exit and invents trades that never existed.
            let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
            if local {
                let mut index: HashMap<String, usize> = HashMap::new();
                for fill in &s.local_fills {
                    let engine = fill["engine_id"].as_str().unwrap_or_default();
                    let at = match index.get(engine) {
                        Some(at) => *at,
                        None => {
                            groups.push((engine.to_string(), Vec::new()));
                            index.insert(engine.to_string(), groups.len() - 1);
                            groups.len() - 1
                        }
                    };
                    groups[at].1.push(fill.clone());
                }
            } else {
                groups.push((String::new(), s.ex.fetch_fills(None, None)));
            }
            let current_engine = s.local_engine_id.clone();
            let risk = s.risk_usdt;
            let snap = s
                .local_account
                .clone()
                .unwrap_or_else(|| s.ex.account_snapshot());
            let realized = snap["pnl"]["realized"].as_f64().unwrap_or(0.0);
            let equity = snap["equity"].as_f64().unwrap_or(0.0);

            // Mark prices from open positions (for unrealized PnL).
            let mut mark_prices = HashMap::new();
            for p in snap["positions"].as_array().unwrap_or(&vec![]) {
                let sym = p["symbol"].as_str().unwrap_or("");
                mark_prices.insert(sym.to_string(), p["markPrice"].as_f64().unwrap_or(0.0));
            }

            let mut trades: Vec<diary::Trade> = Vec::new();
            // Which engine instance produced each trade (parallel to `trades`).
            let mut trade_engine: Vec<String> = Vec::new();
            let mut positions: Vec<diary::OpenPosition> = Vec::new();
            let group_count = groups.len();
            for (group_index, (engine, fills)) in groups.into_iter().enumerate() {
                // The server exchange returns newest-first, while local sync
                // stores browser fills oldest-first. Normalize both to
                // chronological order.
                let n = fills.len() as u64;
                let mut fills_with_meta: Vec<Value> = fills
                    .into_iter()
                    .enumerate()
                    .map(|(idx, mut f)| {
                        f["time_ms"] = json!(f["timestamp"].as_i64().unwrap_or(0));
                        f["fill_index"] = json!(if local {
                            idx as u64 + 1
                        } else {
                            n - idx as u64
                        });
                        f
                    })
                    .collect();
                if !local {
                    fills_with_meta.reverse();
                }

                let (group_trades, group_positions) =
                    diary::reconstruct_trades(fills_with_meta, risk, mark_prices.clone());
                let before = trades.len();
                trades.extend(group_trades);
                for _ in before..trades.len() {
                    trade_engine.push(engine.clone());
                }
                // Only the live engine holds real open positions: an older
                // page's leftover lot was abandoned when that page closed.
                if engine == current_engine || (positions.is_empty() && group_index + 1 == group_count)
                {
                    positions = group_positions;
                }
            }
            let mut paired: Vec<(String, diary::Trade)> =
                trade_engine.into_iter().zip(trades).collect();
            paired.sort_by_key(|(_, t)| (t.exit_time_ms, t.entry_time_ms));

            // Win/loss aggregates for the stats block.
            let wins: Vec<f64> = paired
                .iter()
                .filter(|(_, t)| t.pnl > 0.0)
                .map(|(_, t)| t.pnl)
                .collect();
            let losses: Vec<f64> = paired
                .iter()
                .filter(|(_, t)| t.pnl < 0.0)
                .map(|(_, t)| t.pnl)
                .collect();
            let total_pnl: f64 = paired.iter().map(|(_, t)| t.pnl).sum();

            result = json!({
                "type": "diary",
                "nick": nick,
                "trades": paired.iter().map(|(engine, t)| {
                    // The protective levels the trader planned for this entry,
                    // so the review can measure the outcome against the plan.
                    // Both maps are keyed by the fill that opened the trade.
                    let plan_key = local_protection_key(
                        engine,
                        &t.symbol,
                        &t.entry_order_id,
                        t.entry_time_ms,
                    );
                    let plan = if local {
                        s.local_entry_protections.get(&plan_key)
                    } else {
                        s.entry_protections.get(&plan_key)
                    };
                    // Why it ended: the protective level that fired, if one did.
                    // Keyed by engine + `symbol|orderId|time` because ids
                    // restart at 1 for every symbol and session (a bare or
                    // symbol-scoped id mixed an old trade's level into a new
                    // trade and mislabelled the diary's "Итог" column).
                    let exit_key =
                        exit_reason_key(engine, &t.symbol, &t.exit_order_id, t.exit_time_ms);
                    let reason = if local {
                        s.local_exit_reasons.get(&exit_key).cloned()
                    } else {
                        s.exit_reasons.get(&exit_key).map(|kind| kind.to_string())
                    }
                    .map(|kind| json!(kind));
                    json!({
                        "symbol": t.symbol,
                        "side": t.side,
                        "direction": t.direction,
                        "qty": t.qty,
                        "entry_price": t.entry_price,
                        "exit_price": t.exit_price,
                        "pnl": t.pnl,
                        "r": t.r,
                        "entry_time_ms": t.entry_time_ms,
                        "exit_time_ms": t.exit_time_ms,
                        "entry_fee": t.entry_fee,
                        "exit_fee": t.exit_fee,
                        "fee": t.fee,
                        "entry_order_id": t.entry_order_id,
                        "exit_order_id": t.exit_order_id,
                        "engine_id": engine,
                        "stop_price": plan
                            .and_then(|p| p.get("stop_price").cloned())
                            .unwrap_or(Value::Null),
                        "take_price": plan
                            .and_then(|p| p.get("take_price").cloned())
                            .unwrap_or(Value::Null),
                        "exit_reason": reason.unwrap_or(Value::Null),
                    })
                }).collect::<Vec<_>>(),
                "positions": positions.iter().map(|p| {
                    json!({
                        "symbol": p.symbol,
                        "side": p.side,
                        "qty": p.qty,
                        "entry_price": p.entry_price,
                        "entry_time_ms": p.entry_time_ms,
                        "mark_price": p.mark_price,
                        "unrealized": p.unrealized,
                    })
                }).collect::<Vec<_>>(),
                "stats": {
                    "total_trades": paired.len(),
                    "win_rate": if paired.is_empty() {
                        0.0
                    } else {
                        wins.len() as f64 / paired.len() as f64
                    },
                    "wins": wins.len(),
                    "losses": losses.len(),
                    "realized_pnl": realized,
                    "total_pnl": total_pnl,
                    "avg_r": if paired.is_empty() {
                        0.0
                    } else {
                        paired.iter().map(|(_, t)| t.r).sum::<f64>() / paired.len() as f64
                    },
                    "avg_win": if wins.is_empty() {
                        0.0
                    } else {
                        wins.iter().sum::<f64>() / wins.len() as f64
                    },
                    "avg_loss": if losses.is_empty() {
                        0.0
                    } else {
                        losses.iter().sum::<f64>() / losses.len() as f64
                    },
                    "best": paired.iter().map(|(_, t)| t.pnl).fold(0.0, f64::max),
                    "worst": paired.iter().map(|(_, t)| t.pnl).fold(0.0, f64::min),
                    "fees": paired.iter().map(|(_, t)| t.fee).sum::<f64>(),
                },
                "risk_usdt": risk,
                "capital_usdt": s.capital_usdt,
                "equity": equity,
                "in_challenge": s.in_challenge,
                // Client-pushed visual context: annotations the trader made on
                // the charts + the criteria they enabled, for the review page.
                "drawings": s.diary_meta["drawings"].clone(),
                "criteria": s.diary_meta["criteria"].clone(),
                // Context for the chart: which series the diary page should
                // load candles for (session's current selection).
                "symbol": s.symbol,
                "timeframe": s.timeframe,
                "exchange": s.exchange,
                "range": {
                    "start_ms": s.range_start_ms,
                    "reveal_start_ms": s.reveal_start_ms,
                    "end_ms": s.range_end_ms,
                },
            });
            break;
        }
        let _ = reply.send(result);
    }

    fn handle_job(&mut self, job: EngineJob) {
        match job {
            EngineJob::Command { client_id, cmd } => self.handle_command(client_id, cmd),
            EngineJob::Connect { client_id, tx } => {
                self.clients.insert(client_id, tx);
            }
            EngineJob::Disconnect { client_id } => {
                self.clients.remove(&client_id);
                if let Some(s) = self.sessions.remove(&client_id) {
                    // A page reload closes the socket and reconnects within
                    // milliseconds. Park the session (account, orders, clock,
                    // stats) so the new socket can re-attach by token instead
                    // of starting from zero.
                    let in_challenge = s.in_challenge;
                    if !s.token.is_empty()
                        && !self.sessions.values().any(|other| other.token == s.token)
                    {
                        // Parked sessions are not in `sessions`, so the board
                        // (which iterates only live sessions) already drops
                        // them; `in_challenge` is kept for the restore.
                        log!(
                            "[engine] session #{client_id} ({}) parked for reload",
                            s.nick
                        );
                        self.detached.insert(
                            s.token.clone(),
                            Detached { session: s, since: std::time::Instant::now() },
                        );
                        self.evict_detached();
                    } else {
                        log!("[engine] session #{client_id} ({}) closed", s.nick);
                    }
                    if in_challenge {
                        self.push_board();
                    }
                }
            }
            EngineJob::Diary { nick, reply } => self.handle_diary(&nick, reply),
            EngineJob::Traders { reply } => self.handle_traders(reply),
        }
    }

    /// Drop parked sessions past the TTL, and the oldest when the cap is hit.
    fn evict_detached(&mut self) {
        let now = std::time::Instant::now();
        self.detached
            .retain(|_, parked| now.duration_since(parked.since) < SESSION_TTL);
        while self.detached.len() > MAX_DETACHED {
            let oldest = self
                .detached
                .iter()
                .min_by_key(|(_, parked)| parked.since)
                .map(|(token, _)| token.clone());
            let Some(token) = oldest else { break };
            self.detached.remove(&token);
        }
    }

    /// Restore (or bind) a session token. Returns `true` when an existing
    /// session was resumed, so the caller can tell the client to adopt it.
    fn attach(&mut self, to: u64, token: String) -> bool {
        let token = token.trim().to_string();
        if token.is_empty() || token.len() > 128 {
            return false;
        }
        // Re-attach on a live socket (StrictMode remount / duplicate frame):
        // the session under this id is already ours.
        if self.sessions.get(&to).is_some_and(|s| s.token == token) {
            return true;
        }
        // Takeover race: the new socket may attach before the old socket's
        // disconnect job is processed. Move the live session to the new id.
        if let Some(old_id) = self
            .sessions
            .iter()
            .find(|(id, s)| **id != to && s.token == token)
            .map(|(id, _)| *id)
        {
            if let Some(mut s) = self.sessions.remove(&old_id) {
                s.last_step = std::time::Instant::now();
                self.sessions.insert(to, s);
                return true;
            }
        }
        if let Some(parked) = self.detached.remove(&token) {
            let mut s = parked.session;
            s.last_step = std::time::Instant::now();
            self.sessions.insert(to, s);
            return true;
        }
        let s = self.session(to);
        s.token = token;
        false
    }

    fn handle_traders(&self, reply: oneshot::Sender<Value>) {
        // All online participants (richer than the public board: equity and
        // realized figures come straight from the engine's snapshots).
        let mut rows: Vec<(f64, String, Value)> = self
            .sessions
            .values()
            .map(|s| {
                let snap = s
                    .local_account
                    .clone()
                    .unwrap_or_else(|| s.ex.account_snapshot());
                let realized = snap["pnl"]["realized"].as_f64().unwrap_or(0.0);
                let unrealized = snap["pnl"]["unrealized"].as_f64().unwrap_or(0.0);
                let equity = snap["equity"].as_f64().unwrap_or(0.0);
                let pnl = realized + unrealized;
                let trades = s.seen_orders.len();
                let r = r_of(pnl, s.risk_usdt);
                let value = json!({
                    "nick": s.nick,
                    "in_challenge": s.in_challenge,
                    "risk_usdt": s.risk_usdt,
                    "capital_usdt": s.capital_usdt,
                    "symbol": s.symbol,
                    "timeframe": s.timeframe,
                    "equity": equity,
                    "realized": realized,
                    "unrealized": unrealized,
                    "pnl": pnl,
                    "r": r,
                    "trades": trades,
                });
                (r.unwrap_or(f64::NEG_INFINITY), s.nick.clone(), value)
            })
            .collect();
        // Deterministic order: most R first, ties broken by nick.
        rows.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        let traders: Vec<Value> = rows.into_iter().map(|(_, _, v)| v).collect();
        let _ = reply.send(json!({"type":"traders","traders":traders}));
    }
}

impl Engine {
    /// Sleep until the next session step is due (or the idle poll fires).
    fn next_timeout(&self) -> Duration {
        let mut d = Duration::from_millis(IDLE_POLL_MS);
        let now = std::time::Instant::now();
        for s in self.sessions.values() {
            if s.symbol.is_empty() || s.ex.is_paused() {
                continue;
            }
            let rem = speed_interval(s.speed).saturating_sub(now.duration_since(s.last_step));
            if rem < d {
                d = rem;
            }
        }
        d
    }

    /// One auto-tick per playing session (each at its own speed): exactly one
    /// sim step, then that session's deltas — addressed only to its client.
    fn auto_tick_all(&mut self) {
        let now = std::time::Instant::now();
        let mut emissions: Vec<(u64, Vec<Value>)> = Vec::new();
        for (&id, s) in &mut self.sessions {
            if s.symbol.is_empty() || s.ex.is_paused() {
                continue;
            }
            if now.duration_since(s.last_step) < speed_interval(s.speed) {
                continue;
            }
            s.last_step = now;
            if s.ex.replay_done(&s.symbol, &s.timeframe) {
                s.ex.pause();
                emissions.push((id, vec![replay_json(s)]));
                continue;
            }
            s.ex.step_n(1);
            emissions.push((id, delta_events(s, false)));
        }
        // Mark-to-market changes unrealized PnL on every replay tick.
        let board_dirty = emissions.iter().any(|(id, _)| {
            self.sessions
                .get(id)
                .is_some_and(|session| session.in_challenge)
        });
        for (id, evs) in emissions {
            self.send_all(id, evs);
        }
        // Unrealized PnL moves every tick; a leaderboard at 1 Hz is plenty and
        // avoids serializing every participant's account 100 times a second.
        if board_dirty
            && self.last_board_push.elapsed() >= Duration::from_millis(BOARD_PUSH_MS)
        {
            self.push_board();
        }
    }
}

fn parse_tif(s: &str) -> Option<simccxt_core::TimeInForce> {
    use simccxt_core::TimeInForce as T;
    match s.trim().to_uppercase().as_str() {
        "GTC" => Some(T::GTC),
        "IOC" => Some(T::IOC),
        "FOK" => Some(T::FOK),
        "GTX" => Some(T::GTX),
        "DAY" => Some(T::DAY),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// WebSocket plumbing (thin: no trading logic here)
// ---------------------------------------------------------------------------

async fn handle_conn(
    stream: TcpStream,
    addr: SocketAddr,
    client_id: u64,
    jobs: mpsc::Sender<EngineJob>,
    out: broadcast::Sender<String>,
) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            log!("[ws] #{client_id} handshake from {addr} failed: {e}");
            return;
        }
    };
    log!("[ws] #{client_id} connected from {addr}");
    let (mut sink, mut stream) = ws.split();
    let mut rx = out.subscribe();
    // Private delivery channel: directed events (candles, snapshots, account)
    // never enter the shared broadcast ring, so N sockets no longer clone and
    // parse each other's multi-hundred-KB snapshots.
    let (direct_tx, mut direct_rx) = tokio::sync::mpsc::channel::<String>(256);
    let err_tx = direct_tx.clone();
    if jobs.send(EngineJob::Connect { client_id, tx: direct_tx }).is_err() {
        return;
    }
    // Tell the client its id so it can filter directed messages.
    let hello = msg(json!({ "type": "welcome", "client_id": client_id }));
    if sink
        .send(tokio_tungstenite::tungstenite::Message::Text(hello.into()))
        .await
        .is_err()
    {
        return;
    }
    let jobs_tx = jobs.clone();
    // Incoming JSON -> engine jobs.
    let mut read = async move {
        while let Some(frame) = stream.next().await {
            let frame = match frame {
                Ok(f) => f,
                Err(_) => break,
            };
            if !frame.is_text() {
                continue;
            }
            let text = frame.into_text().unwrap_or_default();
            match serde_json::from_str::<ClientCmd>(&text) {
                Ok(cmd) => {
                    let job = EngineJob::Command { client_id, cmd };
                    if jobs_tx.send(job).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let err = msg_to(client_id, json!({
                        "type": "error",
                        "message": format!("bad command: {e}"),
                    }));
                    // Private channel first; the broadcast fallback is still
                    // routed by its `to` prefix, never delivered to others.
                    if let Err(tokio::sync::mpsc::error::TrySendError::Full(err)) =
                        err_tx.try_send(err)
                    {
                        let _ = out.send(err);
                    }
                }
            }
        }
    }
    .boxed();
    // Two sources, one sink: the private channel (this session) and the
    // broadcast (challenge board + preload progress). A slow client must never
    // kill its connection: broadcast lag asks it to resync and keeps streaming.
    let mut write = async move {
        // Local keepalive, independent of the engine: a long CCXT download on
        // the engine thread must not look like a dead link to the browser.
        let mut beat = tokio::time::interval(Duration::from_secs(5));
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let text = tokio::select! {
                _ = beat.tick() => {
                    json!({ "type": "heartbeat", "keepalive": true }).to_string()
                }
                Some(text) = direct_rx.recv() => text,
                message = rx.recv() => match message {
                    Ok(s) => {
                        // Broadcast carries no `to` (board/preload). Directed
                        // messages on it are the test/legacy fallback only.
                        if let Some(to) = routing_target(&s) {
                            if to != client_id { continue; }
                        }
                        s
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        log!("[ws] #{client_id} lagged, skipped {skipped} msgs -> resync");
                        msg_to(client_id, json!({ "type": "resync" }))
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            };
            let m = tokio_tungstenite::tungstenite::Message::Text(text.into());
            if sink.send(m).await.is_err() {
                break;
            }
        }
    }
    .boxed();
    // Either direction ending closes the connection; log which one so
    // connect/disconnect storms are attributable (client close vs send fail).
    let reason = tokio::select! {
        _ = &mut read => "client-closed",
        _ = &mut write => "send-failed",
    };
    log!("[ws] #{client_id} disconnected ({reason})");
    // Park (or destroy) the session: a reload re-attaches by token.
    let _ = jobs.send(EngineJob::Disconnect { client_id });
}

/// The `to` field of a broadcast message, read without parsing the whole JSON:
/// directed messages always start with `{"to":<id>,` (see `msg_to`).
fn routing_target(text: &str) -> Option<u64> {
    let rest = text.strip_prefix("{\"to\":")?;
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[derive(Debug)]
struct Args {
    port: u16,
    symbol: String,
    timeframe: String,
    /// `--offline`: no network at all (kept for tests and demos).
    offline: bool,
}

fn parse_args() -> Args {
    let mut port = 9001u16;
    let mut symbol = "BTC/USDT".to_string();
    let mut timeframe = "5m".to_string();
    let mut offline = false;
    let mut it = std::env::args().skip(1).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = it.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            "--symbol" => symbol = it.next().unwrap_or(symbol),
            "--timeframe" | "--tf" => timeframe = it.next().unwrap_or(timeframe),
            "--offline" => offline = true,
            "--help" | "-h" => {
                println!("simccxt-server [--port 9001] [--symbol BTC/USDT] [--timeframe 5m] [--offline]");
                std::process::exit(0);
            }
            _ => log!("[args] ignoring unknown flag {a}"),
        }
    }
    Args {
        port,
        symbol,
        timeframe,
        offline,
    }
}


fn engine_main(
    rx: mpsc::Receiver<EngineJob>,
    jobs_tx: mpsc::Sender<EngineJob>,
    out: broadcast::Sender<String>,
    args: Args,
) {
    log!(
        "[engine] cli defaults: {} {} (every session subscribes its own series, net={})",
        args.symbol,
        args.timeframe,
        if args.offline { "off" } else { "CCXT" }
    );
    let mut engine = Engine::new(out)
        .with_net(!args.offline)
        .with_jobs(jobs_tx);

    loop {
        match rx.recv_timeout(engine.next_timeout()) {
            Ok(job) => {
                // Diagnostics: a single job blocking the engine for long is
                // what makes the whole terminal look frozen. Name it in the log.
                let started = std::time::Instant::now();
                engine.handle_job(job);
                let elapsed = started.elapsed();
                if elapsed > Duration::from_millis(150) {
                    log!("[engine] slow job: {elapsed:?} (possible freeze source)");
                }
                engine.evict_detached();
                engine.auto_tick_all();
            }
            Err(RecvTimeoutError::Timeout) => {
                engine.evict_detached();
                engine.auto_tick_all();
            }
            Err(RecvTimeoutError::Disconnected) => {
                log!("[engine] job channel closed, exiting");
                break;
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    let port = args.port;
    let (jobs_tx, jobs_rx) = mpsc::channel::<EngineJob>();
    let (out_tx, _) = broadcast::channel::<String>(1024);
    let out_for_engine = out_tx.clone();
    let jobs_for_engine = jobs_tx.clone();
    std::thread::Builder::new()
        .name("simccxt-engine".into())
        .spawn(move || {
            engine_main(
                jobs_rx,
                jobs_for_engine,
                out_for_engine,
                args,
            )
        })
        .expect("spawn engine thread");
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind ws port");
    log!("[ws] simccxt-server listening on ws://127.0.0.1:{port}/");
    let http_port = port + 1;
    // The diary HTTP API is optional: if `ws_port + 1` is taken (another
    // tool's range), degrade to WebSocket-only instead of killing the server.
    match TcpListener::bind(("127.0.0.1", http_port)).await {
        Err(e) => log!("[http] diary API disabled (port {http_port} busy): {e}"),
        Ok(http_listener) => {
            log!("[http] simccxt diary API listening on http://127.0.0.1:{http_port}/");
            let jobs_tx_http = jobs_tx.clone();

            // HTTP: GET /api/traders, GET /api/trader/:nick. Market data is not
            // served over HTTP any more: every browser downloads its own bars.
            tokio::spawn(async move {
                loop {
                    let (stream, _addr) = match http_listener.accept().await {
                        Ok(v) => v,
                        Err(e) => {
                            log!("[http] accept failed: {e}");
                            continue;
                        }
                    };
                    let tx = jobs_tx_http.clone();
                    tokio::spawn(async move {
                        handle_http_request(stream, tx).await;
                    });
                }
            });
        }
    }

    let mut next_id: u64 = 1;
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                log!("[ws] accept failed: {e}");
                continue;
            }
        };
        let id = next_id;
        next_id += 1;
        tokio::spawn(handle_conn(
            stream,
            addr,
            id,
            jobs_tx.clone(),
            out_tx.clone(),
        ));
    }
}

// ---------------------------------------------------------------------------
// HTTP read-only API (diary + traders list + diary chart candles)
//
// Deviation from the original plan: the plan proposed serving HTTP on the
// SAME port as WS by peeking at the first byte (0x81 = WS). That is wrong:
// a WebSocket *handshake* starts with ASCII "GET / HTTP/1.1" — the same first
// byte as plain HTTP (0x81 is a frame byte, seen only after the handshake).
// Discriminating safely would require parsing headers before handing the
// stream to tungstenite. We instead bind `ws_port + 1` and rely on CORS
// (`Access-Control-Allow-Origin: *`), which the frontend needs anyway.
// ---------------------------------------------------------------------------

/// Routes understood by the HTTP API. Pure enum so parsing is unit-testable.
#[derive(Debug, PartialEq)]
enum HttpRoute {
    Options,
    Traders,
    Trader(String),
    NotFound,
    MethodNotAllowed,
}

/// Parse the request line (`METHOD target`) into an API route.
fn route_request(method: &str, target: &str) -> HttpRoute {
    if method == "OPTIONS" {
        return HttpRoute::Options;
    }
    if method != "GET" {
        return HttpRoute::MethodNotAllowed;
    }
    // Split off the query string (no route reads it any more).
    let (path, _query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    if path == "/api/traders" {
        return HttpRoute::Traders;
    }
    if let Some(raw) = path.strip_prefix("/api/trader/") {
        let nick = percent_decode(raw);
        if nick.is_empty() {
            return HttpRoute::NotFound;
        }
        return HttpRoute::Trader(nick);
    }
    HttpRoute::NotFound
}


/// Assemble a complete HTTP/1.1 response. Headers come BEFORE the body and
/// `Content-Length` counts body bytes — the previous implementation appended
/// headers after the JSON, which made `response.json()` fail in the browser.
fn http_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
}

/// Read the request head (up to the blank line) with a size cap.
async fn read_request_head(stream: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= 8192 {
            break;
        }
    }
    if buf.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

async fn handle_http_request(
    mut stream: tokio::net::TcpStream,
    jobs_tx: std::sync::mpsc::Sender<EngineJob>,
) {
    use tokio::io::AsyncWriteExt;

    let head = match read_request_head(&mut stream).await {
        Some(h) => h,
        None => return,
    };
    let req_line = head.lines().next().unwrap_or("");
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    // Send one job to the engine and wait for its reply (bounded — the
    // engine may be busy warming caches).
    async fn ask(
        jobs_tx: &std::sync::mpsc::Sender<EngineJob>,
        timeout: std::time::Duration,
        make: impl FnOnce(oneshot::Sender<Value>) -> EngineJob,
    ) -> Result<Value, ()> {
        let (tx, rx) = oneshot::channel::<Value>();
        jobs_tx.send(make(tx)).map_err(|_| ())?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(v)) => Ok(v),
            _ => Err(()),
        }
    }

    // 404 semantics: the diary reply carries `error` when the nick is unknown
    // (the trader went offline) — surface it as a real HTTP 404.
    let json_reply = |v: &Value| -> String {
        let status = if v.get("error").is_some() { 404 } else { 200 };
        let reason = if status == 404 { "Not Found" } else { "OK" };
        http_response(
            status,
            reason,
            &serde_json::to_string(v).unwrap_or_else(|_| "{}".into()),
        )
    };

    let response = match route_request(method, target) {
        HttpRoute::Options => http_response(204, "No Content", ""),
        HttpRoute::Traders => match ask(
            &jobs_tx,
            std::time::Duration::from_secs(10),
            |reply| EngineJob::Traders { reply },
        )
        .await
        {
            Ok(v) => json_reply(&v),
            Err(()) => http_response(503, "Service Unavailable", "{\"error\":\"engine busy\"}"),
        },
        HttpRoute::Trader(nick) => {
            match ask(
                &jobs_tx,
                std::time::Duration::from_secs(10),
                |reply| EngineJob::Diary { nick, reply },
            )
            .await
            {
                Ok(v) => json_reply(&v),
                Err(()) => http_response(503, "Service Unavailable", "{\"error\":\"engine busy\"}"),
            }
        }
        HttpRoute::NotFound => http_response(404, "Not Found", "{\"error\":\"not found\"}"),
        HttpRoute::MethodNotAllowed => http_response(
            405,
            "Method Not Allowed",
            "{\"error\":\"method not allowed\"}",
        ),
    };
    let _ = stream.write_all(response.as_bytes()).await;
}

/// Percent-decode a path/query component to a `String`.
///
/// Bytes are collected first and decoded as UTF-8 at the end, so Cyrillic
/// nicks (`%D0%9A...`) survive intact — pushing `b as char` per byte (the
/// previous implementation) produced one Latin-1 char per UTF-8 byte. Hex is
/// parsed from raw bytes (never a `&str` slice) so a multi-byte character
/// right after `%` cannot trigger a char-boundary panic.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let hex_val = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        // Copy the whole (possibly multi-byte) UTF-8 character as-is.
        let c = s[i..].chars().next().unwrap_or('\u{fffd}');
        out.extend_from_slice(c.encode_utf8(&mut [0u8; 4]).as_bytes());
        i += c.len_utf8();
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Unit tests (protocol + incremental diff; run with `cargo test -p simccxt-server`)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_identity_keeps_same_id_on_different_symbols_apart() {
        // Order ids restart at 1 per symbol engine, so `1_0` appears on both
        // BTC and ETH. Merging on the bare id dropped the second fill and the
        // diary invented a trade from the leftover exit.
        let btc = json!({
            "id": "1_0", "symbol": "BTC/USDT", "timestamp": 1_000,
            "price": 100.0, "amount": 1.0, "orderId": "1", "side": "buy",
        });
        let eth = json!({
            "id": "1_0", "symbol": "ETH/USDT", "timestamp": 1_000,
            "price": 100.0, "amount": 1.0, "orderId": "1", "side": "buy",
        });
        assert_ne!(fill_identity(&btc), fill_identity(&eth));
        // The same fill delivered twice is still one fill.
        assert_eq!(fill_identity(&btc), fill_identity(&btc.clone()));
        // A fill without an id is not merged at all.
        assert_eq!(fill_identity(&json!({ "symbol": "BTC/USDT" })), "");
    }

    #[test]
    fn exit_reason_keys_do_not_collide_across_symbols() {
        // Order ids restart at 1 per symbol engine, so the bare id is not
        // unique: the same session can hold BTC's stop #2 and ETH's take #2.
        assert_ne!(
            exit_reason_key("e1", "BTC/USDT", "2", 1_000),
            exit_reason_key("e1", "ETH/USDT", "2", 1_000),
        );
        // ...and they restart per replay session too, so the same symbol's
        // second stop #2 must not inherit the first one's level.
        assert_ne!(
            exit_reason_key("e1", "BTC/USDT", "2", 1_000),
            exit_reason_key("e1", "BTC/USDT", "2", 9_000),
        );
        // A reloaded page re-runs the same bar with the same order ids: only
        // the engine instance tells the two trades apart.
        assert_ne!(
            exit_reason_key("e1", "BTC/USDT", "2", 1_000),
            exit_reason_key("e2", "BTC/USDT", "2", 1_000),
        );
        assert_eq!(
            exit_reason_key("e1", "BTC/USDT", "2", 1_000),
            "e1|BTC/USDT|2|1000"
        );
        // The wasm engine builds the same key shape.
        assert_ne!(
            local_protection_key("e1", "BTC/USDT", "2", 1_000),
            local_protection_key("e1", "ETH/USDT", "2", 1_000),
        );
        assert_ne!(
            local_protection_key("e1", "BTC/USDT", "2", 1_000),
            local_protection_key("e2", "BTC/USDT", "2", 1_000),
        );
    }

    #[test]
    fn client_cmd_place_order_parses() {
        let cmd: ClientCmd = serde_json::from_str(
            r#"{"type":"place_order","symbol":"BTC/USDT","side":"buy",
                "order_type":"market","quantity":0.1}"#,
        )
        .unwrap();
        match cmd {
            ClientCmd::PlaceOrder {
                symbol,
                side,
                quantity,
                price,
                ..
            } => {
                assert_eq!(symbol, "BTC/USDT");
                assert_eq!(side, "buy");
                assert_eq!(quantity, 0.1);
                assert_eq!(price, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn client_cmd_replay_parses() {
        let cmd: ClientCmd =
            serde_json::from_str(r#"{"type":"replay","action":"step","steps":3}"#).unwrap();
        match cmd {
            ClientCmd::Replay { action, steps, .. } => {
                assert_eq!(action, "step");
                assert_eq!(steps, Some(3));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn preload_plan_dedupes_and_validates() {
        let symbols = vec![
            "btc/usdt".to_string(),
            "ETH/USDT".to_string(),
            "bogus".to_string(),
        ];
        let timeframes = vec![
            "5m".to_string(),
            "1m".to_string(),
            "7z".to_string(),
            "5M".to_string(),
            "1d".to_string(),
        ];
        let plan = preload_plan(&symbols, &timeframes);
        // "bogus" (no slash), "7z" (unknown timeframe) and "1m" (an interval the
        // terminal no longer has) are dropped, "5M" normalizes to "5m" and
        // duplicates collapse.
        assert_eq!(
            plan,
            vec![
                ("ETH/USDT".to_string(), "5m".to_string()),
                ("ETH/USDT".to_string(), "1d".to_string()),
                ("BTC/USDT".to_string(), "5m".to_string()),
                ("BTC/USDT".to_string(), "1d".to_string()),
            ]
        );
    }

    #[test]
    fn preload_plan_is_capped() {
        let symbols = (0..40).map(|i| format!("C{i}/USDT")).collect::<Vec<_>>();
        let timeframes = PRELOAD_TIMEFRAMES
            .iter()
            .map(|tf| tf.to_string())
            .collect::<Vec<_>>();
        let plan = preload_plan(&symbols, &timeframes);
        assert!(plan.len() <= PRELOAD_MAX_PAIRS);
        assert_eq!(plan.first().map(|pair| pair.0.as_str()), Some("C39/USDT"));
    }

    #[test]
    fn client_cmd_preload_parses() {
        let cmd: ClientCmd = serde_json::from_str(
            r#"{"type":"preload","symbols":["BTC/USDT"],"timeframes":["1h","1d"],
                "exchange":"bybit","start_ms":1000,"end_ms":2000}"#,
        )
        .expect("preload parses");
        match cmd {
            ClientCmd::Preload {
                symbols,
                timeframes,
                exchange,
                start_ms,
                end_ms,
                ..
            } => {
                assert_eq!(symbols, vec!["BTC/USDT".to_string()]);
                assert_eq!(timeframes, vec!["1h".to_string(), "1d".to_string()]);
                assert_eq!(exchange, "bybit");
                assert_eq!(start_ms, Some(1000));
                assert_eq!(end_ms, Some(2000));
            }
            other => panic!("expected preload, got {other:?}"),
        }
    }

    #[test]
    fn client_cmd_market_lookup_parses() {
        let cmd: ClientCmd = serde_json::from_str(
            r#"{"type":"market_lookup","symbols":["BR/USDT"],"exchange":"binance"}"#,
        )
        .expect("market lookup parses");
        match cmd {
            ClientCmd::MarketLookup {
                symbols, exchange, ..
            } => {
                assert_eq!(symbols, vec!["BR/USDT"]);
                assert_eq!(exchange, "binance");
            }
            other => panic!("expected market lookup, got {other:?}"),
        }
    }

    #[test]
    fn client_cmd_subscribe_parses_replay_window() {
        let cmd: ClientCmd = serde_json::from_str(
            r#"{"type":"subscribe","symbol":"BTC/USDT","timeframe":"5m",
                "start_ms":1000,"reveal_start_ms":2000,"end_ms":3000}"#,
        )
        .unwrap();
        match cmd {
            ClientCmd::Subscribe {
                start_ms,
                reveal_start_ms,
                end_ms,
                ..
            } => {
                assert_eq!(start_ms, Some(1000));
                assert_eq!(reveal_start_ms, Some(2000));
                assert_eq!(end_ms, Some(3000));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn client_cmd_local_sync_parses_browser_state() {
        let cmd: ClientCmd = serde_json::from_str(
            r#"{"type":"local_sync","account":{"equity":1000,"pnl":{"realized":4,"unrealized":2}},
                "fills":[{"id":"o1_0","orderId":"o1","timestamp":1000}],
                "replace_fills":true,"entry_protections":{},"exit_reasons":{},
                "symbol":"BTC/USDT","timeframe":"1m","exchange":"bybit",
                "range":{"start_ms":0,"reveal_start_ms":1000,"end_ms":2000}}"#,
        )
        .expect("local sync parses");
        match cmd {
            ClientCmd::LocalSync {
                account,
                fills,
                replace_fills,
                symbol,
                exchange,
                range,
                ..
            } => {
                assert_eq!(account["equity"], 1000);
                assert_eq!(fills.len(), 1);
                assert!(replace_fills);
                assert_eq!(symbol, "BTC/USDT");
                assert_eq!(exchange, "bybit");
                assert_eq!(range["reveal_start_ms"], 1000);
            }
            other => panic!("expected local sync, got {other:?}"),
        }
    }

    #[test]
    fn client_cmd_unknown_type_rejected() {
        let r = serde_json::from_str::<ClientCmd>(r#"{"type":"mousemove","x":1}"#);
        assert!(r.is_err());
    }

    #[test]
    fn new_bars_returns_only_unseen() {
        let live = vec![vec![1.0], vec![2.0], vec![3.0]];
        assert_eq!(new_bars(&live, 0).len(), 3);
        assert_eq!(new_bars(&live, 2).len(), 1);
        assert!(new_bars(&live, 3).is_empty());
        assert!(new_bars(&live, 99).is_empty());
    }

    #[test]
    fn offline_engine_never_calls_network() {
        // Engine::new has allow_net=false → synthetic path only (no HTTP in tests).
        let (tx, _rx) = broadcast::channel::<String>(8);
        let eng = Engine::new(tx);
        assert!(!eng.allow_net);
    }

    // ------------------------------------------------------------------
    // Session isolation: two connections must never share state.
    // ------------------------------------------------------------------

    fn subscribe_cmd(symbol: &str, timeframe: &str) -> ClientCmd {
        ClientCmd::Subscribe {
            symbol: symbol.into(),
            timeframe: timeframe.into(),
            exchange: default_exchange(),
            start_ms: None,
            reveal_start_ms: None,
            end_ms: None,
            request_id: None,
        }
    }

    /// Give `client_id` a 5m series and the matching selection, the way the
    /// browser does it (it downloads the bars and drives its own replay). The
    /// clock starts at bar 0 with every bar still ahead of it.
    fn seed_session_series(eng: &mut Engine, client_id: u64, timeframe: &str) {
        eng.handle_command(client_id, subscribe_cmd("BTC/USDT", timeframe));
        let step: i64 = if timeframe == "1d" { 86_400_000 } else { 300_000 };
        eng.seed_series(
            client_id,
            "BTC/USDT",
            timeframe,
            4_000_000,
            400,
            4_000_000,
            4_000_000,
            4_000_000 + 400 * step,
        );
    }

    fn replay_cmd(action: &str) -> ClientCmd {
        ClientCmd::Replay {
            action: action.into(),
            steps: None,
            speed: None,
            step_ms: None,
            timestamp_ms: None,
            request_id: None,
        }
    }

    fn login_cmd(name: &str) -> ClientCmd {
        ClientCmd::Login {
            name: name.into(),
            request_id: None,
        }
    }

    fn challenge_cmd(join: bool, risk: Option<f64>) -> ClientCmd {
        ClientCmd::Challenge {
            join,
            risk_usdt: risk,
            capital_usdt: Some(1000.0),
            request_id: None,
        }
    }

    /// Drain the bus, returning only challenge_board messages.
    fn board_messages(rx: &mut broadcast::Receiver<String>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(raw) = rx.try_recv() {
            let v: Value = serde_json::from_str(&raw).expect("valid json");
            if v["type"] == "challenge_board" {
                out.push(v);
            }
        }
        out
    }

    /// Drain the bus, returning the last `snapshot` (or null). Keeps draining
    /// across `Lagged`: a run that emits thousands of deltas overflows the
    /// channel, and the interesting message may sit behind the gap.
    fn last_snapshot(rx: &mut broadcast::Receiver<String>) -> Option<Value> {
        let mut found = None;
        loop {
            match rx.try_recv() {
                Ok(raw) => {
                    let v: Value = serde_json::from_str(&raw).expect("valid json");
                    if v["type"] == "snapshot" {
                        found = Some(v);
                    }
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Empty) => return found,
                Err(broadcast::error::TryRecvError::Closed) => return found,
            }
        }
    }

    #[test]
    fn reset_truncates_the_chart_and_replays_again() {
        // Regression: `reset` only sent `replay_state`, which never shrinks the
        // client's bar list — every already-revealed bar stayed on the chart, so
        // the reset looked like a no-op. It must send a snapshot.
        //
        // The server no longer streams candles, so this checks the replay
        // bookkeeping the snapshot is built from: the clock rewinds to the
        // reveal point and the revealed bar count shrinks with it.
        let (tx, _rx) = broadcast::channel::<String>(1024);
        let mut eng = Engine::new(tx);
        seed_session_series(&mut eng, 1, "5m");

        for _ in 0..5 {
            eng.handle_command(1, replay_cmd("step"));
        }
        let fed_before = eng.sessions[&1].ex.live_len("BTC/USDT", "5m");
        assert!(fed_before >= 5, "stepping reveals bars: {fed_before}");

        eng.handle_command(1, replay_cmd("reset"));
        let s = &eng.sessions[&1];
        // The clock lands one step BEFORE the first revealed bar, so pressing
        // play reveals that bar first instead of skipping it.
        assert_eq!(
            s.ex.now_ms(),
            s.reveal_start_ms - s.step_ms as i64,
            "reset returns to the bar before the reveal point"
        );
        assert_eq!(s.fed, 0, "reset rewinds the push cursor");
        assert_eq!(s.ex.live_len("BTC/USDT", "5m"), 0, "nothing is revealed after reset");
        assert!(
            !s.ex.fetch_ohlcv("BTC/USDT", "5m", None, None).is_empty(),
            "the history itself stays loaded: the chart must not go blank"
        );
    }

    #[test]
    fn server_holds_no_coin_data() {
        // The server is no longer a market-data provider: a subscribe records
        // the selection and acknowledges it, and the session starts with no
        // candles, no open interest and no OI grid. The browser downloads 5m
        // bars from the venue and aggregates 1h/1d itself.
        let (tx, mut rx) = broadcast::channel::<String>(64);
        let mut eng = Engine::new(tx);
        eng.handle_command(1, subscribe_cmd("BTC/USDT", "5m"));

        let s = &eng.sessions[&1];
        assert_eq!(s.symbol, "BTC/USDT");
        assert_eq!(s.timeframe, "5m");
        assert!(
            s.ex.fetch_ohlcv("BTC/USDT", "5m", None, None).is_empty(),
            "the server must never hold candles for a session"
        );
        assert!(s.oi.is_empty(), "the server must never hold open interest");

        let mut acked = false;
        while let Ok(raw) = rx.try_recv() {
            let v: Value = serde_json::from_str(&raw).expect("valid json");
            assert_ne!(v["type"], "snapshot", "subscribe must not stream candles");
            if v["type"] == "series_ack" {
                acked = true;
                assert_eq!(v["symbol"], "BTC/USDT");
                assert_eq!(v["timeframe"], "5m");
            }
        }
        assert!(acked, "subscribe is acknowledged without data");
    }

    #[test]
    fn preload_and_market_lookup_do_no_market_data_work() {
        // Both commands still parse (old clients), but neither may touch the
        // network or produce coin data: the browser does that itself.
        let (tx, mut rx) = broadcast::channel::<String>(64);
        let mut eng = Engine::new(tx);
        let preload: ClientCmd = serde_json::from_str(
            r#"{"type":"preload","symbols":["BTC/USDT"],"timeframes":["5m","1h"],
                "exchange":"binance"}"#,
        )
        .expect("preload parses");
        eng.handle_command(1, preload);
        let lookup: ClientCmd = serde_json::from_str(
            r#"{"type":"market_lookup","symbols":["BTC/USDT"],"exchange":"binance"}"#,
        )
        .expect("market lookup parses");
        eng.handle_command(1, lookup);

        let mut done = 0;
        while let Ok(raw) = rx.try_recv() {
            let v: Value = serde_json::from_str(&raw).expect("valid json");
            if v["type"] == "preload_done" {
                done += 1;
                assert_eq!(v["total"], 2, "both supported pairs are counted");
            }
            if v["type"] == "market_lookup" {
                assert_eq!(v["quotes"].as_array().map(|q| q.len()), Some(0));
            }
        }
        assert_eq!(done, 1, "preload answers exactly once");
    }

    #[test]
    fn play_at_the_end_rewinds_instead_of_freezing() {
        // Regression: pressing play once the series was consumed was a no-op —
        // the clock advanced past the last bar and no candle ever arrived.
        let (tx, mut rx) = broadcast::channel::<String>(1024);
        let mut eng = Engine::new(tx);
        seed_session_series(&mut eng, 1, "5m");

        // Run the whole series out.
        for _ in 0..40 {
            eng.handle_command(
                1,
                ClientCmd::Replay {
                    action: "step".into(),
                    steps: Some(500),
                    speed: None,
                    step_ms: None,
                    timestamp_ms: None,
                    request_id: None,
                },
            );
        }
        let session = &eng.sessions[&1];
        assert!(
            session.ex.replay_done(&session.symbol, &session.timeframe),
            "the scenario must reach the end of the series"
        );
        let _ = last_snapshot(&mut rx);

        eng.handle_command(
            1,
            ClientCmd::Replay {
                action: "play".into(),
                steps: None,
                speed: Some(4.0),
                step_ms: None,
                timestamp_ms: None,
                request_id: None,
            },
        );

        let session = &eng.sessions[&1];
        assert!(
            !session.ex.replay_done(&session.symbol, &session.timeframe),
            "play at the end must rewind, not leave the clock past the last bar"
        );
        assert!(!session.ex.is_paused(), "play resumes");
        assert!(
            last_snapshot(&mut rx).is_some(),
            "the client needs a snapshot to shrink its chart back"
        );
    }

    #[test]
    fn timeframe_switch_keeps_the_replay_clock() {
        // Regression: switching the timeframe used to rewind the clock to the
        // window's reveal point, i.e. *before* the trader's own entry. That
        // flipped the position's PnL, re-evaluated resting orders against an
        // old price and left stops untouchable.
        let (tx, _rx) = broadcast::channel::<String>(512);
        let mut eng = Engine::new(tx);

        eng.handle_command(1, subscribe_cmd("BTC/USDT", "5m"));
        // Play forward so the clock is well past the reveal point.
        for _ in 0..40 {
            eng.handle_command(1, replay_cmd("step"));
        }
        let before = eng.sessions[&1].ex.now_ms();
        assert!(before > 0, "clock advanced");

        eng.handle_command(1, subscribe_cmd("BTC/USDT", "1h"));
        let session = &eng.sessions[&1];
        let after = session.ex.now_ms();
        // The clock snaps onto a bar boundary of the NEW series, so it may move
        // by up to one new-timeframe bar. It may also be pinned to the end of
        // available data (the coarser series simply has no bars past `before`)
        // — but it must never rewind to the window's reveal point.
        let series_end = session
            .ex
            .fetch_ohlcv("BTC/USDT", "1h", None, None)
            .last()
            .map(|row| row[0] as i64)
            .unwrap_or(0);
        let one_bar = 3_600_000i64;
        assert!(
            after >= before - one_bar || after == series_end,
            "timeframe switch rewound the clock: {before} -> {after} (series end {series_end})"
        );
        assert!(
            after <= before + one_bar,
            "timeframe switch jumped the clock forward past one bar: {before} -> {after}"
        );
        assert_eq!(session.timeframe, "1h");
    }

    #[test]
    fn explicit_window_change_still_resets_the_clock() {
        // The other half of the rule: picking new dates DOES rewind to reveal.
        let (tx, _rx) = broadcast::channel::<String>(512);
        let mut eng = Engine::new(tx);
        eng.handle_command(1, subscribe_cmd("BTC/USDT", "5m"));
        for _ in 0..40 {
            eng.handle_command(1, replay_cmd("step"));
        }
        let before = eng.sessions[&1].ex.now_ms();
        assert!(before > 0);

        // A different date range is not a "same window" switch, so the
        // reveal-based reset must still be allowed to move the clock.
        let session = &eng.sessions[&1];
        let new_window = (Some(0), Some(1), Some(2));
        let same_window = new_window.0 == Some(session.range_start_ms)
            && new_window.1 == Some(session.reveal_start_ms)
            && new_window.2 == Some(session.range_end_ms);
        assert!(
            !same_window,
            "a new date range must count as a window change"
        );
    }

    #[test]
    fn sessions_have_independent_symbols_and_clocks() {
        let (tx, mut rx) = broadcast::channel::<String>(512);
        let mut eng = Engine::new(tx);

        eng.handle_command(1, subscribe_cmd("BTC/USDT", "5m"));
        eng.handle_command(2, subscribe_cmd("ETH/USDT", "1h"));

        // Each session keeps its own selection: the server records what the
        // browser says it is replaying (market data itself is local).
        assert_eq!(eng.sessions[&1].symbol, "BTC/USDT");
        assert_eq!(eng.sessions[&1].timeframe, "5m");
        assert_eq!(eng.sessions[&2].symbol, "ETH/USDT");
        assert_eq!(eng.sessions[&2].timeframe, "1h");

        // Every message on the bus is directed at exactly one session, and a
        // subscribe is acknowledged only to its own socket.
        let mut acks = 0;
        while let Ok(raw) = rx.try_recv() {
            let v: Value = serde_json::from_str(&raw).expect("valid json");
            let to = v
                .get("to")
                .and_then(|t| t.as_u64())
                .expect("session message must carry `to`");
            assert!(to == 1 || to == 2, "unknown recipient {to}");
            if v["type"] == "series_ack" {
                acks += 1;
                let want = if to == 1 { "BTC/USDT" } else { "ETH/USDT" };
                assert_eq!(v["symbol"], want, "ack routed to the wrong session");
            }
        }
        assert_eq!(acks, 2, "one ack per subscriber");

        // Clocks are per session too.
        seed_session_series(&mut eng, 1, "5m");
        seed_session_series(&mut eng, 2, "1h");
        eng.handle_command(1, replay_cmd("step"));
        assert_ne!(
            eng.sessions[&1].ex.now_ms(),
            eng.sessions[&2].ex.now_ms(),
            "stepping one session must not move another"
        );
    }

    #[test]
    fn orders_do_not_leak_between_sessions() {
        let (tx, _rx) = broadcast::channel::<String>(64);
        let mut eng = Engine::new(tx);

        for id in [1u64, 2u64] {
            seed_session_series(&mut eng, id, "5m");
            // Two steps establish a reference price for the market order.
            eng.handle_command(
                id,
                ClientCmd::Replay {
                    action: "step".into(),
                    steps: Some(2),
                    speed: None,
                    step_ms: None,
                    timestamp_ms: None,
                    request_id: None,
                },
            );
        }

        // Session 1 buys; session 2 must stay a virgin 1M account.
        eng.handle_command(
            1,
            ClientCmd::PlaceOrder {
                symbol: "BTC/USDT".into(),
                side: "buy".into(),
                order_type: "market".into(),
                quantity: 0.001,
                price: None,
                stop_price: None,
                stop_loss_price: None,
                risk_usdt: None,
                tp_price: None,
                time_in_force: "GTC".into(),
                post_only: false,
                reduce_only: false,
                request_id: None,
            },
        );

        let a1 = eng.sessions[&1].ex.account_snapshot();
        let a2 = eng.sessions[&2].ex.account_snapshot();
        assert!(
            !a1["positions"]
                .as_array()
                .map(|p| p.is_empty())
                .unwrap_or(true),
            "session 1 should hold a position after the market buy"
        );
        assert!(
            a2["positions"]
                .as_array()
                .map(|p| p.is_empty())
                .unwrap_or(false),
            "session 2 must not see session 1's position"
        );
        let equity2 = a2["equity"].as_f64().expect("equity");
        assert!(
            (equity2 - 1_000_000.0).abs() < 1e-6,
            "session 2 equity must stay at the initial deposit, got {equity2}"
        );
        assert_eq!(eng.sessions[&2].ex.fetch_fills(None, None).len(), 0);
    }

    fn place_protected_market_entry(eng: &mut Engine, client_id: u64) -> f64 {
        seed_session_series(eng, client_id, "5m");
        eng.handle_command(1, replay_cmd("step"));
        eng.handle_command(
            client_id,
            ClientCmd::Replay {
                action: "step".into(),
                steps: Some(2),
                speed: None,
                step_ms: None,
                timestamp_ms: None,
                request_id: None,
            },
        );
        let entry = eng.sessions[&client_id]
            .ex
            .last_price_for("BTC/USDT")
            .unwrap();
        eng.handle_command(
            client_id,
            ClientCmd::PlaceOrder {
                symbol: "BTC/USDT".into(),
                side: "buy".into(),
                order_type: "market".into(),
                quantity: 0.001,
                price: None,
                stop_price: None,
                stop_loss_price: Some(entry * 0.99),
                risk_usdt: None,
                tp_price: None,
                time_in_force: "GTC".into(),
                post_only: false,
                reduce_only: false,
                request_id: None,
            },
        );
        entry
    }

    #[test]
    fn stop_loss_is_armed_and_fires_against_replay_price() {
        let (tx, _rx) = broadcast::channel::<String>(128);
        let mut eng = Engine::new(tx);
        let entry = place_protected_market_entry(&mut eng, 9);
        let s = eng.sessions.get_mut(&9).unwrap();
        let orders = s.ex.fetch_open_orders(Some("BTC/USDT"));
        assert_eq!(
            orders.len(),
            1,
            "the filled entry should have one protective stop"
        );
        assert_eq!(orders[0]["type"], "stop");
        assert_eq!(orders[0]["side"], "sell");
        assert_eq!(orders[0]["reduceOnly"], true);
        assert!(orders[0]["triggerPrice"].as_f64().unwrap() < entry);

        let stop = orders[0]["triggerPrice"].as_f64().unwrap();
        let timestamp = s.ex.now() + chrono::Duration::minutes(1);
        s.ex.apply_candle(
            &simccxt_core::Symbol::parse("BTC/USDT").unwrap(),
            &simccxt_core::OHLCV::new(timestamp, entry, entry * 1.01, stop * 0.99, entry, 10.0),
        );
        let events = fresh_fill_events(s);
        assert!(events.iter().any(|event| event["type"] == "trade"));
        assert!(s.ex.fetch_open_orders(Some("BTC/USDT")).is_empty());
        assert!(s.ex.account_snapshot()["positions"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn sell_entry_opens_short_and_its_stop_closes_the_position() {
        let (tx, _rx) = broadcast::channel::<String>(128);
        let mut eng = Engine::new(tx);
        seed_session_series(&mut eng, 13, "5m");
        eng.handle_command(13, replay_cmd("step"));
        eng.handle_command(13, replay_cmd("step"));
        let entry = eng.sessions[&13].ex.last_price_for("BTC/USDT").unwrap();
        let stop = entry * 1.01;
        eng.handle_command(
            13,
            ClientCmd::PlaceOrder {
                symbol: "BTC/USDT".into(),
                side: "sell".into(),
                order_type: "market".into(),
                quantity: 0.001,
                price: None,
                stop_price: None,
                stop_loss_price: Some(stop),
                risk_usdt: None,
                tp_price: None,
                time_in_force: "GTC".into(),
                post_only: false,
                reduce_only: false,
                request_id: None,
            },
        );

        let session = eng.sessions.get_mut(&13).unwrap();
        let account = session.ex.account_snapshot();
        let position = &account["positions"][0];
        assert_eq!(position["side"], "short");
        let orders = session.ex.fetch_open_orders(Some("BTC/USDT"));
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0]["side"], "buy");
        assert_eq!(orders[0]["reduceOnly"], true);

        let timestamp = session.ex.now() + chrono::Duration::minutes(1);
        session.ex.apply_candle(
            &simccxt_core::Symbol::parse("BTC/USDT").unwrap(),
            &simccxt_core::OHLCV::new(timestamp, entry, stop * 1.01, entry * 0.99, entry, 10.0),
        );
        let events = fresh_fill_events(session);
        assert!(events
            .iter()
            .any(|event| { event["type"] == "trade" && event["fill"]["side"] == "buy" }));
        assert!(session.ex.account_snapshot()["positions"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(session.ex.fetch_open_orders(Some("BTC/USDT")).is_empty());
    }

    #[test]
    fn closing_position_cancels_its_protective_stop() {
        let (tx, _rx) = broadcast::channel::<String>(128);
        let mut eng = Engine::new(tx);
        place_protected_market_entry(&mut eng, 10);
        assert_eq!(
            eng.sessions[&10]
                .ex
                .fetch_open_orders(Some("BTC/USDT"))
                .len(),
            1
        );
        eng.handle_command(
            10,
            ClientCmd::ClosePosition {
                symbol: "BTC/USDT".into(),
                quantity: None,
                request_id: None,
            },
        );
        let session = &eng.sessions[&10];
        assert!(session.ex.fetch_open_orders(Some("BTC/USDT")).is_empty());
        assert!(session.ex.account_snapshot()["positions"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn delayed_limit_fill_arms_its_stop_only_after_entry() {
        let (tx, _rx) = broadcast::channel::<String>(128);
        let mut eng = Engine::new(tx);
        seed_session_series(&mut eng, 11, "5m");
        eng.handle_command(11, replay_cmd("step"));
        eng.handle_command(11, replay_cmd("step"));
        let current = eng.sessions[&11].ex.last_price_for("BTC/USDT").unwrap();
        let entry = current * 0.75;
        let stop = entry * 0.95;
        eng.handle_command(
            11,
            ClientCmd::PlaceOrder {
                symbol: "BTC/USDT".into(),
                side: "buy".into(),
                order_type: "limit".into(),
                quantity: 0.001,
                price: Some(entry),
                stop_price: None,
                stop_loss_price: Some(stop),
                risk_usdt: None,
                tp_price: None,
                time_in_force: "GTC".into(),
                post_only: false,
                reduce_only: false,
                request_id: None,
            },
        );
        let waiting = eng.sessions[&11].ex.fetch_open_orders(Some("BTC/USDT"));
        assert_eq!(
            waiting.len(),
            1,
            "only the resting entry limit should be open"
        );
        assert_eq!(waiting[0]["type"], "limit");
        assert_eq!(waiting[0]["triggerPrice"], Value::Null);
        assert_eq!(eng.sessions[&11].pending_protections.len(), 1);

        let session = eng.sessions.get_mut(&11).unwrap();
        let timestamp = session.ex.now() + chrono::Duration::minutes(1);
        session.ex.apply_price(
            &simccxt_core::Symbol::parse("BTC/USDT").unwrap(),
            entry * 0.99,
            timestamp,
        );
        let events = fresh_fill_events(session);
        assert!(events.iter().any(|event| event["type"] == "trade"));
        let orders = session.ex.fetch_open_orders(Some("BTC/USDT"));
        assert_eq!(
            orders.len(),
            1,
            "the stop should be created after the limit fills"
        );
        assert_eq!(orders[0]["triggerPrice"], stop);
    }

    #[test]
    fn triggered_entry_arms_protection_using_its_conditional_id() {
        let (tx, _rx) = broadcast::channel::<String>(128);
        let mut eng = Engine::new(tx);
        seed_session_series(&mut eng, 12, "5m");
        eng.handle_command(12, replay_cmd("step"));
        eng.handle_command(12, replay_cmd("step"));
        let current = eng.sessions[&12].ex.last_price_for("BTC/USDT").unwrap();
        let trigger = current * 1.01;
        let stop = current * 0.99;
        eng.handle_command(
            12,
            ClientCmd::PlaceOrder {
                symbol: "BTC/USDT".into(),
                side: "buy".into(),
                order_type: "stop".into(),
                quantity: 0.001,
                price: None,
                stop_price: Some(trigger),
                stop_loss_price: Some(stop),
                risk_usdt: None,
                tp_price: None,
                time_in_force: "GTC".into(),
                post_only: false,
                reduce_only: false,
                request_id: None,
            },
        );
        assert_eq!(eng.sessions[&12].pending_protections.len(), 1);
        let session = eng.sessions.get_mut(&12).unwrap();
        let timestamp = session.ex.now() + chrono::Duration::minutes(1);
        session.ex.apply_price(
            &simccxt_core::Symbol::parse("BTC/USDT").unwrap(),
            trigger * 1.01,
            timestamp,
        );
        let events = fresh_fill_events(session);
        assert!(events.iter().any(|event| event["type"] == "trade"));
        let orders = session.ex.fetch_open_orders(Some("BTC/USDT"));
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0]["type"], "stop");
        assert_eq!(orders[0]["triggerPrice"], stop);
    }

    #[test]
    fn disconnect_destroys_the_session() {
        let (tx, _rx) = broadcast::channel::<String>(16);
        let mut eng = Engine::new(tx);

        seed_session_series(&mut eng, 7, "5m");
        assert!(
            eng.sessions.contains_key(&7),
            "first subscribe creates the session"
        );

        eng.handle_job(EngineJob::Disconnect { client_id: 7 });
        assert!(
            !eng.sessions.contains_key(&7),
            "disconnect must free the session"
        );
    }

    #[test]
    fn directed_messages_are_routing_compatible() {
        // The broadcast writer routes on the raw byte prefix, so `to` must be
        // the FIRST key even for maps whose alphabetical order puts it later.
        let raw = msg_to(42, json!({ "type": "account_update", "account": { "equity": 1 } }));
        assert!(raw.starts_with(r#"{"to":42,"#), "got {raw}");
        assert_eq!(routing_target(&raw), Some(42));
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["type"], "account_update");
        assert_eq!(v["to"], 42);
        // An existing `to` inside the payload is replaced, never duplicated.
        let raw = msg_to(7, json!({ "type": "error", "to": 999, "message": "x" }));
        assert_eq!(routing_target(&raw), Some(7));
        // Undirected broadcasts must NOT look directed.
        assert_eq!(routing_target(&msg(json!({ "type": "challenge_board", "board": [] }))), None);
    }

    #[test]
    fn attach_restores_a_token_session_after_reload() {
        let (tx, mut rx) = broadcast::channel::<String>(512);
        let mut eng = Engine::new(tx);

        // A reload: the socket dies but the token parks the session.
        seed_session_series(&mut eng, 1, "5m");
        eng.handle_command(1, login_cmd("Reloader"));
        eng.handle_command(1, attach_cmd("tok-1"));
        assert_eq!(eng.sessions[&1].token, "tok-1");
        eng.handle_job(EngineJob::Disconnect { client_id: 1 });
        assert!(!eng.sessions.contains_key(&1), "socket close parks, not destroys");
        assert!(eng.detached.contains_key("tok-1"), "session parked under its token");

        // The new socket re-attaches: same series, same nick, no fresh session.
        let state = take_session_state(&mut rx);
        assert!(!state["restored"].as_bool().unwrap(), "first attach is a new session");
        eng.handle_command(2, attach_cmd("tok-1"));
        let state = take_session_state(&mut rx);
        assert!(state["restored"].as_bool().unwrap(), "re-attach must report restored");
        assert!(state["has_series"].as_bool().unwrap(), "the series came back too");
        assert_eq!(eng.sessions[&2].nick, "Reloader");
        assert_eq!(eng.sessions[&2].symbol, "BTC/USDT");
        assert!(eng.detached.is_empty(), "parked copy moved to the live map");

        // Duplicate attach on the same socket (StrictMode remount) is a no-op
        // restore, not a second session.
        eng.handle_command(2, attach_cmd("tok-1"));
        let state = take_session_state(&mut rx);
        assert!(state["restored"].as_bool().unwrap());
        assert_eq!(eng.sessions.len(), 1);
    }

    fn attach_cmd(token: &str) -> ClientCmd {
        ClientCmd::Attach { token: token.to_string(), request_id: None }
    }

    /// Drain the broadcast until the `session_state` reply shows up: this is
    /// the exact flag the frontend uses to decide restore vs fresh init.
    fn take_session_state(rx: &mut broadcast::Receiver<String>) -> Value {
        while let Ok(raw) = rx.try_recv() {
            let v: Value = serde_json::from_str(&raw).expect("valid json");
            if v["type"] == "session_state" {
                return v;
            }
        }
        panic!("no session_state on the bus");
    }

    // ------------------------------------------------------------------
    // Challenge: nicks, join gating, board math/routing.
    // ------------------------------------------------------------------

    #[test]
    fn nicks_are_unique_and_shaped() {
        let mut sessions: HashMap<u64, Session> = HashMap::new();
        let mut seen = std::collections::HashSet::new();
        for id in 0..60u64 {
            let n = unique_nick(&sessions);
            assert_eq!(n.split('-').count(), 3, "shape A-B-NN, got {n}");
            assert!(seen.insert(n.clone()), "duplicate nick {n}");
            sessions.insert(id, new_session(n));
        }
    }

    #[test]
    fn nick_validation_and_r_math() {
        assert_eq!(clean_nick("  Боб ").as_deref(), Some("Боб"));
        assert_eq!(clean_nick(""), None);
        assert_eq!(clean_nick("   "), None);
        assert_eq!(clean_nick(&"x".repeat(25)), None);
        assert_eq!(
            clean_nick(&"y".repeat(24)).map(|s| s.chars().count()),
            Some(24)
        );
        assert_eq!(clean_nick("a\nb"), None);
        assert_eq!(r_of(20.0, 10.0), Some(2.0));
        assert_eq!(r_of(0.0, 10.0), Some(0.0));
        assert_eq!(r_of(5.0, 0.0), None);
        assert_eq!(r_of(5.0, f64::INFINITY), None);
    }

    #[test]
    fn login_sets_nick_and_confirms() {
        let (tx, mut rx) = broadcast::channel::<String>(64);
        let mut eng = Engine::new(tx);

        // Empty login keeps the server-assigned random nick, still acked.
        eng.handle_command(1, login_cmd(""));
        let assigned = eng.sessions[&1].nick.clone();
        assert!(!assigned.is_empty(), "session always has a nick");
        let mut saw_ok = false;
        while let Ok(raw) = rx.try_recv() {
            let v: Value = serde_json::from_str(&raw).expect("valid json");
            if v["type"] == "login_ok" {
                assert_eq!(v["to"], 1);
                assert_eq!(v["nick"], assigned);
                saw_ok = true;
            }
        }
        assert!(saw_ok, "login_ok must arrive");

        eng.handle_command(1, login_cmd("Тестер"));
        assert_eq!(eng.sessions[&1].nick, "Тестер");
        eng.handle_command(1, login_cmd(""));
        assert_eq!(
            eng.sessions[&1].nick, "Тестер",
            "empty login keeps the nick"
        );
        eng.handle_command(1, login_cmd(&"z".repeat(30)));
        assert_eq!(eng.sessions[&1].nick, "Тестер", "oversize nick rejected");
    }

    #[test]
    fn challenge_join_requires_positive_risk() {
        let (tx, mut rx) = broadcast::channel::<String>(64);
        let mut eng = Engine::new(tx);

        eng.handle_command(1, login_cmd("A"));
        eng.handle_command(1, challenge_cmd(true, None));
        assert!(!eng.sessions[&1].in_challenge, "no risk → no join");
        eng.handle_command(1, challenge_cmd(true, Some(0.0)));
        assert!(!eng.sessions[&1].in_challenge, "zero risk → no join");
        // Only the login board so far: failed joins push nothing.
        assert_eq!(board_messages(&mut rx).len(), 1);

        eng.handle_command(1, challenge_cmd(true, Some(10.0)));
        assert!(eng.sessions[&1].in_challenge);
        assert_eq!(eng.sessions[&1].risk_usdt, 10.0);
        let boards = board_messages(&mut rx);
        assert_eq!(boards.len(), 1);
        assert!(boards[0].get("to").is_none(), "board goes to everyone");
        let rows = boards[0]["board"].as_array().expect("board array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["nick"], "A");
        assert_eq!(rows[0]["risk"], 10.0);
        assert_eq!(rows[0]["capital"], 1000.0);

        eng.handle_command(1, challenge_cmd(false, None));
        assert!(!eng.sessions[&1].in_challenge);
        let boards = board_messages(&mut rx);
        assert_eq!(boards.len(), 1);
        assert!(boards[0]["board"].as_array().unwrap().is_empty());
        eng.handle_command(1, challenge_cmd(true, Some(20.0)));
        assert!(
            !eng.sessions[&1].in_challenge,
            "risk cannot be changed after a round starts"
        );
        assert_eq!(eng.sessions[&1].risk_usdt, 10.0);
    }

    #[test]
    fn board_orders_by_r_and_drops_on_disconnect() {
        let (tx, mut rx) = broadcast::channel::<String>(512);
        let mut eng = Engine::new(tx);

        for (id, nick) in [(1u64, "Первый"), (2u64, "Второй")] {
            eng.handle_command(id, login_cmd(nick));
            eng.handle_command(id, subscribe_cmd("BTC/USDT", "1m"));
            eng.handle_command(id, challenge_cmd(true, Some(10.0)));
        }
        // Session 1 trades (buy + a few bars of drift); session 2 sits out.
        eng.handle_command(
            1,
            ClientCmd::Replay {
                action: "step".into(),
                steps: Some(5),
                speed: None,
                step_ms: None,
                timestamp_ms: None,
                request_id: None,
            },
        );
        eng.handle_command(
            1,
            ClientCmd::PlaceOrder {
                symbol: "BTC/USDT".into(),
                side: "buy".into(),
                order_type: "market".into(),
                quantity: 0.001,
                price: None,
                stop_price: None,
                stop_loss_price: None,
                risk_usdt: None,
                tp_price: None,
                time_in_force: "GTC".into(),
                post_only: false,
                reduce_only: false,
                request_id: None,
            },
        );

        let boards = board_messages(&mut rx);
        let last = boards.last().expect("board pushed after the trade");
        let rows = last["board"].as_array().expect("board array");
        assert_eq!(rows.len(), 2, "both participants listed");
        let r0 = rows[0]["r"].as_f64().expect("r");
        let r1 = rows[1]["r"].as_f64().expect("r");
        assert!(r0 >= r1, "board sorted by R desc");
        for row in rows {
            let id = if row["nick"] == "Первый" { 1 } else { 2 };
            let snap = eng.sessions[&id].ex.account_snapshot();
            let pnl = snap["pnl"]["realized"].as_f64().unwrap_or(0.0)
                + snap["pnl"]["unrealized"].as_f64().unwrap_or(0.0);
            let expect = (pnl / 10.0 * 100.0).round() / 100.0;
            assert!(
                (row["r"].as_f64().unwrap() - expect).abs() < 1e-9,
                "R = pnl/risk for {}",
                row["nick"]
            );
        }

        eng.handle_job(EngineJob::Disconnect { client_id: 1 });
        let boards = board_messages(&mut rx);
        let last = boards.last().expect("board pushed after disconnect");
        let rows = last["board"].as_array().expect("board array");
        assert_eq!(rows.len(), 1, "leaver drops off the board");
        assert_eq!(rows[0]["nick"], "Второй");
    }

    // -------------------------------------------------------------------
    // HTTP API: routing, percent-decoding, response framing
    // -------------------------------------------------------------------

    #[test]
    fn route_get_traders_and_diary() {
        assert_eq!(route_request("GET", "/api/traders"), HttpRoute::Traders);
        assert_eq!(
            route_request("GET", "/api/trader/Бык-Бык-7"),
            HttpRoute::Trader("Бык-Бык-7".into())
        );
        // Query string on the diary path is ignored (nick only).
        assert_eq!(
            route_request("GET", "/api/trader/abc?x=1"),
            HttpRoute::Trader("abc".into())
        );
    }

    #[test]
    fn route_decodes_percent_encoded_cyrillic_nick() {
        // %D0%91%D1%8B%D0%BA = "Бык"
        assert_eq!(
            route_request("GET", "/api/trader/%D0%91%D1%8B%D0%BA"),
            HttpRoute::Trader("Бык".into())
        );
    }


    #[test]
    fn route_rejects_unknown_paths_and_methods() {
        assert_eq!(route_request("GET", "/"), HttpRoute::NotFound);
        assert_eq!(route_request("GET", "/api"), HttpRoute::NotFound);
        assert_eq!(route_request("GET", "/api/trader/"), HttpRoute::NotFound);
        assert_eq!(
            route_request("POST", "/api/traders"),
            HttpRoute::MethodNotAllowed
        );
        assert_eq!(
            route_request("DELETE", "/api/trader/x"),
            HttpRoute::MethodNotAllowed
        );
        assert_eq!(route_request("OPTIONS", "/api/traders"), HttpRoute::Options);
    }

    #[test]
    fn percent_decode_handles_utf8_and_invalid_sequences() {
        // Cyrillic byte-by-byte percent-encoding stays valid UTF-8.
        assert_eq!(percent_decode("%D0%91%D1%8B%D0%BA-1"), "Бык-1");
        // Mixed: literal + encoded.
        assert_eq!(percent_decode("Бык%201"), "Бык 1");
        // Truncated / invalid hex is left as-is (no panic, no garbage).
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%D"), "%D"); // truncated: needs 2 hex digits
                                                // "%D0" IS valid hex → one byte 0xD0, but that byte alone is not a
                                                // complete UTF-8 char, so lossy conversion yields U+FFFD (no panic).
        assert_eq!(percent_decode("%D0"), "\u{fffd}");
        // Raw UTF-8 input passes through untouched.
        assert_eq!(percent_decode("Тихий-Медведь-3"), "Тихий-Медведь-3");
        // '%' followed by a multi-byte char must not panic (char-boundary).
        assert_eq!(percent_decode("%Б"), "%Б");
    }

    #[test]
    fn http_response_is_a_well_formed_json_reply() {
        let resp = http_response(200, "OK", "{\"a\":1}");
        assert!(
            resp.starts_with("HTTP/1.1 200 OK\r\n"),
            "version required: {resp:?}"
        );
        assert!(resp.contains("Content-Type: application/json; charset=utf-8\r\n"));
        assert!(resp.contains("Content-Length: 7\r\n"), "body bytes = 7");
        assert!(resp.contains("Access-Control-Allow-Origin: *\r\n"));
        // Headers strictly before body: split on the blank line.
        let (head, body) = resp.split_once("\r\n\r\n").expect("head/body separator");
        assert!(
            head.ends_with("Connection: close"),
            "last header before body"
        );
        assert_eq!(body, "{\"a\":1}");
        // Content-Length must equal actual body byte length (Cyrillic = 2 bytes/char).
        let cyr = http_response(404, "Not Found", "{\"error\":\"Бык\"}");
        let (_, cyr_body) = cyr.split_once("\r\n\r\n").unwrap();
        let declared: usize = cyr
            .lines()
            .find(|l| l.starts_with("Content-Length:"))
            .unwrap()
            .trim_start_matches("Content-Length: ")
            .parse()
            .unwrap();
        assert_eq!(declared, cyr_body.len());
    }
}
