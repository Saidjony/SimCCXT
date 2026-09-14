//! Downloader for simccxt.
//!
//! Downloads and normalizes market data from exchanges (Binance, Bybit, OKX),
//! stores it locally, deduplicates, and supports resume via cached ranges.

use std::collections::BTreeSet;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use simccxt_core::OHLCV;
use simccxt_storage::{CsvStore, Storage};

/// Supported exchange sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExchangeId {
    /// Binance.
    Binance,
    /// Bybit.
    Bybit,
    /// OKX.
    Okx,
}

impl ExchangeId {
    /// Parse an exchange id from a string.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "binance" => Some(ExchangeId::Binance),
            "bybit" => Some(ExchangeId::Bybit),
            "okx" | "okex" => Some(ExchangeId::Okx),
            _ => None,
        }
    }
}

/// Normalize an exchange-specific symbol into the CCXT `BASE/QUOTE` format.
pub fn normalize_symbol(raw: &str, exchange: ExchangeId) -> String {
    match exchange {
        ExchangeId::Binance | ExchangeId::Bybit => {
            // "BTCUSDT" -> "BTC/USDT". Try to split on common quote assets.
            for quote in ["USDT", "USDC", "USD", "BTC", "ETH", "BUSD"] {
                if let Some(base) = raw.strip_suffix(quote) {
                    if !base.is_empty() {
                        return format!("{base}/{quote}");
                    }
                }
            }
            raw.to_string()
        }
        ExchangeId::Okx => {
            // "BTC-USDT" -> "BTC/USDT"
            raw.replace('-', "/")
        }
    }
}

/// Normalize a raw OHLCV row `[timestamp, open, high, low, close, volume]`.
///
/// `timestamp_ms` indicates whether the timestamp is in milliseconds or seconds.
pub fn normalize_ohlcv(row: &[f64; 6], timestamp_ms: bool) -> Option<OHLCV> {
    let ts_ms = if timestamp_ms {
        row[0] as i64
    } else {
        (row[0] as i64) * 1000
    };
    let timestamp = DateTime::from_timestamp_millis(ts_ms)?;
    Some(OHLCV::new(timestamp, row[1], row[2], row[3], row[4], row[5]))
}

/// Deduplicate and sort a candle series by timestamp (ascending).
pub fn deduplicate(candles: Vec<OHLCV>) -> Vec<OHLCV> {
    let mut seen = BTreeSet::new();
    let mut out: Vec<OHLCV> = candles
        .into_iter()
        .filter(|c| seen.insert(c.timestamp.timestamp_millis()))
        .collect();
    out.sort_by_key(|c| c.timestamp);
    out
}

/// Merge two candle series into one, sorted and deduplicated.
pub fn merge(a: Vec<OHLCV>, b: Vec<OHLCV>) -> Vec<OHLCV> {
    let mut combined = a;
    combined.extend(b);
    deduplicate(combined)
}

/// A local downloader/cache for market data.
///
/// Stores data as CSV under a base directory and merges new downloads with any
/// existing data (deduplicating by timestamp), enabling resume without
/// re-downloading overlapping ranges.
pub struct Downloader {
    store: CsvStore,
}

impl Downloader {
    /// Create a downloader rooted at `base_dir`.
    pub fn new<P: AsRef<Path>>(base_dir: P) -> Self {
        Self { store: CsvStore::new(base_dir) }
    }

    /// Whether candles already exist for a symbol/timeframe.
    pub fn has_candles(&self, symbol: &str, timeframe: &str) -> bool {
        match self.store.load_ohlcv(symbol, timeframe) {
            Ok(candles) => !candles.is_empty(),
            Err(_) => false,
        }
    }

    /// Load cached candles for a symbol/timeframe.
    pub fn load_candles(&self, symbol: &str, timeframe: &str) -> simccxt_storage::Result<Vec<OHLCV>> {
        self.store.load_ohlcv(symbol, timeframe)
    }

