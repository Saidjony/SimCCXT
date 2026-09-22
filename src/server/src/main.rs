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
//! - afterwards the server only broadcasts `candle` (new/updated bar),
//!   `market_update`, `account_update`, `trade` and `replay_state`.

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
//   {"type":"ping"}
//
// Rust -> frontend:
//   {"type":"welcome","client_id":7}
//   {"type":"snapshot","to":7,"symbol":..,"timeframe":..,"candles":[[ts,o,h,l,c,v]..],
//    "account":{..},"replay":{..}}
//   {"type":"candle","symbol":..,"timeframe":..,"candle":[..],"is_new":true}
//   {"type":"market_update","symbol":..,"last":..,"timestamp":..}
//   {"type":"account_update","account":{..}}
//   {"type":"order_update","order":{..}}
//   {"type":"trade","fill":{..}}
//   {"type":"replay_state",.. | "replay":{..}}
//   {"type":"heartbeat","now_ms":..,"paused":..}   (every 10 s when quiet)
//   {"type":"resync","to":7}                       (you lagged: re-subscribe)
//   {"type":"error","to":7,"message":"..","request_id":".."}
//   {"type":"pong"}
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

/// One unit of work for the engine thread.
struct EngineJob {
    client_id: u64,
    cmd: ClientCmd,
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

fn replay_state_json(ex: &Exchange, symbol: &str, tf: &str, speed: f64, step_ms: u64) -> Value {
    json!({
        "type": "replay_state",
        "paused": ex.is_paused(),
        "now_ms": ex.now_ms(),
        "speed": speed,
        "step_ms": step_ms,
        "done": ex.replay_done(symbol, tf),
        "fed": ex.fetch_ohlcv_live(symbol, tf).len(),
        "total": ex.fetch_ohlcv(symbol, tf, None, None).len(),
    })
}

/// Bars in `live` that the client has not seen yet (client has seen `fed`).
/// Pure helper so it can be unit-tested: returns the slice of new rows.
fn new_bars(live: &[Vec<f64>], fed: usize) -> &[Vec<f64>] {
    if fed >= live.len() { &[] } else { &live[fed..] }
}

struct Engine {
    ex: Exchange,
    symbol: String,
    timeframe: String,
    speed: f64,   // ticks per second while playing
    step_ms: u64, // mirrored from Exchange::set_step_ms (no getter there)
    fed: usize,   // candles already pushed to clients
    fills_seen: usize,
    last_beat: std::time::Instant, // last heartbeat broadcast
    out: broadcast::Sender<String>,
}

/// Heartbeat period: even a paused replay emits traffic so the frontend can
/// tell "idle" apart from "disconnected".
const HEARTBEAT_SECS: u64 = 10;

impl Engine {
    fn broadcast(&self, v: Value) {
        let _ = self.out.send(msg(v));
    }

    fn directed(&self, to: u64, v: Value) {
        // Directed messages carry `to`; clients ignore messages for others.
        let mut v = v;
        v["to"] = json!(to);
        let _ = self.out.send(msg(v));
    }

    fn account_event(&self) -> Value {
        json!({ "type": "account_update", "account": self.ex.account_snapshot() })
    }

    /// Emit a tiny heartbeat when the engine has been quiet: lets the
    /// frontend distinguish a paused replay from a dead connection.
    fn maybe_heartbeat(&mut self) {
        if self.last_beat.elapsed() >= Duration::from_secs(HEARTBEAT_SECS) {
            self.last_beat = std::time::Instant::now();
            self.broadcast(json!({
                "type": "heartbeat",
                "now_ms": self.ex.now_ms(),
                "paused": self.ex.is_paused(),
            }));
        }
    }

