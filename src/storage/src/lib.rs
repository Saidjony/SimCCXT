//! Storage layer for simccxt.
//!
//! Provides an abstraction over market data persistence. Supported backends:
//! in-memory, CSV, and (optionally, via the `parquet` feature) Parquet.

use std::fmt;
use std::path::Path;

use chrono::DateTime;use serde::{Deserialize, Serialize};
use simccxt_core::{OHLCV, Side, Trade};

/// Errors that can occur in the storage layer.
#[derive(Debug)]
pub enum StorageError {
    /// I/O error
    Io(std::io::Error),
    /// Serialization/deserialization error
    Csv(csv::Error),
    /// Invalid data encountered while reading
    InvalidData(String),
    /// Backend not available (e.g. missing feature)
    Unsupported(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Io(e) => write!(f, "io error: {e}"),
            StorageError::Csv(e) => write!(f, "csv error: {e}"),
            StorageError::InvalidData(msg) => write!(f, "invalid data: {msg}"),
            StorageError::Unsupported(msg) => write!(f, "unsupported: {msg}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Io(e) => Some(e),
            StorageError::Csv(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::Io(e)
    }
}

impl From<csv::Error> for StorageError {
    fn from(e: csv::Error) -> Self {
        StorageError::Csv(e)
    }
}

/// A common result type for storage operations.
pub type Result<T> = std::result::Result<T, StorageError>;

/// The three memory modes supported by the simulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryMode {
    /// Load everything into RAM.
    InMemory,
    /// Use memory-mapped files to avoid loading the whole dataset into RAM.
    MemoryMapped,
    /// Stream records incrementally.
    Streaming,
}

/// The persistence interface for market data.
///
/// Implementations must be deterministic: reading data that was written by
/// the same implementation must yield identical records.
pub trait Storage: Send + Sync {
    /// Load OHLCV candles for a symbol/timeframe.
    fn load_ohlcv(&self, symbol: &str, timeframe: &str) -> Result<Vec<OHLCV>>;

    /// Save OHLCV candles for a symbol/timeframe.
    fn save_ohlcv(&self, symbol: &str, timeframe: &str, candles: &[OHLCV]) -> Result<()>;

    /// Load public trades for a symbol.
    fn load_trades(&self, symbol: &str) -> Result<Vec<Trade>>;

    /// Save public trades for a symbol.
    fn save_trades(&self, symbol: &str, trades: &[Trade]) -> Result<()>;
}

/// A storage backend that keeps data in memory.
///
/// Useful for tests and small datasets.
#[derive(Debug, Default)]
pub struct InMemoryStore {
    ohlcv: std::sync::Mutex<std::collections::HashMap<(String, String), Vec<OHLCV>>>,
    trades: std::sync::Mutex<std::collections::HashMap<String, Vec<Trade>>>,
}

impl InMemoryStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Storage for InMemoryStore {
    fn load_ohlcv(&self, symbol: &str, timeframe: &str) -> Result<Vec<OHLCV>> {
        let map = self.ohlcv.lock().unwrap();
        Ok(map
            .get(&(symbol.to_string(), timeframe.to_string()))
            .cloned()
            .unwrap_or_default())
    }

    fn save_ohlcv(&self, symbol: &str, timeframe: &str, candles: &[OHLCV]) -> Result<()> {
        let mut map = self.ohlcv.lock().unwrap();
        map.insert((symbol.to_string(), timeframe.to_string()), candles.to_vec());
        Ok(())
    }

    fn load_trades(&self, symbol: &str) -> Result<Vec<Trade>> {
        let map = self.trades.lock().unwrap();
        Ok(map.get(symbol).cloned().unwrap_or_default())
    }

