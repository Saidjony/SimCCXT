//! Market data models for simccxt.
//!
//! This module provides the mutable market state that the simulator owns:
//! order books, tickers, funding rates, open interest, and candle series.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use simccxt_core::{OHLCV, OrderBookSnapshot, Symbol};

/// A single price level in the order book.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PriceLevel {
    /// Price of the level
    pub price: f64,
    /// Quantity available at this level
    pub amount: f64,
}

/// A mutable order book with sorted bid/ask levels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    /// Symbol this book belongs to
    pub symbol: Symbol,
    /// Bids, sorted descending by price (best bid first)
    pub bids: Vec<PriceLevel>,
    /// Asks, sorted ascending by price (best ask first)
    pub asks: Vec<PriceLevel>,
    /// Last update timestamp
    pub timestamp: DateTime<Utc>,
}

impl OrderBook {
    /// Create an empty order book.
    pub fn new(symbol: Symbol, timestamp: DateTime<Utc>) -> Self {
        Self {
            symbol,
            bids: Vec::new(),
            asks: Vec::new(),
            timestamp,
        }
    }

    /// Create an order book from a snapshot.
    pub fn from_snapshot(snapshot: &OrderBookSnapshot) -> Self {
        let bids = snapshot
            .bids
            .iter()
            .map(|(price, amount)| PriceLevel { price: *price, amount: *amount })
            .collect();
        let asks = snapshot
            .asks
            .iter()
            .map(|(price, amount)| PriceLevel { price: *price, amount: *amount })
            .collect();
        let mut book = Self {
            symbol: snapshot.symbol.clone(),
            bids,
            asks,
            timestamp: snapshot.timestamp,
        };
        book.sort();
        book
    }

    /// Sort bids descending and asks ascending.
    fn sort(&mut self) {
        self.bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
        self.asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
    }

    /// Get the best bid (highest bid price).
    pub fn best_bid(&self) -> Option<&PriceLevel> {
        self.bids.first()
    }

    /// Get the best ask (lowest ask price).
    pub fn best_ask(&self) -> Option<&PriceLevel> {
        self.asks.first()
    }

    /// Get the mid price, or `None` if either side is empty.
    pub fn mid_price(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some((bid.price + ask.price) / 2.0),
            _ => None,
        }
    }

    /// Get the spread (ask - bid), or `None` if either side is empty.
    pub fn spread(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some(ask.price - bid.price),
            _ => None,
        }
    }

    /// Apply a bid update: replace the amount at `price`, removing the level if amount is zero.
    pub fn update_bid(&mut self, price: f64, amount: f64) {
        Self::update_level(&mut self.bids, price, amount, true);
    }

    /// Apply an ask update: replace the amount at `price`, removing the level if amount is zero.
    pub fn update_ask(&mut self, price: f64, amount: f64) {
        Self::update_level(&mut self.asks, price, amount, false);
    }

    fn update_level(levels: &mut Vec<PriceLevel>, price: f64, amount: f64, is_bid: bool) {
        if let Some(pos) = levels.iter().position(|l| l.price == price) {
            if amount == 0.0 {
                levels.remove(pos);
            } else {
                levels[pos].amount = amount;
            }
        } else if amount != 0.0 {
            levels.push(PriceLevel { price, amount });
        }
        if is_bid {
            levels.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
        } else {
            levels.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
        }
    }

    /// Consume liquidity from the book, returning the executed fills.
    ///
    /// For a buy order (side = Buy), walks up the asks. For a sell order, walks down the bids.
    /// Returns a list of `(price, filled_amount)` and the remaining unfilled amount.
    pub fn consume(
        &mut self,
        side: simccxt_core::Side,
        amount: f64,
    ) -> (Vec<(f64, f64)>, f64) {
        use simccxt_core::Side;
        let mut fills = Vec::new();
        let mut remaining = amount;

        let levels = match side {
            Side::Buy => &mut self.asks,
            Side::Sell => &mut self.bids,
        };

        for level in levels.iter_mut() {
            if remaining <= 0.0 {
                break;
            }
            if level.amount == 0.0 {
                continue;
            }
            let fill = remaining.min(level.amount);
            fills.push((level.price, fill));
            level.amount -= fill;
            remaining -= fill;
        }

        // Remove depleted levels.
        let levels = match side {
            Side::Buy => &mut self.asks,
            Side::Sell => &mut self.bids,
        };
        levels.retain(|l| l.amount > 0.0);

        (fills, remaining)
    }
}

