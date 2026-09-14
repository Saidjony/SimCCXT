//! Data provider for simccxt.
//!
//! A unified interface over data sources (CSV, in-memory, etc.). It bridges the
//! storage and downloader layers to the exchange, so strategies and the replay
//! engine can load market data without knowing the underlying backend.

use simccxt_core::{OHLCV, Trade};
use simccxt_downloader::Downloader;
use simccxt_storage::{CsvStore, InMemoryStore, Storage};

/// The data provider: wraps a [`Storage`] backend and exposes ergonomic access.
pub struct DataProvider {
    store: Box<dyn Storage>,
}

impl DataProvider {
    /// Create a provider over an arbitrary storage backend.
    pub fn with_store(store: Box<dyn Storage>) -> Self {
        Self { store }
    }

    /// Create an in-memory provider.
    pub fn in_memory() -> Self {
        Self::with_store(Box::new(InMemoryStore::new()))
    }

    /// Create a CSV-backed provider rooted at `base_dir`.
    pub fn csv<P: AsRef<std::path::Path>>(base_dir: P) -> Self {
        Self::with_store(Box::new(CsvStore::new(base_dir)))
    }

    /// The underlying storage backend.
    pub fn store(&self) -> &dyn Storage {
        self.store.as_ref()
    }

    /// Load OHLCV candles.
    pub fn load_ohlcv(&self, symbol: &str, timeframe: &str) -> simccxt_storage::Result<Vec<OHLCV>> {
        self.store.load_ohlcv(symbol, timeframe)
    }

    /// Save OHLCV candles.
    pub fn save_ohlcv(&self, symbol: &str, timeframe: &str, candles: &[OHLCV]) -> simccxt_storage::Result<()> {
        self.store.save_ohlcv(symbol, timeframe, candles)
    }

    /// Load public trades.
    pub fn load_trades(&self, symbol: &str) -> simccxt_storage::Result<Vec<Trade>> {
        self.store.load_trades(symbol)
    }

    /// Save public trades.
    pub fn save_trades(&self, symbol: &str, trades: &[Trade]) -> simccxt_storage::Result<()> {
        self.store.save_trades(symbol, trades)
    }
}

/// Convenience: load candles and store them via a downloader's merge semantics.
pub fn load_candles_merged(
    downloader: &Downloader,
    symbol: &str,
    timeframe: &str,
    new_candles: Vec<OHLCV>,
) -> simccxt_storage::Result<Vec<OHLCV>> {
    downloader.save_candles(symbol, timeframe, new_candles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn ts(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).unwrap()
    }

    #[test]
    fn test_in_memory_provider_roundtrip() {
        let provider = DataProvider::in_memory();
        let candles = vec![OHLCV::new(ts(1_000), 1.0, 2.0, 0.5, 1.5, 10.0)];
        provider.save_ohlcv("BTC/USDT", "1m", &candles).unwrap();
        let loaded = provider.load_ohlcv("BTC/USDT", "1m").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].close, 1.5);
    }

    #[test]
    fn test_csv_provider_roundtrip() {
        let dir = std::env::temp_dir().join(format!("simccxt-dp-{}", std::process::id()));
        let provider = DataProvider::csv(&dir);
        let candles = vec![OHLCV::new(ts(1_000), 1.0, 2.0, 0.5, 1.5, 10.0)];
        provider.save_ohlcv("BTC/USDT", "1m", &candles).unwrap();
        let loaded = provider.load_ohlcv("BTC/USDT", "1m").unwrap();
        assert_eq!(loaded.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_missing_data_empty() {
        let provider = DataProvider::in_memory();
        assert!(provider.load_ohlcv("DOGE/USDT", "1m").unwrap().is_empty());
        assert!(provider.load_trades("DOGE/USDT").unwrap().is_empty());
    }
}
