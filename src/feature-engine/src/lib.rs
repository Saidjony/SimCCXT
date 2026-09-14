//! Feature engine for simccxt.
//!
//! Derives market microstructure features from trades and candles. All
//! features are deterministic pure functions (plus a small stateful engine for
//! features requiring history, such as open-interest change).

use serde::{Deserialize, Serialize};
use simccxt_core::{Side, Trade};

/// Volume-weighted average price of a set of trades.
pub fn vwap(trades: &[Trade]) -> f64 {
    let (total_notional, total_volume) = trades
        .iter()
        .fold((0.0, 0.0), |(notional, vol), t| (notional + t.price * t.qty, vol + t.qty));
    if total_volume == 0.0 {
        0.0
    } else {
        total_notional / total_volume
    }
}

/// The signed volume delta: buy volume minus sell volume.
pub fn volume_delta(trades: &[Trade]) -> f64 {
    trades.iter().fold(0.0, |acc, t| {
        let signed = if t.side == Side::Buy { t.qty } else { -t.qty };
        acc + signed
    })
}

/// The number of trades in the slice.
pub fn trade_count(trades: &[Trade]) -> usize {
    trades.len()
}

/// Trade imbalance in `[-1, 1]`: `(buy - sell) / (buy + sell)`.
pub fn trade_imbalance(trades: &[Trade]) -> f64 {
    let (mut buy, mut sell) = (0.0, 0.0);
    for t in trades {
        if t.side == Side::Buy {
            buy += t.qty;
        } else {
            sell += t.qty;
        }
    }
    let total = buy + sell;
    if total == 0.0 {
        0.0
    } else {
        (buy - sell) / total
    }
}

/// Cumulative volume delta over a chronological trade sequence.
pub fn cvd(trades: &[Trade]) -> Vec<f64> {
    let mut out = Vec::with_capacity(trades.len());
    let mut running = 0.0;
    for t in trades {
        running += if t.side == Side::Buy { t.qty } else { -t.qty };
        out.push(running);
    }
    out
}

/// Aggression ratio in `[0, 1]`: the fraction of volume that is buyer-initiated.
pub fn aggression(trades: &[Trade]) -> f64 {
    let (mut buy, mut total) = (0.0, 0.0);
    for t in trades {
        if t.side == Side::Buy {
            buy += t.qty;
        }
        total += t.qty;
    }
    if total == 0.0 {
        0.0
    } else {
        buy / total
    }
}

/// Fraction of total volume executed by trades larger than `threshold`.
///
/// A measure of whale activity in `[0, 1]`.
pub fn whale_activity(trades: &[Trade], threshold: f64) -> f64 {
    let total: f64 = trades.iter().map(|t| t.qty).sum();
    if total == 0.0 {
        return 0.0;
    }
    let large: f64 = trades.iter().filter(|t| t.qty >= threshold).map(|t| t.qty).sum();
    large / total
}

/// Open interest change between two readings.
pub fn open_interest_change(current: f64, previous: f64) -> f64 {
    current - previous
}

/// A derived set of market features for a window of trades.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MarketFeatures {
    /// Volume-weighted average price.
    pub vwap: f64,
    /// Signed volume delta.
    pub volume_delta: f64,
    /// Number of trades.
    pub trade_count: usize,
    /// Trade imbalance in [-1, 1].
    pub trade_imbalance: f64,
    /// Aggression ratio in [0, 1].
    pub aggression: f64,
    /// Whale activity ratio in [0, 1].
    pub whale_activity: f64,
}

/// The feature engine: computes features from trade windows and tracks history
/// for history-dependent features (e.g. open interest change).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeatureEngine {
    previous_open_interest: Option<f64>,
    whale_threshold: f64,
}

impl FeatureEngine {
    /// Create a feature engine.
    pub fn new() -> Self {
        Self {
            previous_open_interest: None,
            whale_threshold: 10.0,
        }
    }

    /// Set the whale-activity trade-size threshold.
    pub fn set_whale_threshold(&mut self, threshold: f64) {
        self.whale_threshold = threshold;
    }

    /// Compute a full feature set for a window of trades.
    pub fn compute(&self, trades: &[Trade]) -> MarketFeatures {
        MarketFeatures {
            vwap: vwap(trades),
            volume_delta: volume_delta(trades),
            trade_count: trade_count(trades),
            trade_imbalance: trade_imbalance(trades),
            aggression: aggression(trades),
            whale_activity: whale_activity(trades, self.whale_threshold),
        }
    }

    /// Update with a new open interest reading, returning the change since the
    /// previous reading (or `None` on the first observation).
    pub fn open_interest_change(&mut self, current: f64) -> Option<f64> {
        let change = self.previous_open_interest.map(|prev| current - prev);
        self.previous_open_interest = Some(current);
        change
    }

    /// Reset history.
    pub fn reset(&mut self) {
        self.previous_open_interest = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn ts() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1_704_067_200_000).unwrap()
    }

    fn trade(price: f64, qty: f64, side: Side) -> Trade {
        Trade::new(ts(), price, qty, side, "o1".to_string())
    }

    #[test]
    fn test_vwap() {
        let trades = vec![
            trade(100.0, 1.0, Side::Buy),
            trade(110.0, 1.0, Side::Buy),
        ];
        assert!((vwap(&trades) - 105.0).abs() < 1e-9);
    }

    #[test]
    fn test_vwap_empty() {
        assert_eq!(vwap(&[]), 0.0);
    }

    #[test]
    fn test_volume_delta() {
        let trades = vec![
            trade(100.0, 2.0, Side::Buy),
            trade(100.0, 1.0, Side::Sell),
        ];
        assert!((volume_delta(&trades) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_trade_imbalance() {
        let trades = vec![
            trade(100.0, 3.0, Side::Buy),
            trade(100.0, 1.0, Side::Sell),
        ];
        assert!((trade_imbalance(&trades) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_cvd() {
        let trades = vec![
            trade(100.0, 2.0, Side::Buy),
            trade(100.0, 1.0, Side::Sell),
            trade(100.0, 3.0, Side::Buy),
        ];
        let c = cvd(&trades);
        assert_eq!(c, vec![2.0, 1.0, 4.0]);
    }

    #[test]
    fn test_aggression() {
        let trades = vec![
            trade(100.0, 3.0, Side::Buy),
            trade(100.0, 1.0, Side::Sell),
        ];
        assert!((aggression(&trades) - 0.75).abs() < 1e-9);
    }

    #[test]
    fn test_whale_activity() {
        let trades = vec![
            trade(100.0, 100.0, Side::Buy),
            trade(100.0, 1.0, Side::Buy),
            trade(100.0, 1.0, Side::Buy),
        ];
        // 100 / 102 ≈ 0.9804
        assert!((whale_activity(&trades, 10.0) - 100.0 / 102.0).abs() < 1e-9);
    }

    #[test]
    fn test_feature_engine_compute() {
        let engine = FeatureEngine::new();
        let trades = vec![trade(100.0, 1.0, Side::Buy)];
        let features = engine.compute(&trades);
        assert_eq!(features.trade_count, 1);
        assert_eq!(features.vwap, 100.0);
    }

    #[test]
    fn test_open_interest_change() {
        let mut engine = FeatureEngine::new();
        assert!(engine.open_interest_change(100.0).is_none());
        assert!((engine.open_interest_change(120.0).unwrap() - 20.0).abs() < 1e-9);
        engine.reset();
        assert!(engine.open_interest_change(50.0).is_none());
    }
}