/// A ticker with the latest price and 24h statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticker {
    /// Symbol
    pub symbol: Symbol,
    /// Last traded price
    pub last: f64,
    /// Bid price
    pub bid: f64,
    /// Ask price
    pub ask: f64,
    /// 24h high
    pub high: f64,
    /// 24h low
    pub low: f64,
    /// 24h volume in base asset
    pub base_volume: f64,
    /// 24h volume in quote asset
    pub quote_volume: f64,
    /// 24h price change percentage
    pub percentage: f64,
    /// Timestamp
    pub timestamp: DateTime<Utc>,
}

impl Ticker {
    /// Create a ticker.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        symbol: Symbol,
        last: f64,
        bid: f64,
        ask: f64,
        high: f64,
        low: f64,
        base_volume: f64,
        quote_volume: f64,
        timestamp: DateTime<Utc>,
    ) -> Self {
        let percentage = if high > 0.0 { ((last - low) / low) * 100.0 } else { 0.0 };
        Self {
            symbol,
            last,
            bid,
            ask,
            high,
            low,
            base_volume,
            quote_volume,
            percentage,
            timestamp,
        }
    }
}

/// Funding rate for a perpetual contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FundingRate {
    /// Symbol
    pub symbol: Symbol,
    /// Funding rate (fractional, e.g. 0.0001 = 0.01%)
    pub funding_rate: f64,
    /// Funding timestamp
    pub timestamp: DateTime<Utc>,
    /// Funding interval in milliseconds
    pub interval_ms: i64,
}

impl FundingRate {
    /// Create a funding rate.
    pub fn new(symbol: Symbol, funding_rate: f64, timestamp: DateTime<Utc>, interval_ms: i64) -> Self {
        Self { symbol, funding_rate, timestamp, interval_ms }
    }
}

/// Open interest for a derivatives contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenInterest {
    /// Symbol
    pub symbol: Symbol,
    /// Open interest in base asset
    pub open_interest: f64,
    /// Timestamp
    pub timestamp: DateTime<Utc>,
}

impl OpenInterest {
    /// Create an open interest record.
    pub fn new(symbol: Symbol, open_interest: f64, timestamp: DateTime<Utc>) -> Self {
        Self { symbol, open_interest, timestamp }
    }
}

/// A chronological series of OHLCV candles.
#[derive(Debug, Clone, Default)]
pub struct CandleSeries {
    /// Candles sorted ascending by timestamp
    pub candles: Vec<OHLCV>,
}

impl CandleSeries {
    /// Create an empty series.
    pub fn new() -> Self {
        Self { candles: Vec::new() }
    }

    /// Push a candle, maintaining chronological order.
    pub fn push(&mut self, candle: OHLCV) {
        if let Some(last) = self.candles.last() {
            assert!(
                candle.timestamp >= last.timestamp,
                "candles must be appended in chronological order"
            );
        }
        self.candles.push(candle);
    }

    /// Get the number of candles.
    pub fn len(&self) -> usize {
        self.candles.len()
    }

    /// Whether the series is empty.
    pub fn is_empty(&self) -> bool {
        self.candles.is_empty()
    }

    /// Get the last `n` candles.
    pub fn last_n(&self, n: usize) -> &[OHLCV] {
        let start = self.candles.len().saturating_sub(n);
        &self.candles[start..]
    }

    /// Iterate over candles.
    pub fn iter(&self) -> std::slice::Iter<'_, OHLCV> {
        self.candles.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simccxt_core::Side;

