use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use crate::stamp;
use chrono::{DateTime, NaiveDateTime, Utc};
use ccxt::{from_id, TypedExchange, Value};
use simccxt_broker::MarketKind;
use simccxt_core::{Symbol, OHLCV};
use simccxt_exchange::Exchange;

// ---------------------------------------------------------------------------
// Network gate
//
// The engine thread and the background preloader share ONE CCXT network path,
// and CCXT serializes internally. Without a gate a background warm-up of the
// whole watchlist would freeze every user action for as long as the queue
// lasts. This gate gives the engine strict priority: the preloader may only
// touch the network while the engine is idle, and it re-checks between pairs,
// so a click is never stuck behind more than the pair in flight. Cache hits
// never take the gate at all — they are plain file reads.
// ---------------------------------------------------------------------------

thread_local! {
    /// True on the preloader thread: it enters the gate as a background client.
    static BACKGROUND_NET: Cell<bool> = const { Cell::new(false) };
}

/// Mark this thread as the background preloader (call once, at its start).
pub(super) fn mark_background_net() {
    BACKGROUND_NET.with(|flag| flag.set(true));
}

/// One download's exclusive access to the shared CCXT path (released on drop).
struct NetPermit {
    _private: (),
}

struct NetGate {
    state: Mutex<GateState>,
    ready: Condvar,
}

struct GateState {
    /// A download is in flight.
    busy: bool,
    /// A user-facing download is queued: background work must wait for it.
    foreground_waiting: bool,
}

static NET_GATE: NetGate = NetGate {
    state: Mutex::new(GateState { busy: false, foreground_waiting: false }),
    ready: Condvar::new(),
};

impl NetGate {
    /// Block until the shared network path may be used. Background clients
    /// additionally wait for every queued user-facing download.
    fn acquire(&self, background: bool) -> NetPermit {
        let waiting_since = std::time::Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        if !background {
            state.foreground_waiting = true;
        }
        loop {
            let free = !state.busy && (!background || !state.foreground_waiting);
            if free {
                state.busy = true;
                state.foreground_waiting = false;
                // A user-facing download must never sit long behind the warm-up.
                if !background && waiting_since.elapsed() > std::time::Duration::from_secs(2) {
                    log!("[net] engine download waited {:?} for the background loader", waiting_since.elapsed());
                }
                return NetPermit { _private: () };
            }
            state = self.ready.wait(state).unwrap_or_else(|err| err.into_inner());
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state.busy = false;
        drop(state);
        self.ready.notify_all();
    }
}

impl Drop for NetPermit {
    fn drop(&mut self) {
        NET_GATE.release();
    }
}

/// Gate permit for the calling thread's role (engine vs preloader).
fn net_permit() -> NetPermit {
    NET_GATE.acquire(BACKGROUND_NET.with(|flag| flag.get()))
}

// ---------------------------------------------------------------------------
// Market-data loading (memory → exchange-scoped disk cache → CCXT → synthetic)
// ---------------------------------------------------------------------------

/// How many klines to pull when a series is missing from cache and memory.
const DOWNLOAD_BARS: usize = 5_000;
const MAX_WINDOW_BARS: usize = 100_000;
const OHLCV_PAGE: usize = 1_000;
/// How many open-interest points to keep (coarser grid than klines).
const OI_POINTS: usize = 1_500;
const MAX_OI_HISTORY_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
const OI_PAGE: usize = 500;

pub(super) struct SeriesRequest<'a> {
    pub exchange: &'a str,
    pub symbol: &'a str,
    pub timeframe: &'a str,
    pub data_dir: &'a Path,
    pub allow_net: bool,
    pub force_reload: bool,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Step size per timeframe.
pub(super) fn tf_step_ms(tf: &str) -> u64 {
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

fn series_file(exchange: &str, symbol: &str, tf: &str) -> String {
    let file = binance_file(symbol, tf);
    if exchange == "binance" { file } else { format!("{exchange}_{file}") }
}

/// Parse a timestamp cell: epoch seconds/ms (auto-detected) or ISO-8601.
fn parse_ts_ms(raw: &str) -> Option<i64> {
    let t = raw.trim();
    if let Ok(v) = t.parse::<f64>() {
        return Some(if v.abs() < 1e12 {
            (v * 1000.0) as i64
        } else {
            v as i64
        });
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
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_path(path)
        .ok()?;
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
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Write candles to the on-disk cache (`ts_ms,open,high,low,close,volume`).
///
/// Written to a sibling temp file and renamed into place, so a reader (or a
/// second session downloading the same tail) never sees a half-written series.
fn save_csv(path: &Path, candles: &[OHLCV]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = tmp_sibling(path);
    {
        let mut wtr = csv::Writer::from_path(&tmp).map_err(|e| e.to_string())?;
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
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Temp path next to `path`, unique per writer (pid + a monotonic counter), so
/// concurrent downloads of the same series cannot clobber each other.
fn tmp_sibling(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "series".to_string());
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!(".{name}.{}.{seq}.tmp", std::process::id()))
}

/// Convert the row-shaped candles the exchange exposes
/// (`[ts_ms, open, high, low, close, volume]`) into `OHLCV` values.
fn rows_as_candles(rows: &[Vec<f64>]) -> Vec<OHLCV> {
    rows.iter()
        .filter_map(|row| {
            if row.len() < 6 {
                return None;
            }
            Some(OHLCV::new(
                DateTime::from_timestamp_millis(row[0] as i64)?,
                row[1],
                row[2],
                row[3],
                row[4],
                row[5],
            ))
        })
        .collect()
}

/// The base interval every derived timeframe is built from.
///
/// Downloading 5m once and aggregating covers 15m/1h/4h/1d from the same bars,
/// so a pair costs ONE request instead of four, and every timeframe necessarily
/// agrees on prices (they come from one source). 1m/1s are never derived: they
/// are finer than the base and get their own download when asked for.
pub(super) const BASE_TF: &str = "5m";

/// Timeframes built from [`BASE_TF`] rather than downloaded.
pub(super) fn is_derived_tf(tf: &str) -> bool {
    matches!(tf, "15m" | "1h" | "4h" | "1d")
}

/// Roll `rows` up from `from_step` to `to_step`: open from the first bar of the
/// bucket, close from the last, high/low the extremes, volume the sum. Buckets
/// with no bars are skipped (no synthetic filler — a gap stays a gap).
pub(super) fn aggregate_ohlcv(rows: &[OHLCV], from_step: u64, to_step: u64) -> Vec<OHLCV> {
    if from_step == 0 || to_step == 0 || to_step <= from_step {
        return rows.to_vec();
    }
    let mut out: Vec<OHLCV> = Vec::new();
    let to = to_step as i64;
    let mut bucket: Option<i64> = None;
    let mut acc: Option<OHLCV> = None;
    for row in rows {
        let ms = row.timestamp.timestamp_millis();
        let key = ms.div_euclid(to) * to;
        if bucket != Some(key) {
            if let (Some(_), Some(done)) = (bucket, acc.take()) {
                out.push(done);
            }
            bucket = Some(key);
            acc = Some(OHLCV::new(row.timestamp, row.open, row.high, row.low, row.close, row.volume));
            continue;
        }
        if let Some(open) = acc.as_mut() {
            open.high = open.high.max(row.high);
            open.low = open.low.min(row.low);
            open.close = row.close;
            open.volume += row.volume;
        }
    }
    if acc.is_some() {
        out.extend(acc);
    }
    // Keep the bucket's opening timestamp, not the first bar's own timestamp,
    // so the grid lines up with what a venue would report.
    for candle in out.iter_mut() {
        let ms = candle.timestamp.timestamp_millis();
        candle.timestamp = DateTime::from_timestamp_millis(ms.div_euclid(to) * to)
            .unwrap_or(candle.timestamp);
    }
    out
}

/// Which parts of `[start_ms, end_ms]` a cached series does not cover yet.
///
/// Returns `None` when the cache already spans the range, otherwise the
/// missing ranges (usually a single tail). This is what keeps re-subscribing to
/// a slightly later window from re-downloading the whole history.
fn missing_ranges(
    rows: &[OHLCV],
    start_ms: i64,
    end_ms: i64,
    step: i64,
) -> Option<Vec<(i64, i64)>> {
    let mut gaps: Vec<(i64, i64)> = Vec::new();
    match (rows.first(), rows.last()) {
        (None, _) => gaps.push((start_ms, end_ms)),
        (Some(first), last) => {
            // Cache starts after the window: everything up to its first bar.
            let f = first.timestamp.timestamp_millis();
            if f > start_ms + step {
                gaps.push((start_ms, f - step));
            }
            // Cache ends before the window: the tail.
            if let Some(l) = last {
                let l = l.timestamp.timestamp_millis();
                if l < end_ms - step {
                    gaps.push((l + step, end_ms));
                }
            }
        }
    }
    gaps.retain(|(from, to)| to >= from);
    if gaps.is_empty() {
        None
    } else {
        Some(gaps)
    }
}

/// Merge freshly downloaded bars into the cached series (sorted, de-duplicated).
fn merge_candles(cached: &[OHLCV], fresh: Vec<OHLCV>) -> Vec<OHLCV> {
    let mut all: Vec<OHLCV> = Vec::with_capacity(cached.len() + fresh.len());
    all.extend_from_slice(cached);
    all.extend(fresh);
    all.sort_by_key(|c| c.timestamp);
    all.dedup_by_key(|c| c.timestamp);
    all
}

fn ccxt_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start CCXT runtime: {err}"))
}

fn ccxt_exchange(exchange: &str) -> Result<Box<dyn TypedExchange>, String> {
    from_id(exchange, None).ok_or_else(|| format!("CCXT exchange '{exchange}' is not enabled"))
}

/// One CCXT session per thread and exchange: instantiating the exchange and
/// loading its markets costs seconds, so a warm-up of the whole watchlist must
/// not repeat that for every series (nor for every page).
struct CcxtSession {
    exchange: Box<dyn TypedExchange>,
    runtime: tokio::runtime::Runtime,
    markets_loaded: bool,
}

thread_local! {
    static CCXT_SESSIONS: RefCell<HashMap<String, CcxtSession>> =
        RefCell::new(HashMap::new());
}

/// Run `body` on this thread's CCXT session for `exchange_id`, creating it and
/// loading its markets on first use. `body` is called with the session already
/// ready and takes the net gate per request, so a queued user-facing download
/// only ever waits for the request in flight.
fn with_ccxt<T>(
    exchange_id: &str,
    body: impl FnOnce(&mut CcxtSession) -> Result<T, String>,
) -> Result<T, String> {
    CCXT_SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        if !sessions.contains_key(exchange_id) {
            let exchange = {
                // Instantiating touches CCXT's shared state: gate it too.
                let _permit = net_permit();
                ccxt_exchange(exchange_id)?
            };
            sessions.insert(
                exchange_id.to_string(),
                CcxtSession {
                    exchange,
                    runtime: ccxt_runtime()?,
                    markets_loaded: false,
                },
            );
        }
        let session = sessions.get_mut(exchange_id).expect("inserted just above");
        if !session.markets_loaded {
            let CcxtSession { exchange, runtime, .. } = session;
            let _permit = net_permit();
            runtime.block_on(async {
                exchange
                    .call_raw("load_markets", vec![Value::Bool(false), Value::Null])
                    .await
            })
            .map_err(|err| format!("{exchange_id}: load markets failed: {err}"))?;
            session.markets_loaded = true;
        }
        body(session)
    })
}

fn ccxt_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse::<f64>().ok())
        .filter(|number| number.is_finite())
}