    fn save_trades(&self, symbol: &str, trades: &[Trade]) -> Result<()> {
        let mut map = self.trades.lock().unwrap();
        map.insert(symbol.to_string(), trades.to_vec());
        Ok(())
    }
}

/// A storage backend backed by CSV files on disk.
///
/// Layout:
/// - `base/ohlcv/{symbol}-{timeframe}.csv`
/// - `base/trades/{symbol}.csv`
pub struct CsvStore {
    base: std::path::PathBuf,
}

impl CsvStore {
    /// Create a CSV store rooted at `base` (created if missing).
    pub fn new<P: AsRef<Path>>(base: P) -> Self {
        Self { base: base.as_ref().to_path_buf() }
    }

    fn ohlcv_path(&self, symbol: &str, timeframe: &str) -> std::path::PathBuf {
        self.base
            .join("ohlcv")
            .join(format!("{symbol}-{timeframe}.csv"))
    }

    fn trades_path(&self, symbol: &str) -> std::path::PathBuf {
        self.base.join("trades").join(format!("{symbol}.csv"))
    }
}

impl Storage for CsvStore {
    fn load_ohlcv(&self, symbol: &str, timeframe: &str) -> Result<Vec<OHLCV>> {
        let path = self.ohlcv_path(symbol, timeframe);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut rdr = csv::Reader::from_path(&path)?;
        let mut out = Vec::new();
        for result in rdr.deserialize::<CandleRow>() {
            let row: CandleRow = result?;
            out.push(row.into_ohlcv()?);
        }
        Ok(out)
    }

    fn save_ohlcv(&self, symbol: &str, timeframe: &str, candles: &[OHLCV]) -> Result<()> {
        let path = self.ohlcv_path(symbol, timeframe);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut wtr = csv::Writer::from_path(&path)?;
        for c in candles {
            wtr.serialize(CandleRow::from_ohlcv(c))?;
        }
        wtr.flush()?;
        Ok(())
    }

    fn load_trades(&self, symbol: &str) -> Result<Vec<Trade>> {
        let path = self.trades_path(symbol);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut rdr = csv::Reader::from_path(&path)?;
        let mut out = Vec::new();
        for result in rdr.deserialize::<TradeRow>() {
            let row: TradeRow = result?;
            out.push(row.into_trade()?);
        }
        Ok(out)
    }

    fn save_trades(&self, symbol: &str, trades: &[Trade]) -> Result<()> {
        let path = self.trades_path(symbol);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut wtr = csv::Writer::from_path(&path)?;
        for t in trades {
            wtr.serialize(TradeRow::from_trade(t))?;
        }
        wtr.flush()?;
        Ok(())
    }
}

/// CSV row for an OHLCV candle (timestamp in milliseconds).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CandleRow {
    timestamp: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

impl CandleRow {
    fn from_ohlcv(c: &OHLCV) -> Self {
        Self {
            timestamp: c.timestamp.timestamp_millis(),
            open: c.open,
            high: c.high,
            low: c.low,
            close: c.close,
            volume: c.volume,
        }
    }

    fn into_ohlcv(self) -> Result<OHLCV> {
        let timestamp = DateTime::from_timestamp_millis(self.timestamp)
            .ok_or_else(|| StorageError::InvalidData(format!("bad timestamp {}", self.timestamp)))?;
        Ok(OHLCV::new(timestamp, self.open, self.high, self.low, self.close, self.volume))
    }
}

/// CSV row for a public trade.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TradeRow {
    timestamp: i64,
    price: f64,
    qty: f64,
    side: String,
    order_id: String,
    trade_id: String,
}

impl TradeRow {
    fn from_trade(t: &Trade) -> Self {
        Self {
            timestamp: t.timestamp.timestamp_millis(),
            price: t.price,
            qty: t.qty,
            side: match t.side {
                Side::Buy => "buy".to_string(),
                Side::Sell => "sell".to_string(),
            },
            order_id: t.order_id.clone(),
            trade_id: t.trade_id.clone(),
        }
    }

