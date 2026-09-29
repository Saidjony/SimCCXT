use std::path::{Path, PathBuf};

use crate::stamp;
use chrono::{DateTime, NaiveDateTime, Utc};
use ccxt::{from_id, TypedExchange, Value};
use simccxt_broker::MarketKind;
use simccxt_core::{Symbol, OHLCV};
use simccxt_exchange::Exchange;

// ---------------------------------------------------------------------------
// Market-data loading (memory → exchange-scoped disk cache → CCXT → synthetic)
// ---------------------------------------------------------------------------

/// How many klines to pull when a series is missing from cache and memory.
const DOWNLOAD_BARS: usize = 5_000;
const OHLCV_PAGE: usize = 1_000;
/// How many open-interest points to keep (coarser grid than klines).
const OI_POINTS: usize = 1_500;
const MAX_OI_HISTORY_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
const OI_PAGE: usize = 500;

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

fn ccxt_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start CCXT runtime: {err}"))
}

fn ccxt_exchange(exchange: &str) -> Result<Box<dyn TypedExchange>, String> {
    from_id(exchange, None).ok_or_else(|| format!("CCXT exchange '{exchange}' is not enabled"))
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
    if !["1s", "1m", "5m", "15m", "1h", "4h", "1d"].contains(&tf) {
        return Err(format!("unsupported timeframe {tf}"));
    }
    let step = tf_step_ms(tf) as i64;
    let end_ms = Utc::now().timestamp_millis();
    let mut cursor = end_ms - (bars as i64) * step;
    let mut exchange = ccxt_exchange(exchange_id)?;
    let runtime = ccxt_runtime()?;
    runtime.block_on(async {
        exchange
            .call_raw("load_markets", vec![Value::Bool(false), Value::Null])
            .await
            .map_err(|err| format!("{exchange_id}: load markets failed: {err}"))?;
        let mut candles = Vec::with_capacity(bars);
        while cursor <= end_ms && candles.len() < bars {
            let limit = (bars - candles.len()).min(OHLCV_PAGE);
            let page = exchange
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
                candles.push(OHLCV::new(dt, open, high, low, close, volume));
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

/// OI history grid per chart timeframe (use 5m for the 1m candle interval).
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
    let mut exchange = ccxt_exchange(exchange_id)?;
    let runtime = ccxt_runtime()?;
    runtime.block_on(async {
        exchange
            .call_raw("load_markets", vec![Value::Bool(false), Value::Null])
            .await
            .map_err(|err| format!("{exchange_id}: load markets failed: {err}"))?;
        let mut out = Vec::with_capacity(points);
        while cursor < end_ms && out.len() < points {
            let limit = (points - out.len()).min(OI_PAGE);
            let rows = exchange
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
    let start = 1_704_067_200_000i64; // 2024-01-01T00:00:00Z
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
            Err(e) => log!("[engine] {exchange} {symbol} OI@{period}: download failed: {e}"),
        }
    }
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
    let path = data_dir.join(series_file(exchange, symbol, tf));
    if let Some(candles) = load_csv(&path) {
        let n = candles.len();
        if let Ok(sym) = Symbol::parse(symbol) {
            ex.add_market(&sym, MarketKind::Spot);
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
                    ex.add_market(&sym, MarketKind::Spot);
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
        ex.add_market(&sym, MarketKind::Spot);
    }
    ex.load_candles(symbol, tf, candles);
    log!(
        "[engine] {exchange} {symbol} {tf}: 2000 synthetic bars (offline; no cache at {})",
        path.display()
    );
    Ok(())
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
