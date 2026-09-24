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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};
use futures_util::{FutureExt, SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use simccxt_broker::MarketKind;
use simccxt_core::{Symbol, OHLCV};
use simccxt_exchange::{CreateOrderParams, Exchange};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

// ---------------------------------------------------------------------------
// PROTOCOL (must stay in sync with frontend/src/types/protocol.ts)
// ---------------------------------------------------------------------------
//
// Frontend -> Rust ("type" tagged):
//   {"type":"subscribe","symbol":"BTC/USDT","timeframe":"1m","request_id":".."}
//   {"type":"place_order","symbol":..,"side":"buy","order_type":"market",
//    "quantity":0.1,"price":null,"stop_price":null,
//    "time_in_force":"GTC","post_only":false,"reduce_only":false}
//   {"type":"cancel_order","order_id":"..","symbol":"BTC/USDT"}
//   {"type":"close_position","symbol":"BTC/USDT","quantity":null}
//   {"type":"replay","action":"play|pause|step|reset|set_speed|set_step",
//    "steps":1,"speed":2.0,"step_ms":60000,"timestamp_ms":null}
//   {"type":"login","name":"..","request_id":".."}   (empty name = keep current nick)
//   {"type":"challenge","join":true,"risk_usdt":10,"request_id":".."}
//    (join needs risk_usdt>0; resend join to update the risk)
//   {"type":"ping"}
//
// Rust -> frontend:
//   {"type":"welcome","client_id":7}
//   {"type":"snapshot","to":7,"symbol":..,"timeframe":..,"candles":[[ts,o,h,l,c,v]..],
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
//   {"type":"error","to":7,"message":"..","request_id":".."}
//   {"type":"pong","to":7}
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
// on login / join / leave / disconnect / new fills. In-memory only: a server
// restart empties the board. No passwords — nicks are not authenticated.
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

// ---------------------------------------------------------------------------
// Client -> server commands
// ---------------------------------------------------------------------------

/// A command sent by the frontend. Unknown `type` values are rejected.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientCmd {
    Subscribe { symbol: String, timeframe: String, request_id: Option<String> },
    PlaceOrder {
        symbol: String,
        side: String,
        order_type: String,
        quantity: f64,
        price: Option<f64>,
        stop_price: Option<f64>,
        #[serde(default)]
        time_in_force: String,
        #[serde(default)]
        post_only: bool,
        #[serde(default)]
        reduce_only: bool,
        request_id: Option<String>,
    },
    CancelOrder { order_id: String, symbol: String, request_id: Option<String> },
    ClosePosition { symbol: String, quantity: Option<f64>, request_id: Option<String> },
    Login { #[serde(default)] name: String, request_id: Option<String> },
    Challenge {
        join: bool,
        #[serde(default)]
        risk_usdt: Option<f64>,
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
    Ping,
}

/// One unit of work for the engine thread: a command from a client, or the
/// teardown of that client's session.
enum EngineJob {
    Command { client_id: u64, cmd: ClientCmd },
    Disconnect { client_id: u64 },
}

// ---------------------------------------------------------------------------
// Engine (owns the Exchange, single thread)
// ---------------------------------------------------------------------------

/// Outgoing message helper: serialize to a compact JSON string.
fn msg(v: Value) -> String {
    serde_json::to_string(&v).unwrap_or_else(|_| r#"{"type":"error","message":"encode"}"#.into())
}

/// Timestamp prefix for server logs (so connect/disconnect storms are debuggable).
fn stamp() -> String {
    chrono::Utc::now().format("%H:%M:%S").to_string()
}

macro_rules! log {
    ($($t:tt)*) => {
        eprintln!("{} {}", stamp(), format!($($t)*))
    };
}

/// Bars in `live` that the client has not seen yet (client has seen `fed`).
/// Pure helper so it can be unit-tested: returns the slice of new rows.
fn new_bars(live: &[Vec<f64>], fed: usize) -> &[Vec<f64>] {
    if fed >= live.len() { &[] } else { &live[fed..] }
}

/// One WebSocket connection = one isolated session: its own `Exchange` (own
/// account/orders/positions), own series, own replay clock and speed, own
/// push cursors. Nothing is shared between sessions — except the opt-in
/// challenge board (nick + stats only, see CHALLENGE above).
struct Session {
    ex: Exchange,
    symbol: String,
    timeframe: String,
    speed: f64,   // ticks per second while playing
    step_ms: u64, // mirrored from Exchange::set_step_ms (no getter there)
    fed: usize,   // candles already pushed to this client
    fills_seen: usize,
    last_beat: std::time::Instant, // last heartbeat to this client
    last_step: std::time::Instant, // last auto-tick (per-session speed)
    nick: String,      // challenge display name (random at creation)
    in_challenge: bool, // opt-in: publish nick+stats on the shared board
    risk_usdt: f64,    // fixed risk per trade, sent by the frontend on join
    oi: Vec<(i64, f64)>, // open-interest history (ts_ms, USDT value), context only
    oi_period: String, // OI grid actually loaded ("5m", "" when none)
}

// ---------------------------------------------------------------------------
// Challenge: random nicks + shared leaderboard (in-memory, no passwords)
// ---------------------------------------------------------------------------

/// Max nick length in chars (not bytes — Cyrillic is 2 bytes/char).
const MAX_NICK_LEN: usize = 24;

const NICK_A: [&str; 20] = [
    "Быстрый", "Тихий", "Стальной", "Хитрый", "Смелый", "Резкий", "Меткий",
    "Дерзкий", "Умный", "Спокойный", "Ловкий", "Жадный", "Терпеливый", "Зоркий",
    "Крепкий", "Шустрый", "Мудрый", "Дикий", "Холодный", "Цепкий",
];
const NICK_B: [&str; 20] = [
    "Бык", "Медведь", "Волк", "Сокол", "Акула", "Тигр", "Ястреб", "Кит",
    "Скорпион", "Ворон", "Барс", "Койот", "Филин", "Соболь", "Кречет",
    "Тайфун", "Штиль", "Откат", "Пробой", "Флет",
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
        .wrapping_add(SALT.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15))
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
        let snap = s.ex.account_snapshot();
        let realized = snap["pnl"]["realized"].as_f64().unwrap_or(0.0);
        let unrealized = snap["pnl"]["unrealized"].as_f64().unwrap_or(0.0);
        let equity = snap["equity"].as_f64().unwrap_or(0.0);
        let pnl = realized + unrealized;
        let r = r_of(pnl, s.risk_usdt).unwrap_or(0.0);
        rows.push((r, json!({
            "nick": s.nick,
            "risk": s.risk_usdt,
            "pnl": pnl,
            "r": (r * 100.0).round() / 100.0,
            "equity": (equity * 100.0).round() / 100.0,
        })));
    }
    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    json!({ "type": "challenge_board", "board": rows.into_iter().map(|(_, v)| v).collect::<Vec<_>>() })
}

struct Engine {
    sessions: HashMap<u64, Session>,
    out: broadcast::Sender<String>,
    /// Missing series may be downloaded from Binance (false in unit tests,
    /// and when the server is started with `--offline`).
    allow_net: bool,
}

/// Heartbeat period: even a paused replay emits traffic so the frontend can
/// tell "idle" apart from "disconnected".
const HEARTBEAT_SECS: u64 = 10;

/// Wall-clock interval between auto-steps at `speed` ticks/second.
fn speed_interval(speed: f64) -> Duration {
    let ms = (1000.0 / speed.max(0.05)).round() as u64;
    Duration::from_millis(ms.clamp(10, 5000))
}

fn replay_json(s: &Session) -> Value {
    json!({
        "type": "replay_state",
        "paused": s.ex.is_paused(),
        "now_ms": s.ex.now_ms(),
        "speed": s.speed,
        "step_ms": s.step_ms,
        "done": s.ex.replay_done(&s.symbol, &s.timeframe),
        "fed": s.ex.fetch_ohlcv_live(&s.symbol, &s.timeframe).len(),
        "total": s.ex.fetch_ohlcv(&s.symbol, &s.timeframe, None, None).len(),
    })
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
        "candles": rows,
        "account": s.ex.account_snapshot(),
        "replay": replay_json(s),
        "oi": s.oi,
        "oi_period": s.oi_period,
        "request_id": request_id,
    })
}