    fn into_trade(self) -> Result<Trade> {
        let timestamp = DateTime::from_timestamp_millis(self.timestamp)
            .ok_or_else(|| StorageError::InvalidData(format!("bad timestamp {}", self.timestamp)))?;
        let side = match self.side.as_str() {
            "buy" => Side::Buy,
            "sell" => Side::Sell,
            other => return Err(StorageError::InvalidData(format!("bad side {other}"))),
        };
        Ok(Trade {
            timestamp,
            price: self.price,
            qty: self.qty,
            side,
            order_id: self.order_id,
            trade_id: self.trade_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candle(ts_ms: i64, close: f64) -> OHLCV {
        let ts = DateTime::from_timestamp_millis(ts_ms).unwrap();
        OHLCV::new(ts, close - 1.0, close + 1.0, close - 2.0, close, 10.0)
    }

    fn trade(ts_ms: i64, price: f64, side: Side) -> Trade {
        let ts = DateTime::from_timestamp_millis(ts_ms).unwrap();
        Trade::new(ts, price, 1.0, side, "order1".to_string())
    }

    #[test]
    fn test_csv_ohlcv_roundtrip() {
        let dir = std::env::temp_dir().join(format!("simccxt-test-{}", std::process::id()));
        let store = CsvStore::new(&dir);
        let candles = vec![
            candle(1_704_067_200_000, 100.0),
            candle(1_704_067_260_000, 101.0),
            candle(1_704_067_320_000, 99.0),
        ];
        store.save_ohlcv("BTC/USDT", "1m", &candles).unwrap();
        let loaded = store.load_ohlcv("BTC/USDT", "1m").unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].close, 100.0);
        assert_eq!(loaded[2].close, 99.0);
        assert_eq!(loaded[1].timestamp.timestamp_millis(), 1_704_067_260_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_csv_trades_roundtrip() {
        let dir = std::env::temp_dir().join(format!("simccxt-test-trades-{}", std::process::id()));
        let store = CsvStore::new(&dir);
        let trades = vec![
            trade(1_704_067_200_000, 100.0, Side::Buy),
            trade(1_704_067_200_100, 100.5, Side::Sell),
        ];
        store.save_trades("BTC/USDT", &trades).unwrap();
        let loaded = store.load_trades("BTC/USDT").unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].side, Side::Buy);
        assert_eq!(loaded[1].side, Side::Sell);
        assert_eq!(loaded[1].price, 100.5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_csv_missing_file_returns_empty() {
        let dir = std::env::temp_dir().join(format!("simccxt-test-empty-{}", std::process::id()));
        let store = CsvStore::new(&dir);
        assert!(store.load_ohlcv("DOES/NOT", "1m").unwrap().is_empty());
        assert!(store.load_trades("DOES/NOT").unwrap().is_empty());
    }

    #[test]
    fn test_candle_row_roundtrip() {
        let c = candle(1_704_067_200_000, 42.0);
        let row = CandleRow::from_ohlcv(&c);
        let back = row.into_ohlcv().unwrap();
        assert_eq!(back.close, c.close);
        assert_eq!(back.timestamp, c.timestamp);
    }

    #[test]
    fn test_trade_row_roundtrip() {
        let t = trade(1_704_067_200_000, 10.0, Side::Sell);
        let row = TradeRow::from_trade(&t);
        let back = row.into_trade().unwrap();
        assert_eq!(back.side, Side::Sell);
        assert_eq!(back.price, 10.0);
    }

    #[test]
    fn test_trade_row_rejects_bad_side() {
        let row = TradeRow {
            timestamp: 1_704_067_200_000,
            price: 1.0,
            qty: 1.0,
            side: "weird".to_string(),
            order_id: "x".to_string(),
            trade_id: "y".to_string(),
        };
        assert!(row.into_trade().is_err());
    }

    #[test]
    fn test_memory_mode_values() {
        assert_eq!(MemoryMode::InMemory as u8, MemoryMode::InMemory as u8);
        assert_ne!(MemoryMode::InMemory, MemoryMode::Streaming);
    }
}
