//! Portfolio for simccxt.
//!
//! Tracks open positions across symbols and computes realized/unrealized PnL.
//! Supports cross and isolated margin modes and per-symbol leverage.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use simccxt_core::{Position, PositionSide, Side, Symbol};

/// Margin mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarginMode {
    /// All positions share the account's margin.
    Cross,
    /// Each position is margined independently.
    Isolated,
}

/// The portfolio: a collection of positions plus margin/leverage configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Portfolio {
    positions: HashMap<String, Position>,
    margin_mode: MarginMode,
    leverage: HashMap<String, f64>,
    default_leverage: f64,
}

impl Default for Portfolio {
    fn default() -> Self {
        Self {
            positions: HashMap::new(),
            margin_mode: MarginMode::Cross,
            leverage: HashMap::new(),
            default_leverage: 1.0,
        }
    }
}

impl Portfolio {
    /// Create an empty portfolio.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the margin mode.
    pub fn set_margin_mode(&mut self, mode: MarginMode) {
        self.margin_mode = mode;
    }

    /// Get the margin mode.
    pub fn margin_mode(&self) -> MarginMode {
        self.margin_mode
    }

    /// Set leverage for a symbol.
    pub fn set_leverage(&mut self, symbol: &str, leverage: f64) {
        assert!(leverage > 0.0, "leverage must be positive");
        self.leverage.insert(symbol.to_string(), leverage);
    }

    /// Get leverage for a symbol (falls back to the default).
    pub fn leverage(&self, symbol: &str) -> f64 {
        self.leverage.get(symbol).copied().unwrap_or(self.default_leverage)
    }

    /// Get a position by symbol.
    pub fn position(&self, symbol: &str) -> Option<&Position> {
        self.positions.get(symbol)
    }

    /// All positions.
    pub fn positions(&self) -> &HashMap<String, Position> {
        &self.positions
    }

    /// Whether there are any open positions.
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    /// Apply a fill to the portfolio, returning the realized PnL from this fill.
    pub fn apply_fill(&mut self, symbol: &Symbol, side: Side, amount: f64, price: f64) -> f64 {
        assert!(amount >= 0.0, "fill amount must be non-negative");
        if amount == 0.0 {
            return 0.0;
        }

        let realized = match side {
            Side::Buy => self.apply_buy(symbol, amount, price),
            Side::Sell => self.apply_sell(symbol, amount, price),
        };

        // Remove flat positions.
        self.positions.retain(|_, p| p.qty > 0.0);

        realized
    }

    fn apply_buy(&mut self, symbol: &Symbol, amount: f64, price: f64) -> f64 {
        let mut realized = 0.0;
        let key = symbol.symbol.clone();
        match self.positions.get_mut(&key) {
            None => {
                let pos = Position::new(symbol.clone(), PositionSide::Long, amount, price, price);
                self.positions.insert(key, pos);
            }
            Some(pos) => {
                if pos.side == PositionSide::Long {
                    // Increase long.
                    let new_qty = pos.qty + amount;
                    pos.entry_price = (pos.entry_price * pos.qty + price * amount) / new_qty;
                    pos.qty = new_qty;
                } else {
                    // Reduce or flip a short position.
                    let close_qty = amount.min(pos.qty);
                    realized += (pos.entry_price - price) * close_qty;
                    pos.qty -= close_qty;
                    pos.realized_pnl += realized;
                    let remaining = amount - close_qty;
                    if pos.qty == 0.0 && remaining > 0.0 {
                        pos.side = PositionSide::Long;
                        pos.qty = remaining;
                        pos.entry_price = price;
                    }
                }
            }
        }
        realized
    }

    fn apply_sell(&mut self, symbol: &Symbol, amount: f64, price: f64) -> f64 {
        let mut realized = 0.0;
        let key = symbol.symbol.clone();
        match self.positions.get_mut(&key) {
            None => {
                let pos = Position::new(symbol.clone(), PositionSide::Short, amount, price, price);
                self.positions.insert(key, pos);
            }
            Some(pos) => {
                if pos.side == PositionSide::Short {
                    // Increase short.
                    let new_qty = pos.qty + amount;
                    pos.entry_price = (pos.entry_price * pos.qty + price * amount) / new_qty;
                    pos.qty = new_qty;
                } else {
                    // Reduce or flip a long position.
                    let close_qty = amount.min(pos.qty);
                    realized += (price - pos.entry_price) * close_qty;
                    pos.qty -= close_qty;
                    pos.realized_pnl += realized;
                    let remaining = amount - close_qty;
                    if pos.qty == 0.0 && remaining > 0.0 {
                        pos.side = PositionSide::Short;
                        pos.qty = remaining;
                        pos.entry_price = price;
                    }
                }
            }
        }
        realized
    }

