use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::stamp;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;
use simccxt_broker::MarketKind;
use simccxt_core::{Symbol, OHLCV};
use simccxt_exchange::Exchange;

// ---------------------------------------------------------------------------
// Market-data loading (memory → disk cache → Binance → synthetic)
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
    let body = res
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("{host}: {e}"))?;
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
    let body = res
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("fapi: {e}"))?;
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
    format!(
        "{}_oi_{}.csv",
        symbol.replace('/', "").to_uppercase(),
        period
    )
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
/// session: disk cache → Binance download (saved to cache) → empty + log.
/// OI is chart context, never replayed: no synthetic fallback (an invented
/// OI curve would be worse than none).
pub(super) fn ensure_oi(
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
        log!(
            "[engine] {symbol} OI@{period}: {} pts from cache ({})",
            rows.len(),
            path.display()
        );
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
pub(super) fn ensure_series(
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
        log!(
            "[engine] {symbol} {tf}: {n} bars from cache ({})",
            path.display()
        );
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
    log!(
        "[engine] {symbol} {tf}: 2000 synthetic bars (no cache at {})",
        path.display()
    );
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
    fn binance_symbol_from_pair() {
        assert_eq!(to_binance_symbol("DOGE/USDT").as_deref(), Some("DOGEUSDT"));
        assert_eq!(to_binance_symbol("btc/usdt").as_deref(), Some("BTCUSDT"));
        assert_eq!(to_binance_symbol("BTCUSDT"), None);
        assert_eq!(to_binance_symbol("/USDT"), None);
    }

    #[test]
    fn kline_row_parses_ohlcv() {
        let row = serde_json::json!([
            1789257600000i64,
            "77278.73",
            "77282.04",
            "77266.01",
            "77282.04",
            "1.71117",
            1789257659999i64,
            "132440.5",
            42,
            "0",
            "0",
            "0"
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