    /// Push newly fed candles + market/account/replay updates after a step.
    fn push_delta(&mut self) {
        let live = self.ex.fetch_ohlcv_live(&self.symbol, &self.timeframe);
        for bar in new_bars(&live, self.fed) {
            let is_new = true;
            self.broadcast(json!({
                "type": "candle",
                "symbol": self.symbol,
                "timeframe": self.timeframe,
                "candle": bar,
                "is_new": is_new,
            }));
        }
        self.fed = live.len();
        let last = live.last().map(|r| r[4]).unwrap_or(0.0);
        self.broadcast(json!({
            "type": "market_update",
            "symbol": self.symbol,
            "last": last,
            "timestamp": self.ex.now_ms(),
        }));
        // New fills produced by the step (e.g. triggered stop/TP).
        let fills = self.ex.fetch_fills(None, None);
        if fills.len() > self.fills_seen {
            // fetch_fills is newest-first; take the unknown prefix and
            // re-emit oldest-first so the UI appends in order.
            let mut fresh: Vec<&Value> = fills.iter().take(fills.len() - self.fills_seen).collect();
            fresh.reverse();
            for f in fresh {
                self.broadcast(json!({ "type": "trade", "fill": f }));
            }
            self.fills_seen = fills.len();
        }
        self.broadcast(self.account_event());
        let replay = replay_state_json(&self.ex, &self.symbol, &self.timeframe, self.speed, self.step_ms);
        self.broadcast(replay);
    }

    fn send_snapshot(&self, to: u64, request_id: Option<String>) {
        // Only bars <= now: the chart reveals one new bar per tick instead of
        // the whole future at once. Later steps arrive as `candle` deltas.
        let mut rows = self.ex.fetch_ohlcv_live(&self.symbol, &self.timeframe);
        if rows.len() > SNAPSHOT_CAP {
            rows = rows[rows.len() - SNAPSHOT_CAP..].to_vec();
        }
        self.directed(
            to,
            json!({
                "type": "snapshot",
                "symbol": self.symbol,
                "timeframe": self.timeframe,
                "candles": rows,
                "account": self.ex.account_snapshot(),
                "replay": replay_state_json(&self.ex, &self.symbol, &self.timeframe, self.speed, self.step_ms),
                "request_id": request_id,
            }),
        );
    }

    fn send_error(&self, to: u64, message: String, request_id: Option<String>) {
        self.directed(to, json!({ "type": "error", "message": message, "request_id": request_id }));
    }

    /// Switch the replay series. Returns a human-readable source note.
    fn use_series(&mut self, symbol: String, timeframe: String, data_dir: &Path) -> String {
        self.symbol = symbol;
        self.timeframe = timeframe;
        self.step_ms = tf_step_ms(&self.timeframe);
        self.ex.set_step_ms(self.step_ms);
        ensure_series(&mut self.ex, &self.symbol, &self.timeframe, data_dir);
        let rows = self.ex.fetch_ohlcv(&self.symbol, &self.timeframe, None, None);
        if let Some(first) = rows.first() {
            let ts = DateTime::from_timestamp_millis(first[0] as i64).unwrap_or_else(Utc::now);
            self.ex.reset(ts);
        }
        self.ex.resume();
        self.fed = 0;
        self.fills_seen = self.ex.fetch_fills(None, None).len();
        format!("{} {} ({} bars)", self.symbol, self.timeframe, rows.len())
    }