    /// Update the mark price for a symbol.
    pub fn update_mark_price(&mut self, symbol: &str, mark_price: f64) {
        if let Some(pos) = self.positions.get_mut(symbol) {
            pos.update_mark_price(mark_price);
        }
    }

    /// Total unrealized PnL across all positions.
    pub fn unrealized_pnl(&self) -> f64 {
        self.positions.values().map(|p| p.unrealized_pnl).sum()
    }

    /// Total realized PnL across all positions.
    pub fn realized_pnl(&self) -> f64 {
        self.positions.values().map(|p| p.realized_pnl).sum()
    }

    /// Positions that are currently liquidatable.
    pub fn liquidatable_positions(&self) -> Vec<&Position> {
        self.positions.values().filter(|p| p.is_liquidatable()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn btc() -> Symbol {
        Symbol::parse("BTC/USDT").unwrap()
    }

    #[test]
    fn test_open_long_position() {
        let mut p = Portfolio::new();
        let realized = p.apply_fill(&btc(), Side::Buy, 1.0, 100.0);
        assert_eq!(realized, 0.0);
        let pos = p.position("BTC/USDT").unwrap();
        assert_eq!(pos.side, PositionSide::Long);
        assert_eq!(pos.qty, 1.0);
        assert_eq!(pos.entry_price, 100.0);
    }

    #[test]
    fn test_increase_long() {
        let mut p = Portfolio::new();
        p.apply_fill(&btc(), Side::Buy, 1.0, 100.0);
        p.apply_fill(&btc(), Side::Buy, 1.0, 110.0);
        let pos = p.position("BTC/USDT").unwrap();
        assert_eq!(pos.qty, 2.0);
        assert!((pos.entry_price - 105.0).abs() < 1e-9);
    }

    #[test]
    fn test_close_long_for_profit() {
        let mut p = Portfolio::new();
        p.apply_fill(&btc(), Side::Buy, 1.0, 100.0);
        let realized = p.apply_fill(&btc(), Side::Sell, 1.0, 110.0);
        assert!((realized - 10.0).abs() < 1e-9);
        assert!(p.position("BTC/USDT").is_none());
    }

    #[test]
    fn test_short_position_profit() {
        let mut p = Portfolio::new();
        p.apply_fill(&btc(), Side::Sell, 1.0, 100.0);
        let pos = p.position("BTC/USDT").unwrap();
        assert_eq!(pos.side, PositionSide::Short);
        let realized = p.apply_fill(&btc(), Side::Buy, 1.0, 90.0);
        assert!((realized - 10.0).abs() < 1e-9);
    }

    #[test]
    fn test_flip_long_to_short() {
        let mut p = Portfolio::new();
        p.apply_fill(&btc(), Side::Buy, 1.0, 100.0);
        // Sell 2 => close 1 long (profit 10) and open 1 short at 110.
        let realized = p.apply_fill(&btc(), Side::Sell, 2.0, 110.0);
        assert!((realized - 10.0).abs() < 1e-9);
        let pos = p.position("BTC/USDT").unwrap();
        assert_eq!(pos.side, PositionSide::Short);
        assert_eq!(pos.qty, 1.0);
        assert_eq!(pos.entry_price, 110.0);
    }

    #[test]
    fn test_partial_close() {
        let mut p = Portfolio::new();
        p.apply_fill(&btc(), Side::Buy, 2.0, 100.0);
        let realized = p.apply_fill(&btc(), Side::Sell, 1.0, 110.0);
        assert!((realized - 10.0).abs() < 1e-9);
        let pos = p.position("BTC/USDT").unwrap();
        assert_eq!(pos.qty, 1.0);
        assert_eq!(pos.entry_price, 100.0);
    }

    #[test]
    fn test_unrealized_pnl() {
        let mut p = Portfolio::new();
        p.apply_fill(&btc(), Side::Buy, 1.0, 100.0);
        p.update_mark_price("BTC/USDT", 120.0);
        assert!((p.unrealized_pnl() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn test_leverage_default_and_set() {
        let mut p = Portfolio::new();
        assert_eq!(p.leverage("BTC/USDT"), 1.0);
        p.set_leverage("BTC/USDT", 10.0);
        assert_eq!(p.leverage("BTC/USDT"), 10.0);
        assert_eq!(p.leverage("ETH/USDT"), 1.0);
    }

    #[test]
    fn test_margin_mode() {
        let mut p = Portfolio::new();
        assert_eq!(p.margin_mode(), MarginMode::Cross);
        p.set_margin_mode(MarginMode::Isolated);
        assert_eq!(p.margin_mode(), MarginMode::Isolated);
    }

    #[test]
    fn test_liquidation_detection() {
        let mut p = Portfolio::new();
        p.apply_fill(&btc(), Side::Buy, 1.0, 100.0);
        p.update_mark_price("BTC/USDT", 70.0);
        let liq = p.liquidatable_positions();
        assert_eq!(liq.len(), 1);
    }
}