    fn ts() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1_704_067_200_000).unwrap()
    }

    fn btc() -> Symbol {
        Symbol::parse("BTC/USDT").unwrap()
    }

    #[test]
    fn test_order_book_sorting() {
        let snapshot = OrderBookSnapshot::new(
            btc(),
            vec![(100.0, 1.0), (101.0, 2.0), (99.0, 3.0)],
            vec![(102.0, 1.0), (104.0, 2.0), (103.0, 3.0)],
            ts(),
        );
        let book = OrderBook::from_snapshot(&snapshot);
        assert_eq!(book.best_bid().unwrap().price, 101.0);
        assert_eq!(book.best_ask().unwrap().price, 102.0);
    }

    #[test]
    fn test_mid_price_and_spread() {
        let snapshot = OrderBookSnapshot::new(
            btc(),
            vec![(100.0, 1.0)],
            vec![(102.0, 1.0)],
            ts(),
        );
        let book = OrderBook::from_snapshot(&snapshot);
        assert_eq!(book.mid_price(), Some(101.0));
        assert_eq!(book.spread(), Some(2.0));
    }

    #[test]
    fn test_mid_price_empty() {
        let book = OrderBook::new(btc(), ts());
        assert_eq!(book.mid_price(), None);
        assert_eq!(book.spread(), None);
    }

    #[test]
    fn test_update_bid_level() {
        let mut book = OrderBook::new(btc(), ts());
        book.update_bid(100.0, 5.0);
        assert_eq!(book.best_bid().unwrap().amount, 5.0);
        book.update_bid(100.0, 3.0);
        assert_eq!(book.best_bid().unwrap().amount, 3.0);
        book.update_bid(100.0, 0.0);
        assert!(book.bids.is_empty());
    }

    #[test]
    fn test_consume_buy_walks_asks() {
        let mut book = OrderBook::new(btc(), ts());
        book.update_ask(100.0, 1.0);
        book.update_ask(101.0, 2.0);
        let (fills, remaining) = book.consume(Side::Buy, 2.5);
        assert_eq!(fills, vec![(100.0, 1.0), (101.0, 1.5)]);
        assert!((remaining - 0.0).abs() < 1e-9);
        assert_eq!(book.best_ask().unwrap().amount, 0.5);
    }

    #[test]
    fn test_consume_sell_walks_bids() {
        let mut book = OrderBook::new(btc(), ts());
        book.update_bid(100.0, 2.0);
        book.update_bid(99.0, 2.0);
        let (fills, remaining) = book.consume(Side::Sell, 3.0);
        assert_eq!(fills, vec![(100.0, 2.0), (99.0, 1.0)]);
        assert!((remaining - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_consume_partial_fill() {
        let mut book = OrderBook::new(btc(), ts());
        book.update_ask(100.0, 1.0);
        let (fills, remaining) = book.consume(Side::Buy, 5.0);
        assert_eq!(fills, vec![(100.0, 1.0)]);
        assert!((remaining - 4.0).abs() < 1e-9);
        assert!(book.asks.is_empty());
    }

    #[test]
    fn test_ticker() {
        let t = Ticker::new(btc(), 105.0, 104.0, 106.0, 110.0, 100.0, 50.0, 5250.0, ts());
        assert_eq!(t.last, 105.0);
        assert_eq!(t.high, 110.0);
        assert!((t.percentage - 5.0).abs() < 1e-9);
    }

    #[test]
    fn test_funding_rate() {
        let f = FundingRate::new(btc(), 0.0001, ts(), 8 * 60 * 60 * 1000);
        assert_eq!(f.funding_rate, 0.0001);
        assert_eq!(f.interval_ms, 28_800_000);
    }

    #[test]
    fn test_open_interest() {
        let oi = OpenInterest::new(btc(), 1234.5, ts());
        assert_eq!(oi.open_interest, 1234.5);
    }

    #[test]
    fn test_candle_series_chronological() {
        let mut series = CandleSeries::new();
        series.push(OHLCV::new(ts(), 1.0, 2.0, 0.5, 1.5, 10.0));
        series.push(OHLCV::new(ts() + chrono::Duration::minutes(1), 1.5, 2.5, 1.0, 2.0, 20.0));
        assert_eq!(series.len(), 2);
        assert_eq!(series.last_n(1)[0].close, 2.0);
    }

    #[test]
    #[should_panic]
    fn test_candle_series_rejects_out_of_order() {
        let mut series = CandleSeries::new();
        series.push(OHLCV::new(ts() + chrono::Duration::minutes(1), 1.0, 2.0, 0.5, 1.5, 10.0));
        series.push(OHLCV::new(ts(), 1.0, 2.0, 0.5, 1.5, 10.0));
    }
}