    fn handle(&mut self, job: EngineJob, data_dir: &Path) {
        let to = job.client_id;
        match job.cmd {
            ClientCmd::Ping => {
                self.directed(to, json!({ "type": "pong" }));
            }
            ClientCmd::Subscribe { symbol, timeframe, request_id } => {
                if symbol != self.symbol || timeframe != self.timeframe {
                    let note = self.use_series(symbol, timeframe, data_dir);
                    log!("[engine] subscribe -> {note}");
                }
                // (Re)sync the fed cursor to "everything live so far", then
                // send the full snapshot; later steps only push deltas.
                self.fed = self.ex.fetch_ohlcv_live(&self.symbol, &self.timeframe).len();
                self.send_snapshot(to, request_id);
            }
            ClientCmd::PlaceOrder {
                symbol, side, order_type, quantity, price, stop_price,
                time_in_force, post_only, reduce_only, request_id,
            } => {
                let mut p = CreateOrderParams::new();
                p.stop_price = stop_price;
                p.reduce_only = reduce_only;
                p.post_only = post_only;
                if !time_in_force.is_empty() {
                    if let Some(tif) = parse_tif(&time_in_force) {
                        p.time_in_force = tif;
                    }
                }
                match self.ex.create_order(&symbol, &order_type, &side, quantity, price, p) {
                    Ok(order) => {
                        self.directed(to, json!({
                            "type": "order_update", "order": order, "request_id": request_id }));
                        let fills = self.ex.fetch_fills(None, None);
                        if fills.len() > self.fills_seen {
                            let mut fresh: Vec<&Value> =
                                fills.iter().take(fills.len() - self.fills_seen).collect();
                            fresh.reverse();
                            for f in fresh {
                                self.broadcast(json!({ "type": "trade", "fill": f }));
                            }
                            self.fills_seen = fills.len();
                        }
                        self.broadcast(self.account_event());
                    }
                    Err(e) => self.send_error(to, format!("place_order: {e}"), request_id),
                }
            }
            ClientCmd::CancelOrder { order_id, symbol, request_id } => {
                match self.ex.cancel_order(&order_id, &symbol) {
                    Ok(order) => {
                        self.directed(to, json!({
                            "type": "order_update", "order": order, "request_id": request_id }));
                        self.broadcast(self.account_event());
                    }
                    Err(e) => self.send_error(to, format!("cancel_order: {e}"), request_id),
                }
            }
            ClientCmd::ClosePosition { symbol, quantity, request_id } => {
                let (held, _) = self.ex.spot_basis(&symbol);
                let q = quantity.unwrap_or(held);
                if q <= 0.0 {
                    self.send_error(to, "nothing to close".into(), request_id);
                } else {
                    match self.ex.create_order(
                        &symbol, "market", "sell", q, None, CreateOrderParams::new(),
                    ) {
                        Ok(order) => {
                            self.directed(to, json!({
                                "type": "order_update", "order": order,
                                "request_id": request_id }));
                            self.broadcast(self.account_event());
                        }
                        Err(e) => self.send_error(to, format!("close_position: {e}"), request_id),
                    }
                }
            }
            ClientCmd::Replay { action, steps, speed, step_ms, timestamp_ms, request_id } => {
                match action.as_str() {
                    "play" => self.ex.resume(),
                    "pause" => self.ex.pause(),
                    "step" => {
                        // Advance exactly n bars even while paused (pause restored).
                        let was_paused = self.ex.is_paused();
                        if was_paused {
                            self.ex.resume();
                        }
                        if let Some(ms) = step_ms {
                            self.ex.set_step_ms(ms);
                            self.step_ms = ms;
                        }
                        self.ex.step_n(steps.unwrap_or(1).max(1));
                        if was_paused {
                            self.ex.pause();
                        }
                        self.push_delta();
                    }
                    "reset" => {
                        let rows = self.ex.fetch_ohlcv(&self.symbol, &self.timeframe, None, None);
                        let ts_ms = timestamp_ms.unwrap_or(
                            rows.first().map(|r| r[0] as i64).unwrap_or_else(|| self.ex.now_ms()),
                        );
                        if let Some(ts) = DateTime::from_timestamp_millis(ts_ms) {
                            self.ex.reset(ts);
                        }
                        // Sync the cursor to the new clock (the client follows
                        // up with subscribe -> full snapshot). Broadcasting
                        // every live bar as deltas here would be a burst of
                        // thousands of messages; deltas resume from here.
                        self.fed = self.ex.fetch_ohlcv_live(&self.symbol, &self.timeframe).len();
                        self.broadcast(self.account_event());
                        self.broadcast(replay_state_json(
                            &self.ex, &self.symbol, &self.timeframe, self.speed, self.step_ms,
                        ));
                    }
                    "set_speed" => {
                        if let Some(s) = speed {
                            if s > 0.0 && s <= 100.0 {
                                self.speed = s;
                            }
                        }
                    }
                    "set_step" => {
                        if let Some(ms) = step_ms {
                            let ms = ms.max(1);
                            self.ex.set_step_ms(ms);
                            self.step_ms = ms;
                        }
                    }
                    other => {
                        self.send_error(to, format!("unknown replay action: {other}"), request_id);
                        return;
                    }
                }
                if let Some(s) = speed {
                    if action == "play" && s > 0.0 && s <= 100.0 {
                        self.speed = s;
                    }
                }
                self.directed(
                    to,
                    json!({
                        "type": "replay_state",
                        "paused": self.ex.is_paused(),
                        "now_ms": self.ex.now_ms(),
                        "speed": self.speed,
                        "step_ms": self.step_ms,
                        "done": self.ex.replay_done(&self.symbol, &self.timeframe),
                        "fed": self.ex.fetch_ohlcv_live(&self.symbol, &self.timeframe).len(),
                        "total": self.ex.fetch_ohlcv(&self.symbol, &self.timeframe, None, None).len(),
                        "request_id": request_id,
                    }),
                );
            }
        }
    }