fn ccxt_timestamp_ms(value: &Value) -> Option<i64> {
    if let Some(timestamp) = value.as_str() {
        return parse_ts_ms(timestamp);
    }
    ccxt_number(value).map(|timestamp| {
        if timestamp.abs() < 1e12 {
            (timestamp * 1000.0) as i64
        } else {
            timestamp as i64
        }
    })
}

fn oi_row(row: &Value) -> Option<(i64, f64)> {
    let interest = ccxt::types::OpenInterest::from_value(row.clone());
    let raw = row.as_map();
    let timestamp = interest.timestamp.or_else(|| {
        raw?.get("timestamp").and_then(ccxt_timestamp_ms)
    })?;
    let value = interest
        .open_interest_value
        .or(interest.quote_volume)
        .or(interest.open_interest_amount)
        .or_else(|| raw?.get("openInterest").and_then(ccxt_number))?;
    value.is_finite().then_some((timestamp, value))
}

/// Fetch and normalize OHLCV through CCXT's unified API. Each exchange is
/// instantiated only for this download; replay and order execution remain in
/// the simulator's Exchange.
fn download_series(exchange_id: &str, symbol: &str, tf: &str, bars: usize) -> Result<Vec<OHLCV>, String> {
    let end_ms = Utc::now().timestamp_millis();
    let start_ms = end_ms - (bars as i64) * tf_step_ms(tf) as i64;
    download_series_window(exchange_id, symbol, tf, start_ms, end_ms)
}