fn account_json(s: &Session) -> Value {
    json!({ "type": "account_update", "account": s.ex.account_snapshot() })
}

fn error_json(message: String, request_id: Option<String>) -> Value {
    json!({ "type": "error", "message": message, "request_id": request_id })
}

/// Deltas after one step: new candles + market/trades/account/replay.
fn delta_events(s: &mut Session) -> Vec<Value> {
    let mut ev = Vec::new();
    let live = s.ex.fetch_ohlcv_live(&s.symbol, &s.timeframe);
    for bar in new_bars(&live, s.fed) {
        ev.push(json!({
            "type": "candle",
            "symbol": s.symbol,
            "timeframe": s.timeframe,
            "candle": bar,
            "is_new": true,
        }));
    }
    s.fed = live.len();
    let last = live.last().map(|r| r[4]).unwrap_or(0.0);
    ev.push(json!({
        "type": "market_update",
        "symbol": s.symbol,
        "last": last,
        "timestamp": s.ex.now_ms(),
    }));
    // New fills produced by the step (e.g. triggered stop/TP).
    let fills = s.ex.fetch_fills(None, None);
    if fills.len() > s.fills_seen {
        // fetch_fills is newest-first; take the unknown prefix and
        // re-emit oldest-first so the UI appends in order.
        let mut fresh: Vec<&Value> = fills.iter().take(fills.len() - s.fills_seen).collect();
        fresh.reverse();
        for f in fresh {
            ev.push(json!({ "type": "trade", "fill": f }));
        }
        s.fills_seen = fills.len();
    }
    ev.push(account_json(s));
    ev.push(replay_json(s));
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
        speed: DEFAULT_SPEED,
        step_ms: 60_000,
        fed: 0,
        fills_seen: 0,
        last_beat: std::time::Instant::now(),
        last_step: std::time::Instant::now(),
        nick,
        in_challenge: false,
        risk_usdt: 0.0,
        oi: Vec::new(),
        oi_period: String::new(),
    }
}

/// Switch the replay series of ONE session. Returns a human-readable note.
fn use_series(
    s: &mut Session,
    symbol: String,
    timeframe: String,
    data_dir: &Path,
    allow_net: bool,
) -> String {
    s.symbol = symbol;
    s.timeframe = timeframe;
    s.step_ms = tf_step_ms(&s.timeframe);
    s.ex.set_step_ms(s.step_ms);
    ensure_series(&mut s.ex, &s.symbol, &s.timeframe, data_dir, allow_net);
    ensure_oi(&mut s.oi, &mut s.oi_period, &s.symbol, &s.timeframe, data_dir, allow_net);
    let rows = s.ex.fetch_ohlcv(&s.symbol, &s.timeframe, None, None);
    if let Some(first) = rows.first() {
        let ts = DateTime::from_timestamp_millis(first[0] as i64).unwrap_or_else(Utc::now);
        s.ex.reset(ts);
    }
    s.ex.resume();
    s.fed = 0;
    s.fills_seen = s.ex.fetch_fills(None, None).len();
    s.last_step = std::time::Instant::now();
    format!("{} {} ({} bars)", s.symbol, s.timeframe, rows.len())
}

impl Engine {
    /// Unit-test constructor: never touches the network.
    fn new(out: broadcast::Sender<String>) -> Self {
        Engine { sessions: HashMap::new(), out, allow_net: false }
    }

    fn with_net(mut self, allow_net: bool) -> Self {
        self.allow_net = allow_net;
        self
    }