    /// Save (merge) newly downloaded candles with any existing data.
    ///
    /// The result is sorted and deduplicated, so overlapping downloads are safe.
    pub fn save_candles(
        &self,
        symbol: &str,
        timeframe: &str,
        new_candles: Vec<OHLCV>,
    ) -> simccxt_storage::Result<Vec<OHLCV>> {
        let existing = self.store.load_ohlcv(symbol, timeframe)?;
        let merged = merge(existing, new_candles);
        self.store.save_ohlcv(symbol, timeframe, &merged)?;
        Ok(merged)
    }

    /// Compute the contiguous time range covered by cached candles.
    pub fn covered_range(&self, symbol: &str, timeframe: &str) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let candles = self.load_candles(symbol, timeframe).ok()?;
        let first = candles.first()?.timestamp;
        let last = candles.last()?.timestamp;
        Some((first, last))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candle(ms: i64, close: f64) -> OHLCV {
        OHLCV::new(DateTime::from_timestamp_millis(ms).unwrap(), close - 1.0, close + 1.0, close - 2.0, close, 1.0)
    }

    #[test]
    fn test_normalize_symbol_binance() {
        assert_eq!(normalize_symbol("BTCUSDT", ExchangeId::Binance), "BTC/USDT");
        assert_eq!(normalize_symbol("ETHUSDC", ExchangeId::Bybit), "ETH/USDC");
    }

    #[test]
    fn test_normalize_symbol_okx() {
        assert_eq!(normalize_symbol("BTC-USDT", ExchangeId::Okx), "BTC/USDT");
    }

    #[test]
    fn test_exchange_parse() {
        assert_eq!(ExchangeId::parse("binance"), Some(ExchangeId::Binance));
        assert_eq!(ExchangeId::parse("OKX"), Some(ExchangeId::Okx));
        assert_eq!(ExchangeId::parse("unknown"), None);
    }

    #[test]
    fn test_normalize_ohlcv_ms() {
        let row = [1_704_067_200_000.0, 1.0, 2.0, 0.5, 1.5, 100.0];
        let c = normalize_ohlcv(&row, true).unwrap();
        assert_eq!(c.timestamp.timestamp_millis(), 1_704_067_200_000);
        assert_eq!(c.close, 1.5);
    }

    #[test]
    fn test_normalize_ohlcv_seconds() {
        let row = [1_704_067_200.0, 1.0, 2.0, 0.5, 1.5, 100.0];
        let c = normalize_ohlcv(&row, false).unwrap();
        assert_eq!(c.timestamp.timestamp_millis(), 1_704_067_200_000);
    }

    #[test]
    fn test_deduplicate() {
        let candles = vec![candle(3_000, 3.0), candle(1_000, 1.0), candle(3_000, 9.0), candle(2_000, 2.0)];
        let deduped = deduplicate(candles);
        assert_eq!(deduped.len(), 3);
        assert_eq!(deduped[0].timestamp.timestamp_millis(), 1_000);
        assert_eq!(deduped[2].timestamp.timestamp_millis(), 3_000);
    }

    #[test]
    fn test_merge() {
        let a = vec![candle(1_000, 1.0), candle(2_000, 2.0)];
        let b = vec![candle(2_000, 9.0), candle(3_000, 3.0)];
        let merged = merge(a, b);
        assert_eq!(merged.len(), 3);
        // The deduplication keeps the first occurrence of the overlapping candle.
        assert_eq!(merged[1].close, 2.0);
    }

    #[test]
    fn test_downloader_save_merge_and_resume() {
        let dir = std::env::temp_dir().join(format!("simccxt-dl-{}", std::process::id()));
        let dl = Downloader::new(&dir);
        assert!(!dl.has_candles("BTC/USDT", "1m"));

        dl.save_candles("BTC/USDT", "1m", vec![candle(1_000, 1.0), candle(2_000, 2.0)]).unwrap();
        assert!(dl.has_candles("BTC/USDT", "1m"));

        // Resume: download an overlapping + new range.
        dl.save_candles("BTC/USDT", "1m", vec![candle(2_000, 2.0), candle(3_000, 3.0)]).unwrap();

        let all = dl.load_candles("BTC/USDT", "1m").unwrap();
        assert_eq!(all.len(), 3);

        let range = dl.covered_range("BTC/USDT", "1m").unwrap();
        assert_eq!(range.0.timestamp_millis(), 1_000);
        assert_eq!(range.1.timestamp_millis(), 3_000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