fn download_series_window(
    exchange_id: &str,
    symbol: &str,
    tf: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<OHLCV>, String> {
    if !["1s", "1m", "5m", "15m", "1h", "4h", "1d"].contains(&tf) {
        return Err(format!("unsupported timeframe {tf}"));
    }
    if start_ms >= end_ms {
        return Err("market-data range start must be before its end".into());
    }
    let step = tf_step_ms(tf) as i64;
    let bars = ((end_ms - start_ms) / step + 1) as usize;
    if bars > MAX_WINDOW_BARS {
        return Err(format!(
            "requested range exceeds {MAX_WINDOW_BARS} bars at {tf}; narrow the dates"
        ));
    }
    with_ccxt(exchange_id, |session| {
        let mut cursor = start_ms;
        let mut candles = Vec::with_capacity(bars);
        while cursor <= end_ms && candles.len() < bars {
            let limit = (bars - candles.len()).min(OHLCV_PAGE);
            // Gate per page, not per range: a user-facing download queued up
            // mid-range then only waits for the page in flight.
            let page = {
                let _permit = net_permit();
                let CcxtSession { exchange, runtime, .. } = &mut *session;
                runtime.block_on(async {
                    exchange
                        .call_raw(
                            "fetch_ohlcv",
                            vec![
                                Value::from(symbol),
                                Value::from(tf),
                                Value::Int(cursor),
                                Value::Int(limit as i64),
                                Value::Null,
                            ],
                        )
                        .await
                })
            }
            .map_err(|err| format!("{exchange_id} {symbol} {tf}: {err}"))?;
            let Value::Arr(rows) = page else {
                return Err(format!("{exchange_id} returned invalid OHLCV data"));
            };
            if rows.is_empty() {
                break;
            }
            let mut last_time = None;
            for row in rows.iter() {
                let Value::Arr(fields) = row else { continue };
                if fields.len() < 6 { continue; }
                let Some(ms) = fields.first().and_then(ccxt_number).map(|time| time as i64) else { continue };
                let Some(dt) = DateTime::from_timestamp_millis(ms) else { continue };
                let Some(open) = ccxt_number(&fields[1]) else { continue };
                let Some(high) = ccxt_number(&fields[2]) else { continue };
                let Some(low) = ccxt_number(&fields[3]) else { continue };
                let Some(close) = ccxt_number(&fields[4]) else { continue };
                let Some(volume) = ccxt_number(&fields[5]) else { continue };
                if ms >= start_ms && ms <= end_ms {
                    candles.push(OHLCV::new(dt, open, high, low, close, volume));
                }
                last_time = Some(ms);
            }

            let Some(last_time) = last_time else { break };
            if last_time < cursor {
                break;
            }
            cursor = last_time + step;
        }
        candles.sort_by_key(|candle| candle.timestamp.timestamp_millis());
        candles.dedup_by_key(|candle| candle.timestamp.timestamp_millis());
        if candles.is_empty() {
            return Err(format!("{exchange_id} has no {symbol} candles at {tf}"));
        }
        Ok(candles)
    })
}

fn window_file(exchange: &str, symbol: &str, tf: &str, start_ms: i64, end_ms: i64) -> String {
    // Cache key aligned to day boundaries: a client asking for "now-60d"
    // must hit the same file on every reconnect, otherwise each subscribe
    // re-downloads the whole window (the raw ms differ per request).
    const DAY_MS: i64 = 86_400_000;
    let key_start = start_ms.div_euclid(DAY_MS) * DAY_MS;
    let key_end = end_ms.div_euclid(DAY_MS) * DAY_MS + DAY_MS;
    let base = binance_file(symbol, tf)
        .trim_end_matches(".csv")
        .to_string();
    format!(
        "{exchange}_{base}_{key_start}_{key_end}.csv",
    )
}

/// Load enough history for the requested window, fetching and caching an
/// exchange-specific range when the normal recent-history cache is too short.
pub(super) fn ensure_series_window(
    ex: &mut Exchange,
    request: SeriesRequest<'_>,
) -> Result<(), String> {
    let SeriesRequest {
        exchange,
        symbol,
        timeframe,
        data_dir,
        allow_net,
        force_reload,
        start_ms,
        end_ms,
    } = request;
    ensure_series(ex, exchange, symbol, timeframe, data_dir, allow_net, force_reload)?;
    let step = tf_step_ms(timeframe) as i64;
    let cached = rows_as_candles(&ex.fetch_ohlcv(symbol, timeframe, None, None));
    let gaps = match missing_ranges(&cached, start_ms, end_ms, step) {
        None => return Ok(()),
        Some(gaps) => gaps,
    };
    if !allow_net {
        return Err(format!(
            "{exchange} {symbol} {timeframe}: requested dates are outside loaded history; enable network access or narrow the range"
        ));
    }

    // Download ONLY the gaps (usually the tail) and merge into the one canonical
    // per-series cache file. The old code wrote a separate day-keyed file per
    // window, so every slightly different date range re-downloaded the whole
    // history and left dozens of near-duplicate files behind.
    //
    // For a derived timeframe the download targets the 5m BASE (one file, one
    // source of truth) and the result is aggregated back up — so topping up a
    // 1h chart also improves the 15m/4h/1d views, and costs a single request.
    let (fetch_tf, fetch_step) = if is_derived_tf(timeframe) {
        (BASE_TF, tf_step_ms(BASE_TF) as i64)
    } else {
        (timeframe, step)
    };
    let path = data_dir.join(series_file(exchange, symbol, fetch_tf));
    let mut merged: Vec<OHLCV> = if fetch_tf == timeframe {
        cached
    } else {
        load_csv(&path).unwrap_or_default()
    };
    let mut downloaded = 0usize;
    for (from, to) in gaps {
        // Map the derived gap onto the base grid before asking for it.
        let (from, to) = if fetch_tf == timeframe {
            (from, to)
        } else {
            (from.div_euclid(fetch_step) * fetch_step, to)
        };
        let fresh = download_series_window(exchange, symbol, fetch_tf, from, to)?;
        downloaded += fresh.len();
        if fresh.is_empty() {
            continue;
        }
        merged = merge_candles(&merged, fresh);
    }
    if merged.is_empty() {
        return Err(format!("{exchange} {symbol} {timeframe}: no candles in requested date range"));
    }
    save_csv(&path, &merged)?;
    if let Ok(sym) = Symbol::parse(symbol) {
        ex.add_market(&sym, MarketKind::Linear);
    }
    let loaded = if fetch_tf == timeframe {
        merged.clone()
    } else {
        aggregate_ohlcv(&merged, tf_step_ms(fetch_tf), tf_step_ms(timeframe))
    };
    ex.load_candles(symbol, timeframe, loaded.clone());
    log!(
        "[engine] {exchange} {symbol} {timeframe}: +{downloaded} {fetch_tf} bars merged into {} → {} {timeframe} bars",
        path.display(),
        loaded.len()
    );
    Ok(())
}

/// OI history grid per chart timeframe (use 5m for the 1m candle interval).
/// Public so the preloader can dedupe work per grid (see preload_main).
pub(super) fn oi_period(tf: &str) -> Option<&'static str> {
    match tf {
        "1s" | "1m" | "5m" => Some("5m"),
        "15m" => Some("15m"),
        "1h" => Some("1h"),
        "4h" => Some("4h"),
        "1d" => Some("1d"),
        _ => None,
    }
}