    /// Everything the engine says to a session carries `to`; the per-socket
    /// writer drops messages addressed to other clients.
    fn send_to(&self, to: u64, mut v: Value) {
        v["to"] = json!(to);
        let _ = self.out.send(msg(v));
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

    /// Broadcast the challenge board to EVERYONE (no `to` — the writer
    /// forwards such messages to all sockets). Cheap: only participants.
    fn push_board(&self) {
        let _ = self.out.send(msg(challenge_board_json(&self.sessions)));
    }

    fn handle_command(&mut self, to: u64, cmd: ClientCmd, data_dir: &Path) {
        // Every event is collected first and sent after the match: the
        // session borrow must end before self.send_all.
        let mut evs: Vec<Value> = Vec::new();
        // Set when the shared challenge board changed and must be
        // re-broadcast to everyone after the directed events go out.
        let mut board_push = false;
        match cmd {
            ClientCmd::Ping => {
                evs.push(json!({ "type": "pong" }));
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
            ClientCmd::Challenge { join, risk_usdt, request_id } => {
                let s = self.session(to);
                if join {
                    match risk_usdt {
                        Some(r) if r > 0.0 && r.is_finite() => {
                            s.risk_usdt = r;
                            s.in_challenge = true;
                            log!("[engine] #{to} ({}) joins the challenge, risk ${r}", s.nick);
                            evs.push(json!({
                                "type": "challenge_ok", "joined": true,
                                "nick": s.nick, "request_id": request_id }));
                            board_push = true;
                        }
                        _ => evs.push(error_json(
                            "challenge: нужен риск > 0 (поле «Риск» в сделке)".into(),
                            request_id,
                        )),
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
            ClientCmd::Subscribe { symbol, timeframe, request_id } => {
                let allow_net = self.allow_net;
                let s = self.session(to);
                if s.symbol.is_empty() || symbol != s.symbol || timeframe != s.timeframe {
                    let note = use_series(s, symbol, timeframe, data_dir, allow_net);
                    log!("[engine] #{to} subscribe -> {note}");
                }
                // (Re)sync the fed cursor to "everything live so far", then
                // send the full snapshot; later steps only push deltas.
                s.fed = s.ex.fetch_ohlcv_live(&s.symbol, &s.timeframe).len();
                evs.push(snapshot_json(s, request_id));
            }
            ClientCmd::PlaceOrder {
                symbol, side, order_type, quantity, price, stop_price,
                time_in_force, post_only, reduce_only, request_id,
            } => {
                let s = self.session(to);
                let mut p = CreateOrderParams::new();
                p.stop_price = stop_price;
                p.reduce_only = reduce_only;
                p.post_only = post_only;
                if !time_in_force.is_empty() {
                    if let Some(tif) = parse_tif(&time_in_force) {
                        p.time_in_force = tif;
                    }
                }
                match s.ex.create_order(&symbol, &order_type, &side, quantity, price, p) {
                    Ok(order) => {
                        evs.push(json!({
                            "type": "order_update", "order": order, "request_id": request_id }));
                        let fills = s.ex.fetch_fills(None, None);
                        if fills.len() > s.fills_seen {
                            let mut fresh: Vec<&Value> =
                                fills.iter().take(fills.len() - s.fills_seen).collect();
                            fresh.reverse();
                            for f in fresh {
                                evs.push(json!({ "type": "trade", "fill": f }));
                            }
                            s.fills_seen = fills.len();
                            // A fill moves PnL: refresh the board for everyone.
                            if s.in_challenge {
                                board_push = true;
                            }
                        }
                        evs.push(account_json(s));
                    }
                    Err(e) => evs.push(error_json(format!("place_order: {e}"), request_id)),
                }
            }
            ClientCmd::CancelOrder { order_id, symbol, request_id } => {
                let s = self.session(to);
                match s.ex.cancel_order(&order_id, &symbol) {
                    Ok(order) => {
                        evs.push(json!({
                            "type": "order_update", "order": order, "request_id": request_id }));
                        evs.push(account_json(s));
                    }
                    Err(e) => evs.push(error_json(format!("cancel_order: {e}"), request_id)),
                }
            }
            ClientCmd::ClosePosition { symbol, quantity, request_id } => {
                let s = self.session(to);
                let (held, _) = s.ex.spot_basis(&symbol);
                let q = quantity.unwrap_or(held);
                if q <= 0.0 {
                    evs.push(error_json("nothing to close".into(), request_id));
                } else {
                    match s.ex.create_order(
                        &symbol, "market", "sell", q, None, CreateOrderParams::new(),
                    ) {
                        Ok(order) => {
                            evs.push(json!({
                                "type": "order_update", "order": order,
                                "request_id": request_id }));
                            evs.push(account_json(s));
                            // A close realizes PnL (fills_seen intentionally
                            // untouched — trade-event behavior unchanged).
                            if s.ex.fetch_fills(None, None).len() > s.fills_seen
                                && s.in_challenge
                            {
                                board_push = true;
                            }
                        }
                        Err(e) => evs.push(error_json(format!("close_position: {e}"), request_id)),
                    }
                }
            }
            ClientCmd::Replay { action, steps, speed, step_ms, timestamp_ms, request_id } => {
                let known = matches!(
                    action.as_str(),
                    "play" | "pause" | "step" | "reset" | "set_speed" | "set_step"
                );
                if !known {
                    evs.push(error_json(format!("unknown replay action: {action}"), request_id));
                } else {
                    let s = self.session(to);
                    match action.as_str() {
                        "play" => {
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
                            let fills_before = s.ex.fetch_fills(None, None).len();
                            s.ex.step_n(steps.unwrap_or(1).max(1));
                            if was_paused {
                                s.ex.pause();
                            }
                            s.last_step = std::time::Instant::now();
                            let d = delta_events(s);
                            if s.ex.fetch_fills(None, None).len() > fills_before
                                && s.in_challenge
                            {
                                board_push = true;
                            }
                            evs.extend(d);
                        }
                        "reset" => {
                            let rows = s.ex.fetch_ohlcv(&s.symbol, &s.timeframe, None, None);
                            let ts_ms = timestamp_ms.unwrap_or(
                                rows.first().map(|r| r[0] as i64).unwrap_or_else(|| s.ex.now_ms()),
                            );
                            if let Some(ts) = DateTime::from_timestamp_millis(ts_ms) {
                                s.ex.reset(ts);
                            }
                            // Sync the cursor to the new clock (the client follows
                            // up with subscribe -> full snapshot). Broadcasting
                            // every live bar as deltas here would be a burst of
                            // thousands of messages; deltas resume from here.
                            s.fed = s.ex.fetch_ohlcv_live(&s.symbol, &s.timeframe).len();
                            s.last_step = std::time::Instant::now();
                            evs.push(account_json(s));
                        }
                        "set_speed" => {
                            if let Some(v) = speed {
                                if v > 0.0 && v <= 100.0 {
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
                        if action == "play" && v > 0.0 && v <= 100.0 {
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

    fn handle_job(&mut self, job: EngineJob, data_dir: &Path) {
        match job {
            EngineJob::Command { client_id, cmd } => self.handle_command(client_id, cmd, data_dir),
            EngineJob::Disconnect { client_id } => {
                if let Some(s) = self.sessions.remove(&client_id) {
                    log!("[engine] session #{client_id} ({}) closed", s.nick);
                    if s.in_challenge {
                        self.push_board();
                    }
                }
            }
        }
    }

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
            emissions.push((id, delta_events(s)));
        }
        // Auto-step fills move PnL: one board refresh if any participant filled.
        let mut board_dirty = false;
        for (id, evs) in &emissions {
            let filled = evs
                .iter()
                .any(|v| v.get("type").and_then(|t| t.as_str()) == Some("trade"));
            if filled && self.sessions.get(id).is_some_and(|s| s.in_challenge) {
                board_dirty = true;
                break;
            }
        }
        for (id, evs) in emissions {
            self.send_all(id, evs);
        }
        if board_dirty {
            self.push_board();
        }
    }

    /// Per-session heartbeat: lets the frontend distinguish a paused replay
    /// from a dead connection (every HEARTBEAT_SECS per session).
    fn heartbeat_all(&mut self) {
        let now = std::time::Instant::now();
        let mut beats: Vec<(u64, Value)> = Vec::new();
        for (&id, s) in &mut self.sessions {
            if s.last_beat.elapsed() >= Duration::from_secs(HEARTBEAT_SECS) {
                s.last_beat = now;
                beats.push((
                    id,
                    json!({
                        "type": "heartbeat",
                        "now_ms": s.ex.now_ms(),
                        "paused": s.ex.is_paused(),
                    }),
                ));
            }
        }
        for (id, v) in beats {
            self.send_to(id, v);
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
// Market-data loading via ccxt (memory → disk cache → Binance → synthetic)
// ---------------------------------------------------------------------------

/// Public Binance klines hosts, tried in order (api.binance.com is
/// geo-restricted in some regions with HTTP 451 — the vision mirror is not).
/// Direct REST (not the `ccxt` crate): ccxt needs >19GB RAM just to compile
/// and gets OOM-killed on ordinary machines, which would break every build.
/// This stays small, proven, and keeps the mirror fallback.
const BINANCE_HOSTS: [&str; 2] = ["https://api.binance.com", "https://data-api.binance.vision"];
/// How many klines to pull when a series is missing from cache and memory.
const DOWNLOAD_BARS: usize = 5_000;
/// Binance klines page cap.
const KLINES_PAGE: u32 = 1_000;
/// How many open-interest points to keep (coarser grid than klines).
const OI_POINTS: usize = 1_500;
/// Binance OI-history page cap.
const OI_PAGE: u32 = 500;
/// Politeness pause between download pages (Binance rate limit).
const PAGE_PAUSE: Duration = Duration::from_millis(150);

/// Step size per timeframe.
fn tf_step_ms(tf: &str) -> u64 {
    match tf {
        "1s" => 1_000,
        "1m" => 60_000,
        "5m" => 300_000,
        "15m" => 900_000,
        "1h" => 3_600_000,
        "4h" => 14_400_000,
        "1d" => 86_400_000,
        _ => 60_000,
    }
}

fn binance_file(symbol: &str, tf: &str) -> String {
    format!("{}_{}.csv", symbol.replace('/', "").to_uppercase(), tf)
}

/// Parse a timestamp cell: epoch seconds/ms (auto-detected) or ISO-8601.
fn parse_ts_ms(raw: &str) -> Option<i64> {
    let t = raw.trim();
    if let Ok(v) = t.parse::<f64>() {
        return Some(if v.abs() < 1e12 { (v * 1000.0) as i64 } else { v as i64 });
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(t) {
        return Some(dt.timestamp_millis());
    }
    let s = t.trim_end_matches('Z').trim_end_matches("+00:00");
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(ndt.and_utc().timestamp_millis());
        }
        if fmt == "%Y-%m-%d" {
            if let Ok(d) = chrono::NaiveDate::parse_from_str(s, fmt) {
                return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
            }
        }
    }
    None
}

fn load_csv(path: &PathBuf) -> Option<Vec<OHLCV>> {
    let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_path(path).ok()?;
    let mut out = Vec::new();
    for rec in rdr.records().flatten() {
        if rec.len() < 6 {
            continue;
        }
        let ms = parse_ts_ms(rec.get(0).unwrap_or(""))?;
        let num = |i: usize| rec.get(i).unwrap_or("").parse::<f64>().ok();
        let ts = DateTime::from_timestamp_millis(ms)?;
        out.push(OHLCV::new(ts, num(1)?, num(2)?, num(3)?, num(4)?, num(5)?));
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Write candles to the on-disk cache (`ts_ms,open,high,low,close,volume`).
fn save_csv(path: &Path, candles: &[OHLCV]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut wtr = csv::Writer::from_path(path).map_err(|e| e.to_string())?;
    wtr.write_record(["ts_ms", "open", "high", "low", "close", "volume"])
        .map_err(|e| e.to_string())?;
    for c in candles {
        wtr.write_record([
            c.timestamp.timestamp_millis().to_string(),
            c.open.to_string(),
            c.high.to_string(),
            c.low.to_string(),
            c.close.to_string(),
            c.volume.to_string(),
        ])
        .map_err(|e| e.to_string())?;
    }
    wtr.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// `"DOGE/USDT"` → `"DOGEUSDT"` (Binance symbol, spot klines and USDT-M
/// futures alike). `None` if not BASE/QUOTE.
fn to_binance_symbol(symbol: &str) -> Option<String> {
    let parts: Vec<&str> = symbol.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return None;
    }
    Some(format!("{}{}", parts[0], parts[1]).to_uppercase())
}

/// One Binance kline row → OHLCV: `[openTime, o, h, l, c, volume, ...]`
/// (OHLCV cells arrive as JSON strings, openTime as a number).
fn parse_kline_row(row: &Value) -> Option<OHLCV> {
    let arr = row.as_array()?;
    let ts = arr.first()?.as_i64()?;
    let num = |i: usize| -> Option<f64> {
        match arr.get(i)? {
            Value::String(s) => s.parse().ok(),
            Value::Number(n) => n.as_f64(),
            _ => None,
        }
    };
    let dt = DateTime::from_timestamp_millis(ts)?;
    Some(OHLCV::new(dt, num(1)?, num(2)?, num(3)?, num(4)?, num(5)?))
}

/// One OI-history row → `(ts_ms, USDT value)`: `{sumOpenInterestValue,
/// sumOpenInterest, timestamp}` (numbers arrive as JSON strings).
fn parse_oi_row(row: &Value) -> Option<(i64, f64)> {
    let ts = row.get("timestamp")?.as_i64()?;
    let str_num = |k: &str| -> Option<f64> {
        match row.get(k)? {
            Value::String(s) => s.parse().ok(),
            Value::Number(n) => n.as_f64(),
            _ => None,
        }
    };
    let v = str_num("sumOpenInterestValue").or_else(|| str_num("sumOpenInterest"))?;
    if v.is_finite() {
        Some((ts, v))
    } else {
        None
    }
}

/// Shared HTTP agent (15s global timeout).
fn http_agent() -> ureq::Agent {
    // ureq 3: timeout lives on the agent, not the request builder.
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into()
}

/// One klines page from `host`. Non-2xx / transport / JSON errors → `Err`.
fn fetch_klines_page(
    agent: &ureq::Agent,
    host: &str,
    sym: &str,
    tf: &str,
    start_ms: i64,
    end_ms: i64,
    limit: u32,
) -> Result<Vec<OHLCV>, String> {
    let url = format!(
        "{host}/api/v3/klines?symbol={sym}&interval={tf}\
         &startTime={start_ms}&endTime={end_ms}&limit={limit}"
    );
    let mut res = agent
        .get(&url)
        .header("User-Agent", "simccxt/0.1")
        .call()
        .map_err(|e| format!("{host}: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("{host}: HTTP {}", res.status()));
    }
    let body = res.body_mut().read_to_string().map_err(|e| format!("{host}: {e}"))?;
    let rows: Vec<Value> = serde_json::from_str(&body).map_err(|e| format!("{host}: {e}"))?;
    Ok(rows.iter().filter_map(parse_kline_row).collect())
}

/// One open-interest-history page from the USDT-M futures API
/// (no mirror host exists for futures — a 451 here means "no OI").
fn fetch_oi_page(
    agent: &ureq::Agent,
    sym: &str,
    period: &str,
    start_ms: i64,
    end_ms: i64,
    limit: u32,
) -> Result<Vec<(i64, f64)>, String> {
    let url = format!(
        "https://fapi.binance.com/futures/data/openInterestHist\
         ?symbol={sym}&period={period}\
         &startTime={start_ms}&endTime={end_ms}&limit={limit}"
    );
    let mut res = agent
        .get(&url)
        .header("User-Agent", "simccxt/0.1")
        .call()
        .map_err(|e| format!("fapi: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("fapi: HTTP {}", res.status()));
    }
    let body = res.body_mut().read_to_string().map_err(|e| format!("fapi: {e}"))?;
    let rows: Vec<Value> = serde_json::from_str(&body).map_err(|e| format!("fapi: {e}"))?;
    Ok(rows.iter().filter_map(parse_oi_row).collect())
}

/// Download the most recent `bars` klines for `symbol@tf` from Binance
/// (paginated, no API key). Tries both hosts per page; short page = end of
/// available history. Returns bars sorted ascending by open time.
fn download_series(symbol: &str, tf: &str, bars: usize) -> Result<Vec<OHLCV>, String> {
    let sym = to_binance_symbol(symbol).ok_or_else(|| format!("bad symbol {symbol}"))?;
    // Only intervals Binance knows — never invent a grid for unknown tf.
    let step: i64 = match tf {
        "1s" | "1m" | "5m" | "15m" | "1h" | "4h" | "1d" => tf_step_ms(tf) as i64,
        _ => return Err(format!("unsupported timeframe {tf}")),
    };
    let end_ms = Utc::now().timestamp_millis();
    let start_ms = end_ms - (bars as i64) * step;
    let mut out: Vec<OHLCV> = Vec::with_capacity(bars);
    let mut cursor = start_ms;
    let mut last_err: Option<String> = None;
    let agent = http_agent();

    while cursor <= end_ms && out.len() < bars {
        let mut page: Option<Vec<OHLCV>> = None;
        for host in BINANCE_HOSTS {
            match fetch_klines_page(&agent, host, &sym, tf, cursor, end_ms, KLINES_PAGE) {
                Ok(rows) => {
                    page = Some(rows);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let Some(rows) = page else { break };
        if rows.is_empty() {
            break;
        }
        let short = rows.len() < KLINES_PAGE as usize;
        let last_open = rows
            .last()
            .map(|c| c.timestamp.timestamp_millis())
            .unwrap_or(cursor);
        out.extend(rows);
        if short || last_open >= end_ms || last_open < cursor {
            break;
        }
        cursor = last_open + step;
        std::thread::sleep(PAGE_PAUSE);
    }

    if out.is_empty() {
        return Err(last_err.unwrap_or_else(|| "no klines returned".into()));
    }
    out.sort_by_key(|c| c.timestamp.timestamp_millis());
    out.dedup_by_key(|c| c.timestamp.timestamp_millis());
    Ok(out)
}

/// OI history grid per chart timeframe (Binance panics on 1m — map to 5m).
fn oi_period(tf: &str) -> Option<&'static str> {
    match tf {
        "1s" | "1m" | "5m" => Some("5m"),
        "15m" => Some("15m"),
        "1h" => Some("1h"),
        "4h" => Some("4h"),
        "1d" => Some("1d"),
        _ => None,
    }
}

fn oi_file(symbol: &str, period: &str) -> String {
    format!("{}_oi_{}.csv", symbol.replace('/', "").to_uppercase(), period)
}

/// Write `[(ts_ms, value)]` cache rows (`ts_ms,oi`).
fn save_pairs_csv(path: &Path, rows: &[(i64, f64)]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut wtr = csv::Writer::from_path(path).map_err(|e| e.to_string())?;
    wtr.write_record(["ts_ms", "oi"]).map_err(|e| e.to_string())?;
    for (ts, v) in rows {
        wtr.write_record([ts.to_string(), v.to_string()])
            .map_err(|e| e.to_string())?;
    }
    wtr.flush().map_err(|e| e.to_string())?;
    Ok(())
}

fn load_pairs_csv(path: &PathBuf) -> Option<Vec<(i64, f64)>> {
    let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_path(path).ok()?;
    let mut out = Vec::new();
    for rec in rdr.records().flatten() {
        if rec.len() < 2 {
            continue;
        }
        let ms = parse_ts_ms(rec.get(0).unwrap_or(""))?;
        let v = rec.get(1).unwrap_or("").parse::<f64>().ok()?;
        out.push((ms, v));
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Download open-interest history (USDT value) for the USDT-M perp matching
/// `symbol@tf`, paginated. Unlike klines, the OI endpoint returns the LATEST
/// `limit` rows of the window — so pages walk BACKWARD via `endTime`.
/// No mirror exists for futures: a 451/geo error here means "no OI"
/// (klines still work via the mirror).
fn download_oi(symbol: &str, tf: &str, points: usize) -> Result<Vec<(i64, f64)>, String> {
    let period = oi_period(tf).ok_or_else(|| format!("no OI grid for {tf}"))?;
    let sym = to_binance_symbol(symbol).ok_or_else(|| format!("bad symbol {symbol}"))?;
    let step = tf_step_ms(period) as i64;
    let end_ms = Utc::now().timestamp_millis();
    let start_ms = end_ms - (points as i64) * step;
    let mut out: Vec<(i64, f64)> = Vec::with_capacity(points);
    let mut end = end_ms;
    let mut last_err: Option<String> = None;
    let agent = http_agent();

    while end > start_ms && out.len() < points {
        match fetch_oi_page(&agent, &sym, period, start_ms, end, OI_PAGE) {
            Ok(rows) => {
                if rows.is_empty() {
                    break;
                }
                let short = rows.len() < OI_PAGE as usize;
                let mut first_ts = end;
                for (ts, v) in rows {
                    if ts >= start_ms && ts <= end {
                        out.push((ts, v));
                        first_ts = first_ts.min(ts);
                    }
                }
                if short || first_ts <= start_ms || first_ts >= end {
                    break;
                }
                end = first_ts - 1;
                std::thread::sleep(PAGE_PAUSE);
            }
            Err(e) => {
                last_err = Some(e);
                break;
            }
        }
    }

    if out.is_empty() {
        return Err(last_err.unwrap_or_else(|| "no open interest returned".into()));
    }
    out.sort_by_key(|r| r.0);
    out.dedup_by_key(|r| r.0);
    Ok(out)
}

/// Deterministic synthetic random walk (xorshift64, fixed seed) — explicit
/// fallback when no cache file exists. Never silent: the source is logged.
fn synthetic(symbol: &str, tf: &str, n: usize) -> Vec<OHLCV> {
    let step = tf_step_ms(tf) as i64;
    let start = 1_704_067_200_000i64; // 2024-01-01T00:00:00Z
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15 ^ (symbol.len() as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (u64::MAX >> 11) as f64
    };
    let mut price = 50_000.0;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let drift = (next() - 0.495) * price * 0.002;
        let o = price;
        let c = (price + drift).max(1.0);
        let h = o.max(c) * (1.0 + next() * 0.0005);
        let l = o.min(c) * (1.0 - next() * 0.0005);
        let v = 1.0 + next() * 9.0;
        let ts = DateTime::from_timestamp_millis(start + i as i64 * step).unwrap();
        out.push(OHLCV::new(ts, o, h, l, c, v));
        price = c;
    }
    out
}

/// Make sure open interest for `symbol@timeframe` is loaded into the
/// session: disk cache → ccxt download (saved to cache) → empty + log.
/// OI is chart context, never replayed: no synthetic fallback (an invented
/// OI curve would be worse than none).
fn ensure_oi(
    oi: &mut Vec<(i64, f64)>,
    oi_period_out: &mut String,
    symbol: &str,
    tf: &str,
    data_dir: &Path,
    allow_net: bool,
) {
    oi.clear();
    oi_period_out.clear();
    let Some(period) = oi_period(tf) else { return };
    let path = data_dir.join(oi_file(symbol, period));
    if let Some(rows) = load_pairs_csv(&path) {
        log!("[engine] {symbol} OI@{period}: {} pts from cache ({})", rows.len(), path.display());
        *oi = rows;
        *oi_period_out = period.to_string();
        return;
    }
    if allow_net {
        log!("[engine] {symbol} OI@{period}: no cache, downloading…");
        match download_oi(symbol, tf, OI_POINTS) {
            Ok(rows) => {
                if let Err(e) = save_pairs_csv(&path, &rows) {
                    log!("[engine] {symbol} OI@{period}: cache write failed: {e}");
                }
                log!(
                    "[engine] {symbol} OI@{period}: {} pts from binance (cached {})",
                    rows.len(),
                    path.display()
                );
                *oi = rows;
                *oi_period_out = period.to_string();
            }
            Err(e) => log!("[engine] {symbol} OI@{period}: download failed: {e}"),
        }
    }
}

/// Make sure `symbol@timeframe` is loaded. Order: already in memory →
/// disk cache → Binance download (saved to cache) → synthetic fallback.
/// Network is only attempted when `allow_net` (off in unit tests, and with
/// `--offline`); every source is logged — synthetic is never silent.
fn ensure_series(
    ex: &mut Exchange,
    symbol: &str,
    tf: &str,
    data_dir: &Path,
    allow_net: bool,
) {
    if !ex.fetch_ohlcv(symbol, tf, None, Some(1)).is_empty() {
        return;
    }
    let path = data_dir.join(binance_file(symbol, tf));
    if let Some(candles) = load_csv(&path) {
        let n = candles.len();
        if let Ok(sym) = Symbol::parse(symbol) {
            ex.add_market(&sym, MarketKind::Spot);
        }
        ex.load_candles(symbol, tf, candles);
        log!("[engine] {symbol} {tf}: {n} bars from cache ({})", path.display());
        return;
    }
    if allow_net {
        log!("[engine] {symbol} {tf}: no cache, downloading {DOWNLOAD_BARS} bars from binance…");
        match download_series(symbol, tf, DOWNLOAD_BARS) {
            Ok(candles) => {
                let n = candles.len();
                if let Err(e) = save_csv(&path, &candles) {
                    log!("[engine] {symbol} {tf}: cache write failed: {e}");
                }
                if let Ok(sym) = Symbol::parse(symbol) {
                    ex.add_market(&sym, MarketKind::Spot);
                }
                ex.load_candles(symbol, tf, candles);
                log!(
                    "[engine] {symbol} {tf}: {n} bars from binance (cached {})",
                    path.display()
                );
                return;
            }
            Err(e) => log!("[engine] {symbol} {tf}: binance download failed: {e}"),
        }
    }
    let candles = synthetic(symbol, tf, 2000);
    if let Ok(sym) = Symbol::parse(symbol) {
        ex.add_market(&sym, MarketKind::Spot);
    }
    ex.load_candles(symbol, tf, candles);
    log!("[engine] {symbol} {tf}: 2000 synthetic bars (no cache at {})", path.display());
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
    // Tell the client its id so it can filter directed messages.
    let hello = msg(json!({ "type": "welcome", "client_id": client_id }));
    if sink.send(tokio_tungstenite::tungstenite::Message::Text(hello.into())).await.is_err() {
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
                    let err = msg(json!({
                        "type": "error", "to": client_id,
                        "message": format!("bad command: {e}"),
                    }));
                    let _ = out.send(err);
                }
            }
        }
    }
    .boxed();
    // Broadcast -> this socket (skip other clients' directed messages).
    // A slow client must NEVER kill its connection: on lag we ask it to
    // resync (fresh snapshot) and keep streaming.
    let mut write = async move {
        loop {
            let s = match rx.recv().await {
                Ok(s) => s,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    log!("[ws] #{client_id} lagged, skipped {skipped} msgs -> resync");
                    let resync = msg(json!({ "type": "resync", "to": client_id }));
                    let m = tokio_tungstenite::tungstenite::Message::Text(resync.into());
                    if sink.send(m).await.is_err() {
                        break;
                    }
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                if let Some(to) = v.get("to").and_then(|t| t.as_u64()) {
                    if to != client_id {
                        continue;
                    }
                }
            }
            let m = tokio_tungstenite::tungstenite::Message::Text(s.into());
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
    // Destroy the session: its account/clock die with the connection.
    let _ = jobs.send(EngineJob::Disconnect { client_id });
}

#[derive(Debug)]
struct Args {
    port: u16,
    symbol: String,
    timeframe: String,
    data_dir: PathBuf,
    /// `--offline`: never call Binance; missing series fall back to synthetic.
    offline: bool,
}

fn parse_args() -> Args {
    let mut port = 9001u16;
    let mut symbol = "BTC/USDT".to_string();
    let mut timeframe = "1m".to_string();
    let mut data_dir = PathBuf::from("data/ohlcv");
    let mut offline = false;
    let mut it = std::env::args().skip(1).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = it.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            "--symbol" => symbol = it.next().unwrap_or(symbol),
            "--timeframe" | "--tf" => timeframe = it.next().unwrap_or(timeframe),
            "--data-dir" => data_dir = it.next().map(PathBuf::from).unwrap_or(data_dir),
            "--offline" => offline = true,
            "--help" | "-h" => {
                println!("simccxt-server [--port 9001] [--symbol BTC/USDT] [--timeframe 1m] [--data-dir data/ohlcv] [--offline]");
                std::process::exit(0);
            }
            _ => log!("[args] ignoring unknown flag {a}"),
        }
    }
    Args { port, symbol, timeframe, data_dir, offline }
}

fn engine_main(
    rx: mpsc::Receiver<EngineJob>,
    out: broadcast::Sender<String>,
    args: Args,
) {
    log!(
        "[engine] cli defaults: {} {} (every session subscribes its own series, net={})",
        args.symbol,
        args.timeframe,
        if args.offline { "off" } else { "binance" }
    );
    let mut engine = Engine::new(out).with_net(!args.offline);

    loop {
        match rx.recv_timeout(engine.next_timeout()) {
            Ok(job) => {
                engine.handle_job(job, &args.data_dir);
                engine.auto_tick_all();
                engine.heartbeat_all();
            }
            Err(RecvTimeoutError::Timeout) => {
                engine.auto_tick_all();
                engine.heartbeat_all();
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
    std::thread::Builder::new()
        .name("simccxt-engine".into())
        .spawn(move || engine_main(jobs_rx, out_for_engine, args))
        .expect("spawn engine thread");
    let listener = TcpListener::bind(("127.0.0.1", port)).await.expect("bind ws port");
    log!("[ws] simccxt-server listening on ws://127.0.0.1:{port}/");
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
        tokio::spawn(handle_conn(stream, addr, id, jobs_tx.clone(), out_tx.clone()));
    }
}

// ---------------------------------------------------------------------------
// Unit tests (protocol + incremental diff; run with `cargo test -p simccxt-server`)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_cmd_place_order_parses() {
        let cmd: ClientCmd = serde_json::from_str(
            r#"{"type":"place_order","symbol":"BTC/USDT","side":"buy",
                "order_type":"market","quantity":0.1}"#,
        )
        .unwrap();
        match cmd {
            ClientCmd::PlaceOrder { symbol, side, quantity, price, .. } => {
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
    fn ts_parser_handles_epochs_and_iso() {
        assert_eq!(parse_ts_ms("1704067200000"), Some(1_704_067_200_000));
        assert_eq!(parse_ts_ms("1704067200"), Some(1_704_067_200_000));
        assert_eq!(parse_ts_ms("2024-01-01T00:00:00Z"), Some(1_704_067_200_000));
        assert_eq!(parse_ts_ms("not-a-date"), None);
    }

    #[test]
    fn tf_steps_known() {
        assert_eq!(tf_step_ms("1m"), 60_000);
        assert_eq!(tf_step_ms("1h"), 3_600_000);
    }

    #[test]
    fn synthetic_is_deterministic() {
        let a = synthetic("BTC/USDT", "1m", 50);
        let b = synthetic("BTC/USDT", "1m", 50);
        assert_eq!(a.len(), 50);
        assert_eq!(
            a.iter().map(|c| c.close).collect::<Vec<_>>(),
            b.iter().map(|c| c.close).collect::<Vec<_>>()
        );
    }

    #[test]
    fn binance_symbol_from_pair() {
        assert_eq!(to_binance_symbol("DOGE/USDT").as_deref(), Some("DOGEUSDT"));
        assert_eq!(to_binance_symbol("btc/usdt").as_deref(), Some("BTCUSDT"));
        assert_eq!(to_binance_symbol("BTCUSDT"), None);
        assert_eq!(to_binance_symbol("/USDT"), None);
    }

    #[test]
    fn kline_row_parses_ohlcv() {
        let row = serde_json::json!([
            1789257600000i64, "77278.73", "77282.04", "77266.01", "77282.04",
            "1.71117", 1789257659999i64, "132440.5", 42, "0", "0", "0"
        ]);
        let c = parse_kline_row(&row).expect("parses");
        assert_eq!(c.timestamp.timestamp_millis(), 1_789_257_600_000);
        assert_eq!(c.open, 77278.73);
        assert_eq!(c.close, 77282.04);
        assert_eq!(c.volume, 1.71117);
    }

    #[test]
    fn oi_row_parses_value() {
        let row = serde_json::json!({
            "symbol": "BTCUSDT",
            "sumOpenInterest": "12345.6",
            "sumOpenInterestValue": "987654321.5",
            "timestamp": 1789257600000i64,
        });
        assert_eq!(parse_oi_row(&row), Some((1_789_257_600_000, 987654321.5)));
        // Falls back to the base-asset amount when value is missing.
        let row2 = serde_json::json!({
            "symbol": "BTCUSDT", "sumOpenInterest": "12345.6", "timestamp": 1i64,
        });
        assert_eq!(parse_oi_row(&row2), Some((1, 12345.6)));
        assert_eq!(parse_oi_row(&serde_json::json!({"a": 1})), None);
    }

    #[test]
    fn save_and_load_csv_roundtrip() {
        let dir = std::env::temp_dir().join(format!("simccxt-csv-{}", std::process::id()));
        let path = dir.join("TESTUSDT_1m.csv");
        let candles = vec![
            OHLCV::new(
                DateTime::from_timestamp_millis(1_789_257_600_000).unwrap(),
                1.0, 2.0, 0.5, 1.5, 10.0,
            ),
            OHLCV::new(
                DateTime::from_timestamp_millis(1_789_257_660_000).unwrap(),
                1.5, 3.0, 1.0, 2.5, 20.0,
            ),
        ];
        save_csv(&path, &candles).expect("save");
        let back = load_csv(&path).expect("load");
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].close, 2.5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_rejects_unknown_timeframe() {
        // No network needed: validation fails before any request.
        let err = download_series("BTC/USDT", "7m", 10).unwrap_err();
        assert!(err.contains("unsupported timeframe"), "{err}");
        assert!(download_oi("BTC/USDT", "7m", 10).is_err());
    }

    #[test]
    fn oi_grid_mapping() {
        // Binance has no 1m OI: we map to 5m instead.
        assert_eq!(oi_period("1m"), Some("5m"));
        assert_eq!(oi_period("1s"), Some("5m"));
        assert_eq!(oi_period("15m"), Some("15m"));
        assert_eq!(oi_period("1h"), Some("1h"));
        assert_eq!(oi_period("7m"), None);
    }

    #[test]
    fn oi_cache_roundtrip() {
        let dir = std::env::temp_dir().join(format!("simccxt-oi-{}", std::process::id()));
        let path = dir.join("BTCUSDT_oi_5m.csv");
        let rows = vec![(1_789_257_600_000i64, 1_234_567.5), (1_789_257_900_000i64, 1_300_000.0)];
        save_pairs_csv(&path, &rows).expect("save");
        let back = load_pairs_csv(&path).expect("load");
        assert_eq!(back, rows);
        let _ = std::fs::remove_dir_all(&dir);
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
            request_id: None,
        }
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
        ClientCmd::Login { name: name.into(), request_id: None }
    }

    fn challenge_cmd(join: bool, risk: Option<f64>) -> ClientCmd {
        ClientCmd::Challenge { join, risk_usdt: risk, request_id: None }
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

    fn test_data_dir() -> &'static Path {
        // No cache files here: every series is deterministic synthetic data.
        Path::new("/nonexistent-simccxt-session-test")
    }

    #[test]
    fn sessions_have_independent_symbols_and_clocks() {
        let (tx, mut rx) = broadcast::channel::<String>(512);
        let mut eng = Engine::new(tx);
        let data = test_data_dir();

        eng.handle_command(1, subscribe_cmd("BTC/USDT", "1m"), data);
        eng.handle_command(2, subscribe_cmd("ETH/USDT", "5m"), data);

        // Each session keeps its own series.
        assert_eq!(eng.sessions[&1].symbol, "BTC/USDT");
        assert_eq!(eng.sessions[&1].timeframe, "1m");
        assert_eq!(eng.sessions[&2].symbol, "ETH/USDT");
        assert_eq!(eng.sessions[&2].timeframe, "5m");

        // Pausing session 1 must not pause session 2 (subscribe resumes).
        eng.handle_command(1, replay_cmd("pause"), data);
        assert!(eng.sessions[&1].ex.is_paused());
        assert!(!eng.sessions[&2].ex.is_paused());

        // Every message on the bus is directed at exactly one session and
        // snapshots carry the right symbol for their recipient.
        let mut snaps = 0;
        while let Ok(raw) = rx.try_recv() {
            let v: Value = serde_json::from_str(&raw).expect("valid json");
            let to = v
                .get("to")
                .and_then(|t| t.as_u64())
                .expect("session message must carry `to`");
            assert!(to == 1 || to == 2, "unknown recipient {to}");
            if v["type"] == "snapshot" {
                snaps += 1;
                let want = if to == 1 { "BTC/USDT" } else { "ETH/USDT" };
                assert_eq!(v["symbol"], want, "snapshot routed to the wrong session");
            }
        }
        assert_eq!(snaps, 2, "one snapshot per subscriber");
    }

    #[test]
    fn orders_do_not_leak_between_sessions() {
        let (tx, _rx) = broadcast::channel::<String>(64);
        let mut eng = Engine::new(tx);
        let data = test_data_dir();

        for id in [1u64, 2u64] {
            eng.handle_command(id, subscribe_cmd("BTC/USDT", "1m"), data);
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
                data,
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
                time_in_force: "GTC".into(),
                post_only: false,
                reduce_only: false,
                request_id: None,
            },
            data,
        );

        let a1 = eng.sessions[&1].ex.account_snapshot();
        let a2 = eng.sessions[&2].ex.account_snapshot();
        assert!(
            !a1["positions"].as_array().map(|p| p.is_empty()).unwrap_or(true),
            "session 1 should hold a position after the market buy"
        );
        assert!(
            a2["positions"].as_array().map(|p| p.is_empty()).unwrap_or(false),
            "session 2 must not see session 1's position"
        );
        let equity2 = a2["equity"].as_f64().expect("equity");
        assert!(
            (equity2 - 1_000_000.0).abs() < 1e-6,
            "session 2 equity must stay at the initial deposit, got {equity2}"
        );
        assert_eq!(eng.sessions[&2].ex.fetch_fills(None, None).len(), 0);
    }

    #[test]
    fn disconnect_destroys_the_session() {
        let (tx, _rx) = broadcast::channel::<String>(16);
        let mut eng = Engine::new(tx);
        let data = test_data_dir();

        eng.handle_command(7, subscribe_cmd("BTC/USDT", "1m"), data);
        assert!(eng.sessions.contains_key(&7), "first subscribe creates the session");

        eng.handle_job(EngineJob::Disconnect { client_id: 7 }, data);
        assert!(!eng.sessions.contains_key(&7), "disconnect must free the session");
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
        assert_eq!(clean_nick(&"y".repeat(24)).map(|s| s.chars().count()), Some(24));
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
        let data = test_data_dir();

        // Empty login keeps the server-assigned random nick, still acked.
        eng.handle_command(1, login_cmd(""), data);
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

        eng.handle_command(1, login_cmd("Тестер"), data);
        assert_eq!(eng.sessions[&1].nick, "Тестер");
        eng.handle_command(1, login_cmd(""), data);
        assert_eq!(eng.sessions[&1].nick, "Тестер", "empty login keeps the nick");
        eng.handle_command(1, login_cmd(&"z".repeat(30)), data);
        assert_eq!(eng.sessions[&1].nick, "Тестер", "oversize nick rejected");
    }

    #[test]
    fn challenge_join_requires_positive_risk() {
        let (tx, mut rx) = broadcast::channel::<String>(64);
        let mut eng = Engine::new(tx);
        let data = test_data_dir();

        eng.handle_command(1, login_cmd("A"), data);
        eng.handle_command(1, challenge_cmd(true, None), data);
        assert!(!eng.sessions[&1].in_challenge, "no risk → no join");
        eng.handle_command(1, challenge_cmd(true, Some(0.0)), data);
        assert!(!eng.sessions[&1].in_challenge, "zero risk → no join");
        // Only the login board so far: failed joins push nothing.
        assert_eq!(board_messages(&mut rx).len(), 1);

        eng.handle_command(1, challenge_cmd(true, Some(10.0)), data);
        assert!(eng.sessions[&1].in_challenge);
        assert_eq!(eng.sessions[&1].risk_usdt, 10.0);
        let boards = board_messages(&mut rx);
        assert_eq!(boards.len(), 1);
        assert!(boards[0].get("to").is_none(), "board goes to everyone");
        let rows = boards[0]["board"].as_array().expect("board array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["nick"], "A");
        assert_eq!(rows[0]["risk"], 10.0);

        eng.handle_command(1, challenge_cmd(false, None), data);
        assert!(!eng.sessions[&1].in_challenge);
        let boards = board_messages(&mut rx);
        assert_eq!(boards.len(), 1);
        assert!(boards[0]["board"].as_array().unwrap().is_empty());
    }

    #[test]
    fn board_orders_by_r_and_drops_on_disconnect() {
        let (tx, mut rx) = broadcast::channel::<String>(512);
        let mut eng = Engine::new(tx);
        let data = test_data_dir();

        for (id, nick) in [(1u64, "Первый"), (2u64, "Второй")] {
            eng.handle_command(id, login_cmd(nick), data);
            eng.handle_command(id, subscribe_cmd("BTC/USDT", "1m"), data);
            eng.handle_command(id, challenge_cmd(true, Some(10.0)), data);
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
            data,
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
                time_in_force: "GTC".into(),
                post_only: false,
                reduce_only: false,
                request_id: None,
            },
            data,
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

        eng.handle_job(EngineJob::Disconnect { client_id: 1 }, data);
        let boards = board_messages(&mut rx);
        let last = boards.last().expect("board pushed after disconnect");
        let rows = last["board"].as_array().expect("board array");
        assert_eq!(rows.len(), 1, "leaver drops off the board");
        assert_eq!(rows[0]["nick"], "Второй");
    }
}