    /// One auto-tick while playing: exactly one sim step, then deltas.
    fn auto_tick(&mut self) {
        if self.ex.is_paused() {
            return;
        }
        if self.ex.replay_done(&self.symbol, &self.timeframe) {
            self.ex.pause();
            let replay =
                replay_state_json(&self.ex, &self.symbol, &self.timeframe, self.speed, self.step_ms);
            self.broadcast(replay);
            return;
        }
        self.ex.step_n(1);
        self.push_delta();
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
// Market-data loading (disk cache first, deterministic synthetic fallback)
// ---------------------------------------------------------------------------

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

/// Make sure `symbol@timeframe` is loaded; cache-first, synthetic fallback.
fn ensure_series(ex: &mut Exchange, symbol: &str, tf: &str, data_dir: &Path) {
    if !ex.fetch_ohlcv(symbol, tf, None, Some(1)).is_empty() {
        return;
    }
    let path = data_dir.join(binance_file(symbol, tf));
    if let Some(candles) = load_csv(&path) {
        let n = candles.len();
        if Symbol::parse(symbol).is_ok() {
            ex.add_market(&Symbol::parse(symbol).unwrap(), MarketKind::Spot);
        }
        ex.load_candles(symbol, tf, candles);
        log!("[engine] {symbol} {tf}: {n} bars from cache ({})", path.display());
        return;
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
                    let job = EngineJob { client_id, cmd };
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
}

#[derive(Debug)]
struct Args {
    port: u16,
    symbol: String,
    timeframe: String,
    data_dir: PathBuf,
}

fn parse_args() -> Args {
    let mut port = 9001u16;
    let mut symbol = "BTC/USDT".to_string();
    let mut timeframe = "1m".to_string();
    let mut data_dir = PathBuf::from("data/ohlcv");
    let mut it = std::env::args().skip(1).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = it.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            "--symbol" => symbol = it.next().unwrap_or(symbol),
            "--timeframe" | "--tf" => timeframe = it.next().unwrap_or(timeframe),
            "--data-dir" => data_dir = it.next().map(PathBuf::from).unwrap_or(data_dir),
            "--help" | "-h" => {
                println!("simccxt-server [--port 9001] [--symbol BTC/USDT] [--timeframe 1m] [--data-dir data/ohlcv]");
                std::process::exit(0);
            }
            _ => log!("[args] ignoring unknown flag {a}"),
        }
    }
    Args { port, symbol, timeframe, data_dir }
}

fn engine_main(
    rx: mpsc::Receiver<EngineJob>,
    out: broadcast::Sender<String>,
    args: Args,
) {
    let mut ex = Exchange::default();
    ex.deposit("USDT", 1_000_000.0);
    let mut engine = Engine {
        ex,
        symbol: String::new(),
        timeframe: String::new(),
        speed: DEFAULT_SPEED,
        step_ms: 60_000,
        fed: 0,
        fills_seen: 0,
        last_beat: std::time::Instant::now(),
        out,
    };
    let note = engine.use_series(args.symbol, args.timeframe, &args.data_dir);
    log!("[engine] initial series: {note}");
    // Push the initial state so late subscribers get deltas only after this.
    engine.fed = engine.ex.fetch_ohlcv_live(&engine.symbol, &engine.timeframe).len();
    engine.broadcast(engine.account_event());
    engine.broadcast(replay_state_json(
        &engine.ex, &engine.symbol, &engine.timeframe, engine.speed, engine.step_ms,
    ));

    loop {
        let interval = if engine.ex.is_paused() {
            Duration::from_millis(IDLE_POLL_MS)
        } else {
            let ms = (1000.0 / engine.speed.max(0.05)).round() as u64;
            Duration::from_millis(ms.clamp(10, 5000))
        };
        match rx.recv_timeout(interval) {
            Ok(job) => {
                engine.handle(job, &args.data_dir);
                engine.maybe_heartbeat();
            }
            Err(RecvTimeoutError::Timeout) => {
                engine.auto_tick();
                engine.maybe_heartbeat();
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
}