fn oi_points(period: &str) -> usize {
    let step_ms = tf_step_ms(period) as i64;
    ((MAX_OI_HISTORY_MS / step_ms) as usize).min(OI_POINTS)
}

fn oi_file(exchange: &str, symbol: &str, period: &str) -> String {
    let file = format!(
        "{}_oi_{}.csv",
        symbol.replace('/', "").to_uppercase(),
        period
    );
    if exchange == "binance" { file } else { format!("{exchange}_{file}") }
}

/// Write `[(ts_ms, value)]` cache rows (`ts_ms,oi`).
fn save_pairs_csv(path: &Path, rows: &[(i64, f64)]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut wtr = csv::Writer::from_path(path).map_err(|e| e.to_string())?;
    wtr.write_record(["ts_ms", "oi"])
        .map_err(|e| e.to_string())?;
    for (ts, v) in rows {
        wtr.write_record([ts.to_string(), v.to_string()])
            .map_err(|e| e.to_string())?;
    }
    wtr.flush().map_err(|e| e.to_string())?;
    Ok(())
}

fn load_pairs_csv(path: &PathBuf) -> Option<Vec<(i64, f64)>> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_path(path)
        .ok()?;
    let mut out = Vec::new();
    for rec in rdr.records().flatten() {
        if rec.len() < 2 {
            continue;
        }
        let ms = parse_ts_ms(rec.get(0).unwrap_or(""))?;
        let v = rec.get(1).unwrap_or("").parse::<f64>().ok()?;
        out.push((ms, v));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Download open-interest history through CCXT where the selected venue
/// exposes the unified fetchOpenInterestHistory method.
fn download_oi(exchange_id: &str, symbol: &str, tf: &str, points: usize) -> Result<Vec<(i64, f64)>, String> {
    let period = oi_period(tf).ok_or_else(|| format!("no OI grid for {tf}"))?;
    let step = tf_step_ms(period) as i64;
    let end_ms = Utc::now().timestamp_millis();
    let start_ms = end_ms - (points as i64) * step;
    let mut cursor = start_ms;
    let swap_symbol = symbol
        .split_once('/')
        .map(|(base, quote)| format!("{base}/{quote}:{quote}"))
        .unwrap_or_else(|| symbol.to_string());
    with_ccxt(exchange_id, |session| {
        let mut out = Vec::with_capacity(points);
        while cursor < end_ms && out.len() < points {
            let limit = (points - out.len()).min(OI_PAGE);
            // Per-request gate (see NET_GATE): never hold it across pages.
            let rows = {
                let _permit = net_permit();
                let CcxtSession { exchange, runtime, .. } = &mut *session;
                runtime.block_on(async {
                    exchange
                        .call_raw(
                            "fetch_open_interest_history",
                            vec![
                                Value::from(swap_symbol.as_str()),
                                Value::from(period),
                                Value::Int(cursor),
                                Value::Int(limit as i64),
                                Value::Null,
                            ],
                        )
                        .await
                })
            }
            .map_err(|err| format!("{exchange_id} {swap_symbol} OI: {err}"))?;
            let Value::Arr(rows) = rows else {
                return Err(format!("{exchange_id} returned invalid OI data"));
            };
            if rows.is_empty() { break; }
            let mut last_ts = None;
            for row in rows.iter() {
                let Some((ts, value)) = oi_row(row) else { continue };
                last_ts = Some(last_ts.map_or(ts, |last: i64| last.max(ts)));
                if ts >= start_ms && ts <= end_ms && value.is_finite() {
                    out.push((ts, value));
                }
            }
            let Some(last_ts) = last_ts else { break };
            if last_ts < cursor { break; }
            cursor = last_ts + 1;
        }
        out.sort_by_key(|row| row.0);
        out.dedup_by_key(|row| row.0);
        if out.is_empty() {
            return Err(format!("{exchange_id} has no OI history for {symbol}"));
        }
        Ok(out)
    })
}

/// Deterministic synthetic random walk (xorshift64, fixed seed) — explicit
/// fallback when no cache file exists. Never silent: the source is logged.
fn synthetic(symbol: &str, tf: &str, n: usize) -> Vec<OHLCV> {
    let step = tf_step_ms(tf) as i64;
    let start = Utc::now().timestamp_millis() - (n as i64) * step;
    let mut state: u64 =
        0x9E37_79B9_7F4A_7C15 ^ (symbol.len() as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
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

/// Marker written when a venue has no open-interest history for a pair.
/// Without it every subscribe would retry the same doomed request and pay
/// its full timeout on the engine thread; with it the answer is instant.
fn oi_missing_file(exchange: &str, symbol: &str, period: &str) -> String {
    let file = format!("{}_oi_{}_none", symbol.replace('/', "").to_uppercase(), period);
    if exchange == "binance" { file } else { format!("{exchange}_{file}") }
}

/// True when a previous attempt proved this pair has no OI history.
pub(super) fn oi_known_missing(exchange: &str, symbol: &str, tf: &str, data_dir: &Path) -> bool {
    let Some(period) = oi_period(tf) else { return true };
    data_dir.join(oi_missing_file(exchange, symbol, period)).exists()
}

/// Remember that this pair has no OI history (best effort, ignore failures).
pub(super) fn mark_oi_missing(exchange: &str, symbol: &str, tf: &str, data_dir: &Path) {
    let Some(period) = oi_period(tf) else { return };
    let path = data_dir.join(oi_missing_file(exchange, symbol, period));
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, "no open interest history for this pair\n");
}

/// Make sure open interest for `symbol@timeframe` is loaded into the
/// session: disk cache → CCXT download (saved to cache) → empty + log.
/// OI is chart context, never replayed: no synthetic fallback (an invented
/// OI curve would be worse than none).
pub(super) fn ensure_oi(
    oi: &mut Vec<(i64, f64)>,
    oi_period_out: &mut String,
    exchange: &str,
    symbol: &str,
    tf: &str,
    data_dir: &Path,
    allow_net: bool,
) {
    oi.clear();
    oi_period_out.clear();
    let Some(period) = oi_period(tf) else { return };
    // Note: an exact `_none` sentinel does NOT short-circuit here — a
    // denser-cache fallback below may still deliver real points, and only
    // downloading is pointless when we know the source won't serve it.
    let path = data_dir.join(oi_file(exchange, symbol, period));
    if let Some(rows) = load_pairs_csv(&path) {
        log!(
            "[engine] {exchange} {symbol} OI@{period}: {} pts from cache ({})",
            rows.len(),
            path.display()
        );
        *oi = rows;
        *oi_period_out = period.to_string();
        return;
    }
    // Exact cache missing (or sentinel): derive a usable overlay from the
    // coarsest finer-grained cached period. OI is chart context —real points
    // from a denser cache beat "нет данных" for a whole timeframe.
    if let Some((sub, rows)) = coarsest_finer_cached_oi(exchange, symbol, period, data_dir) {
        log!(
            "[engine] {exchange} {symbol} OI@{period}: derived from {sub} cache ({} pts)",
            rows.len()
        );
        *oi = rows;
        *oi_period_out = period.to_string();
        return;
    }
    if oi_known_missing(exchange, symbol, tf, data_dir) {
        log!("[engine] {exchange} {symbol} OI@{period}: none available (known)");
        return;
    }
    if allow_net {
        log!("[engine] {exchange} {symbol} OI@{period}: no cache, downloading via CCXT…");
        match download_oi(exchange, symbol, tf, oi_points(period)) {
            Ok(rows) => {
                if let Err(e) = save_pairs_csv(&path, &rows) {
                    log!("[engine] {exchange} {symbol} OI@{period}: cache write failed: {e}");
                }
                log!(
                    "[engine] {exchange} {symbol} OI@{period}: {} pts via CCXT (cached {})",
                    rows.len(),
                    path.display()
                );
                *oi = rows;
                *oi_period_out = period.to_string();
            }
            Err(e) => {
                // Remember the verdict: this venue has no OI history for this
                // pair, so later subscribes skip the doomed request entirely.
                log!("[engine] {exchange} {symbol} OI@{period}: download failed: {e}");
                mark_oi_missing(exchange, symbol, tf, data_dir);
            }
        }
    }
}

/// True when a real on-disk series cache exists for `symbol@tf`.
/// The read-only diary candles API checks this first: offline with no cache
/// must answer "no data" instead of `ensure_series`'s silent synthetic bars —
/// fabricated history must never be served as a trader's chart.
pub(super) fn has_series_cache(exchange: &str, symbol: &str, tf: &str, data_dir: &Path) -> bool {
    data_dir.join(series_file(exchange, symbol, tf)).exists()
}

/// Closest finer-grained cached OI series for `exchange/symbol` that can
/// stand in for `period`. Order of preference: coarser sub-periods first
/// (15m beats 5m for a 1h target — denser rows noisily clutter the pane).
fn coarsest_finer_cached_oi(
    exchange: &str,
    symbol: &str,
    period: &str,
    data_dir: &Path,
) -> Option<(&'static str, Vec<(i64, f64)>)> {
    let target_step = tf_step_ms(period);
    for sub_period in ["4h", "1h", "15m", "5m"] {
        let sub_step = tf_step_ms(sub_period);
        if sub_step >= target_step {
            continue;
        }
        let path = data_dir.join(oi_file(exchange, symbol, sub_period));
        if let Some(rows) = load_pairs_csv(&path) {
            if !rows.is_empty() {
                return Some((sub_period, rows));
            }
        }
    }
    None
}

/// Make sure `symbol@timeframe` is loaded. Order: already in memory →
/// disk cache → CCXT download (saved to cache) → synthetic fallback.
/// Network is only attempted when `allow_net` (off in unit tests, and with
/// `--offline`); every source is logged — synthetic is never silent.
pub(super) fn ensure_series(
    ex: &mut Exchange,
    exchange: &str,
    symbol: &str,
    tf: &str,
    data_dir: &Path,
    allow_net: bool,
    force_reload: bool,
) -> Result<(), String> {
    if !force_reload && !ex.fetch_ohlcv(symbol, tf, None, Some(1)).is_empty() {
        return Ok(());
    }
    // 15m/1h/4h/1d are rolled up from the 5m base rather than downloaded: one
    // request per pair instead of four, and every timeframe then agrees on
    // prices because they share one source. The base cache is authoritative, so
    // only ONE file per pair is ever written.
    if is_derived_tf(tf) {
        let base_path = data_dir.join(series_file(exchange, symbol, BASE_TF));
        if let Some(base) = load_csv(&base_path) {
            let derived = aggregate_ohlcv(&base, tf_step_ms(BASE_TF), tf_step_ms(tf));
            if !derived.is_empty() {
                let n = derived.len();
                if let Ok(sym) = Symbol::parse(symbol) {
                    ex.add_market(&sym, MarketKind::Linear);
                }
                ex.load_candles(symbol, tf, derived);
                log!(
                    "[engine] {exchange} {symbol} {tf}: {n} bars aggregated from {BASE_TF} ({})",
                    base_path.display()
                );
                return Ok(());
            }
        }
    }
    let path = data_dir.join(series_file(exchange, symbol, tf));
    if let Some(candles) = load_csv(&path) {
        let n = candles.len();
        if let Ok(sym) = Symbol::parse(symbol) {
            ex.add_market(&sym, MarketKind::Linear);
        }
        ex.load_candles(symbol, tf, candles);
        log!(
            "[engine] {exchange} {symbol} {tf}: {n} bars from cache ({})",
            path.display()
        );
        return Ok(());
    }
    if allow_net {
        log!("[engine] {exchange} {symbol} {tf}: no cache, downloading {DOWNLOAD_BARS} bars via CCXT…");
        match download_series(exchange, symbol, tf, DOWNLOAD_BARS) {
            Ok(candles) => {
                let n = candles.len();
                if let Err(e) = save_csv(&path, &candles) {
                    log!("[engine] {symbol} {tf}: cache write failed: {e}");
                }
                if let Ok(sym) = Symbol::parse(symbol) {
                    ex.add_market(&sym, MarketKind::Linear);
                }
                ex.load_candles(symbol, tf, candles);
                log!(
                    "[engine] {exchange} {symbol} {tf}: {n} bars via CCXT (cached {})",
                    path.display()
                );
                return Ok(());
            }
            Err(e) => return Err(format!("{exchange} {symbol} {tf}: {e}")),
        }
    }
    let candles = synthetic(symbol, tf, 2000);
    if let Ok(sym) = Symbol::parse(symbol) {
        ex.add_market(&sym, MarketKind::Linear);
    }
    ex.load_candles(symbol, tf, candles);
    log!(
        "[engine] {exchange} {symbol} {tf}: 2000 synthetic bars (offline; no cache at {})",
        path.display()
    );
    Ok(())
}

/// True when `candles` already spans `[start_ms, end_ms]` (one step of slack
/// so the last visible bar counts as covered). Same rule as
/// `ensure_series_window`, kept in one place.
fn covers_range(candles: &[OHLCV], start_ms: i64, end_ms: i64, step: i64) -> bool {
    candles.first().is_some_and(|c| c.timestamp.timestamp_millis() <= start_ms + step)
        && candles.last().is_some_and(|c| c.timestamp.timestamp_millis() + step >= end_ms)
}

/// Warm the candle caches for one `symbol@timeframe` WITHOUT touching a
/// session: recent history first, then the date-window cache when the replay
/// window is not covered by it. A cache hit is just a small CSV read; a miss is
/// downloaded through CCXT and saved exactly like a later `subscribe` would
/// do — the only difference is that this runs off the engine thread, so the UI
/// never waits for a download. Returns a short note for the progress event.
pub(super) fn warm_series_cache(
    exchange: &str,
    symbol: &str,
    timeframe: &str,
    data_dir: &Path,
    allow_net: bool,
    window: Option<(i64, i64)>,
) -> Result<String, String> {
    let step = tf_step_ms(timeframe) as i64;
    let base = data_dir.join(series_file(exchange, symbol, timeframe));
    let mut note = match load_csv(&base) {
        Some(candles) => format!("{} bars from cache", candles.len()),
        None => {
            if !allow_net {
                return Err("no cache and downloads are disabled (--offline)".into());
            }
            let candles = download_series(exchange, symbol, timeframe, DOWNLOAD_BARS)?;
            let bars = candles.len();
            save_csv(&base, &candles).map_err(|err| format!("cache write failed: {err}"))?;
            format!("{bars} bars downloaded")
        }
    };
    if let Some((start_ms, end_ms)) = window {
        let covered = load_csv(&base)
            .map(|candles| covers_range(&candles, start_ms, end_ms, step))
            .unwrap_or(false);
        if !covered {
            let path = data_dir.join(window_file(exchange, symbol, timeframe, start_ms, end_ms));
            if load_csv(&path).is_none() {
                if !allow_net {
                    return Err("replay window is outside the cached history".into());
                }
                let candles =
                    download_series_window(exchange, symbol, timeframe, start_ms, end_ms)?;
                let bars = candles.len();
                save_csv(&path, &candles)
                    .map_err(|err| format!("window cache write failed: {err}"))?;
                note = format!("{note} + {bars} bars for the date window");
            }
        }
    }
    Ok(note)
}

/// True when the OI grid for `timeframe` is already on disk (nothing to warm).
pub(super) fn oi_cache_hit(exchange: &str, symbol: &str, timeframe: &str, data_dir: &Path) -> bool {
    let Some(period) = oi_period(timeframe) else { return true };
    load_pairs_csv(&data_dir.join(oi_file(exchange, symbol, period))).is_some()
}

/// Warm the open-interest cache for one `symbol@timeframe` (chart context, no
/// synthetic fallback). Separate from the candles so the preloader can make
/// every chart switchable first and fill OI afterwards.
pub(super) fn warm_oi_cache(
    exchange: &str,
    symbol: &str,
    timeframe: &str,
    data_dir: &Path,
    allow_net: bool,
) -> Result<String, String> {
    if oi_known_missing(exchange, symbol, timeframe, data_dir) {
        return Err("open interest is unavailable for this pair".into());
    }
    let before = oi_cache_hit(exchange, symbol, timeframe, data_dir);
    let mut oi = Vec::new();
    let mut oi_period_out = String::new();
    ensure_oi(&mut oi, &mut oi_period_out, exchange, symbol, timeframe, data_dir, allow_net);
    if oi.is_empty() {
        // No synthetic fallback for OI: an invented curve is worse than none,
        // and a venue without OI history is a normal, non-fatal outcome.
        return Err("open interest is unavailable for this pair".into());
    }
    Ok(if before {
        format!("OI@{}: {} pts from cache", oi_period_out, oi.len())
    } else {
        format!("OI@{}: {} pts downloaded", oi_period_out, oi.len())
    })
}

/// Restrict an already loaded series to a user-selected replay interval.
/// Returns the final historical candle shown before step-by-step playback.
pub(super) fn apply_series_window(
    ex: &mut Exchange,
    symbol: &str,
    tf: &str,
    start_ms: i64,
    reveal_ms: i64,
    end_ms: i64,
) -> Result<i64, String> {
    if start_ms >= reveal_ms || reveal_ms > end_ms {
        return Err("replay range must satisfy start < reveal start <= end".into());
    }
    let rows = ex.fetch_ohlcv(symbol, tf, None, None);
    let selected: Vec<_> = rows
        .into_iter()
        .filter(|row| {
            let ts = row[0] as i64;
            ts >= start_ms && ts <= end_ms
        })
        .collect();
    let Some(first) = selected.first() else {
        return Err(format!(
            "no {symbol} {tf} candles in selected date range; choose dates within loaded history"
        ));
    };
    let Some(last_visible) = selected.iter().rev().find(|row| (row[0] as i64) < reveal_ms) else {
        return Err("replay start must leave at least one visible historical candle".into());
    };
    let first_ms = first[0] as i64;
    let last_visible_ms = last_visible[0] as i64;
    let candles = selected
        .into_iter()
        .filter_map(|row| {
            let timestamp = chrono::DateTime::from_timestamp_millis(row[0] as i64)?;
            Some(OHLCV::new(timestamp, row[1], row[2], row[3], row[4], row[5]))
        })
        .collect();
    ex.load_candles(symbol, tf, candles);
    Ok(last_visible_ms.max(first_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn oi_falls_back_to_finer_cached_period() {
        // ETH/USDT has no real 1h OI cache (only a `_none` sentinel), but its
        // 15m cache is real history — the 1h pane must derive from it.
        let dir = std::env::temp_dir().join(format!("simccxt-oifallback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rows: Vec<(i64, f64)> = (0..5).map(|i| (1_789_000_000_000 + i * 900_000, 100.0 + i as f64)).collect();
        save_pairs_csv(&dir.join(oi_file("binance", "ETH/USDT", "15m")), &rows).expect("save");
        let (period, loaded) =
            coarsest_finer_cached_oi("binance", "ETH/USDT", "1h", &dir).expect("fallback exists");
        assert_eq!(period, "15m");
        assert_eq!(loaded.len(), 5);
        // 5m target: 15m is coarser, so no fallback.
        assert!(coarsest_finer_cached_oi("binance", "ETH/USDT", "5m", &dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derived_timeframes_roll_up_from_the_5m_base() {
        let step5 = tf_step_ms("5m") as i64;
        let base_ms = 1_789_000_000_000i64 - base_ms_remainder();
        // 12 consecutive 5m bars with a known shape.
        let rows: Vec<OHLCV> = (0..12)
            .map(|i| {
                let price = 100.0 + i as f64;
                OHLCV::new(
                    DateTime::from_timestamp_millis(base_ms + i * step5).unwrap(),
                    price,
                    price + 2.0,
                    price - 1.0,
                    price + 1.0,
                    10.0,
                )
            })
            .collect();

        // 1h = 12 x 5m: exactly one bucket.
        let hour = aggregate_ohlcv(&rows, tf_step_ms("5m"), tf_step_ms("1h"));
        assert_eq!(hour.len(), 1, "12 five-minute bars make one hourly bar");
        assert_eq!(hour[0].open, 100.0, "open comes from the first bar");
        assert_eq!(hour[0].close, 112.0, "close from the last (100+11+1)");
        assert_eq!(hour[0].high, 113.0, "high is the extreme");
        assert_eq!(hour[0].low, 99.0, "low is the extreme");
        assert_eq!(hour[0].volume, 120.0, "volume is summed");
        assert_eq!(
            hour[0].timestamp.timestamp_millis() % tf_step_ms("1h") as i64,
            0,
            "buckets land on the timeframe grid"
        );

        // 15m = 3 x 5m: four buckets.
        let m15 = aggregate_ohlcv(&rows, tf_step_ms("5m"), tf_step_ms("15m"));
        assert_eq!(m15.len(), 4);
        assert!(m15.windows(2).all(|w| w[0].timestamp < w[1].timestamp));

        // Never invent bars for a gap, and never "downsample".
        assert_eq!(aggregate_ohlcv(&rows, tf_step_ms("5m"), tf_step_ms("1m")).len(), rows.len());
        assert_eq!(aggregate_ohlcv(&[], tf_step_ms("5m"), tf_step_ms("1h")).len(), 0);
    }

    /// Offset that puts `1_789_000_000_000` inside a whole 5m bar, so the test's
    /// first bar starts on a clean bucket boundary.
    fn base_ms_remainder() -> i64 {
        1_789_000_000_000i64 % tf_step_ms("1h") as i64
    }

    #[test]
    fn only_the_5m_base_and_the_requested_finer_grid_are_downloaded() {
        // 15m/1h/4h/1d come from the 5m base; 1m/1s are finer, so they keep
        // their own download.
        for tf in ["15m", "1h", "4h", "1d"] {
            assert!(is_derived_tf(tf), "{tf} must be aggregated");
        }
        for tf in ["1s", "1m", BASE_TF] {
            assert!(!is_derived_tf(tf), "{tf} must not be aggregated");
        }
    }

    #[test]
    fn only_the_missing_part_of_a_window_is_requested() {
        // Regression: the window loader used to re-download the WHOLE range
        // whenever the cache fell short, which is what produced dozens of
        // near-duplicate per-window files and slow first paints.
        let step = 300_000i64;
        let base = 1_789_000_000_000i64;
        let cached: Vec<OHLCV> = (0..100)
            .map(|i| {
                OHLCV::new(
                    DateTime::from_timestamp_millis(base + i * step).unwrap(),
                    1.0,
                    1.0,
                    1.0,
                    1.0,
                    1.0,
                )
            })
            .collect();
        let last_cached = base + 99 * step;

        // Fully covered: nothing to do.
        assert_eq!(
            missing_ranges(&cached, base, last_cached, step),
            None,
            "a window inside the cache must not download anything"
        );

        // Window runs past the cache: only the tail.
        let gaps = missing_ranges(&cached, base, last_cached + 10 * step, step)
            .expect("the tail is missing");
        assert_eq!(gaps, vec![(last_cached + step, last_cached + 10 * step)]);

        // Window starts before the cache: only the head.
        let gaps = missing_ranges(&cached, base - 5 * step, last_cached, step)
            .expect("the head is missing");
        assert_eq!(gaps, vec![(base - 5 * step, base - step)]);

        // Empty cache: the whole window.
        assert_eq!(
            missing_ranges(&[], base, last_cached, step),
            Some(vec![(base, last_cached)])
        );
    }

    #[test]
    fn merging_downloaded_bars_never_duplicates_or_reorders() {
        let step = 300_000i64;
        let base = 1_789_000_000_000i64;
        let bar = |ms: i64, close: f64| {
            OHLCV::new(
                DateTime::from_timestamp_millis(ms).unwrap(),
                close,
                close,
                close,
                close,
                1.0,
            )
        };
        let cached = vec![bar(base, 1.0), bar(base + step, 2.0)];
        // A tail that overlaps the last cached bar (the venue re-sent it).
        let fresh = vec![bar(base + step, 2.0), bar(base + 2 * step, 3.0)];
        let merged = merge_candles(&cached, fresh);
        assert_eq!(merged.len(), 3, "the overlapping bar appears once: {merged:?}");
        assert!(merged.windows(2).all(|w| w[0].timestamp < w[1].timestamp));
        assert_eq!(merged[1].close, 2.0, "the cached value survives the merge");
    }

    #[test]
    fn the_cache_write_is_atomic() {
        // Readers (and a concurrent download) must never see a half-written
        // series, so the CSV lands via a temp file + rename.
        let dir = std::env::temp_dir().join(format!("simccxt-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SYM_5m.csv");
        let rows = vec![OHLCV::new(
            DateTime::from_timestamp_millis(1_789_000_000_000).unwrap(),
            1.0,
            2.0,
            0.5,
            1.5,
            10.0,
        )];
        save_csv(&path, &rows).expect("save");
        assert_eq!(load_csv(&path).map(|c| c.len()), Some(1));
        // No temp leftovers behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
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
    fn save_and_load_csv_roundtrip() {
        let dir = std::env::temp_dir().join(format!("simccxt-csv-{}", std::process::id()));
        let path = dir.join("TESTUSDT_1m.csv");
        let candles = vec![
            OHLCV::new(
                DateTime::from_timestamp_millis(1_789_257_600_000).unwrap(),
                1.0,
                2.0,
                0.5,
                1.5,
                10.0,
            ),
            OHLCV::new(
                DateTime::from_timestamp_millis(1_789_257_660_000).unwrap(),
                1.5,
                3.0,
                1.0,
                2.5,
                20.0,
            ),
        ];
        save_csv(&path, &candles).expect("save");
        let back = load_csv(&path).expect("load");
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].close, 2.5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn has_series_cache_only_sees_real_files() {
        // The read-only diary candles API answers "no data" offline instead of
        // letting `ensure_series` fabricate bars — this is that gate.
        let dir = std::env::temp_dir().join(format!("simccxt-cachechk-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_file(dir.join(series_file("binance", "BTC/USDT", "5m")));
        assert!(!has_series_cache("binance", "BTC/USDT", "5m", &dir));

        let path = dir.join(series_file("binance", "BTC/USDT", "5m"));
        std::fs::write(&path, "ts_ms,open,high,low,close,volume\n").expect("touch cache");
        assert!(has_series_cache("binance", "BTC/USDT", "5m", &dir));
        // A different exchange or timeframe must not be mistaken for a hit.
        assert!(!has_series_cache("bybit", "BTC/USDT", "5m", &dir));
        assert!(!has_series_cache("binance", "BTC/USDT", "1m", &dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_rejects_unknown_timeframe() {
        // No network needed: validation fails before any request.
        let err = download_series("binance", "BTC/USDT", "7m", 10).unwrap_err();
        assert!(err.contains("unsupported timeframe"), "{err}");
        assert!(download_oi("binance", "BTC/USDT", "7m", 10).is_err());
    }

    #[test]
    fn oi_grid_mapping() {
        // Most exchanges expose 1m candles but OI at 5m.
        assert_eq!(oi_period("1m"), Some("5m"));
        assert_eq!(oi_period("1s"), Some("5m"));
        assert_eq!(oi_period("15m"), Some("15m"));
        assert_eq!(oi_period("1h"), Some("1h"));
        assert_eq!(oi_period("7m"), None);
    }

    #[test]
    fn oi_points_stay_within_exchange_history_window() {
        assert_eq!(oi_points("1h"), 720);
        assert_eq!(oi_points("15m"), OI_POINTS);
        assert_eq!(oi_points("5m"), OI_POINTS);
        assert!(oi_points("1h") as i64 * tf_step_ms("1h") as i64 <= MAX_OI_HISTORY_MS);
    }

    #[test]
    fn oi_rows_accept_bybit_raw_open_interest_fields() {
        let row = Value::from(serde_json::json!({
            "timestamp": "1790679600000",
            "openInterest": "56029.63800000",
        }));
        assert_eq!(oi_row(&row), Some((1_790_679_600_000, 56_029.638)));
    }

    #[test]
    fn series_window_keeps_visible_history_and_replay_tail() {
        let mut ex = Exchange::default();
        let candles = (1..=5)
            .map(|minute| {
                let timestamp = DateTime::from_timestamp_millis(minute * 60_000).unwrap();
                OHLCV::new(timestamp, 1.0, 2.0, 0.5, 1.5, 10.0)
            })
            .collect();
        ex.load_candles("BTC/USDT", "1m", candles);

        let reset_ms = apply_series_window(
            &mut ex,
            "BTC/USDT",
            "1m",
            60_000,
            180_000,
            240_000,
        )
        .unwrap();

        assert_eq!(reset_ms, 120_000);
        assert_eq!(
            ex.fetch_ohlcv("BTC/USDT", "1m", None, None)
                .iter()
                .map(|row| row[0] as i64)
                .collect::<Vec<_>>(),
            vec![60_000, 120_000, 180_000, 240_000]
        );
    }

    #[test]
    fn warm_series_cache_reports_cache_hits_and_offline_misses() {
        let dir = std::env::temp_dir().join(format!("simccxt-warm-{}", std::process::id()));
        let path = dir.join(series_file("binance", "BTC/USDT", "1m"));
        let candles = (1..=3)
            .map(|minute| {
                let timestamp = DateTime::from_timestamp_millis(minute * 60_000).unwrap();
                OHLCV::new(timestamp, 1.0, 2.0, 0.5, 1.5, 10.0)
            })
            .collect::<Vec<_>>();
        save_csv(&path, &candles).expect("save");

        // A cached pair is reported as a hit and never touches the network.
        let note = warm_series_cache("binance", "BTC/USDT", "1m", &dir, false, None).expect("hit");
        assert!(note.contains("from cache"), "{note}");
        assert!(!oi_cache_hit("binance", "BTC/USDT", "1m", &dir), "no OI file yet");

        // Offline + no cache: an error, not a synthetic series.
        let error = warm_series_cache("binance", "ETH/USDT", "1m", &dir, false, None)
            .expect_err("must fail offline");
        assert!(error.contains("offline"), "{error}");

        // A window outside the cached history is also an offline error.
        let error = warm_series_cache(
            "binance",
            "BTC/USDT",
            "1m",
            &dir,
            false,
            Some((10_000_000_000, 20_000_000_000)),
        )
        .expect_err("window miss must fail offline");
        assert!(error.contains("outside"), "{error}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn covers_range_matches_the_subscribe_rule() {
        let candles = (0..3)
            .map(|hour| {
                let timestamp = DateTime::from_timestamp_millis(hour * 3_600_000).unwrap();
                OHLCV::new(timestamp, 1.0, 2.0, 0.5, 1.5, 10.0)
            })
            .collect::<Vec<_>>();
        let step = tf_step_ms("1h") as i64;
        assert!(covers_range(&candles, 0, 2 * 3_600_000, step));
        assert!(!covers_range(&candles, 5 * 3_600_000, 9 * 3_600_000, step));
        assert!(!covers_range(&[], 0, 3_600_000, step));
    }

    #[test]
    fn net_gate_gives_the_engine_priority_over_the_preloader() {
        use std::sync::mpsc;
        use std::time::Duration;

        // A background download in flight: a user-facing one must wait for it.
        let background = NET_GATE.acquire(true);
        let (tx, rx) = mpsc::channel();
        let foreground = std::thread::spawn(move || {
            let _permit = NET_GATE.acquire(false);
            tx.send(()).ok();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "the engine must not share the network path mid-download"
        );
        drop(background);
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the engine must run as soon as the pair in flight is done"
        );
        foreground.join().ok();

        // A queued user-facing download also holds off new background work.
        let held = NET_GATE.acquire(false);
        let (tx_fg, rx_fg) = mpsc::channel();
        let queued = std::thread::spawn(move || {
            let _permit = NET_GATE.acquire(false);
            tx_fg.send(()).ok();
        });
        std::thread::sleep(Duration::from_millis(50)); // let it reach the queue
        let (tx_bg, rx_bg) = mpsc::channel();
        let background = std::thread::spawn(move || {
            let _permit = NET_GATE.acquire(true);
            tx_bg.send(()).ok();
        });
        assert!(
            rx_bg.recv_timeout(Duration::from_millis(150)).is_err(),
            "background warm-up must yield to a queued user download"
        );
        drop(held);
        assert!(rx_fg.recv_timeout(Duration::from_secs(5)).is_ok());
        assert!(rx_bg.recv_timeout(Duration::from_secs(5)).is_ok());
        queued.join().ok();
        background.join().ok();
    }

    #[test]
    fn oi_missing_marker_short_circuits_the_download() {
        let dir = std::env::temp_dir().join(format!("simccxt-oimissing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!oi_known_missing("binance", "BTC/USDT", "1h", &dir));

        mark_oi_missing("binance", "BTC/USDT", "1h", &dir);
        assert!(oi_known_missing("binance", "BTC/USDT", "1h", &dir));
        // The verdict is per venue: another exchange still tries.
        assert!(!oi_known_missing("bybit", "BTC/USDT", "1h", &dir));
        // A different grid is unaffected.
        assert!(!oi_known_missing("binance", "BTC/USDT", "1d", &dir));
        // ensure_oi must return immediately, without any network attempt.
        let mut oi = vec![(1i64, 2.0f64)];
        let mut period = "stale".to_string();
        ensure_oi(&mut oi, &mut period, "binance", "BTC/USDT", "1h", &dir, true);
        assert!(oi.is_empty());
        assert!(period.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oi_cache_roundtrip() {
        let dir = std::env::temp_dir().join(format!("simccxt-oi-{}", std::process::id()));
        let path = dir.join(oi_file("binance", "BTC/USDT", "5m"));
        let rows = vec![
            (1_789_257_600_000i64, 1_234_567.5),
            (1_789_257_900_000i64, 1_300_000.0),
        ];
        save_pairs_csv(&path, &rows).expect("save");
        let back = load_pairs_csv(&path).expect("load");
        assert_eq!(back, rows);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
