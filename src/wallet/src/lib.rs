//! Wallet for simccxt.
//!
//! Tracks account balances, realized PnL, fees, and funding across all
//! assets. The wallet is the source of truth for available vs. locked funds.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use simccxt_core::Balance;

/// The wallet: a collection of per-asset balances plus aggregate PnL/fee stats.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Wallet {
    balances: HashMap<String, Balance>,
    realized_pnl: f64,
    total_fees: f64,
    total_funding: f64,
}

impl Wallet {
    /// Create an empty wallet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a wallet with an initial set of balances.
    pub fn from_balances(balances: Vec<Balance>) -> Self {
        let mut wallet = Self::new();
        for b in balances {
            wallet.balances.insert(b.asset.clone(), b);
        }
        wallet
    }

    /// Deposit an amount of an asset, creating the balance if needed.
    pub fn deposit(&mut self, asset: &str, amount: f64) {
        assert!(amount >= 0.0, "deposit amount must be non-negative");
        let balance = self
            .balances
            .entry(asset.to_string())
            .or_insert_with(|| Balance::new(asset, 0.0, 0.0));
        balance.total += amount;
        balance.available += amount;
    }

    /// Withdraw an amount of an asset.
    ///
    /// Returns `Err(amount)` if the available balance is insufficient.
    pub fn withdraw(&mut self, asset: &str, amount: f64) -> Result<(), f64> {
        assert!(amount >= 0.0, "withdraw amount must be non-negative");
        let balance = self
            .balances
            .get_mut(asset)
            .ok_or(amount)?;
        if balance.available < amount {
            return Err(amount);
        }
        balance.total -= amount;
        balance.available -= amount;
        Ok(())
    }

    /// Get the balance for an asset, if present.
    pub fn balance(&self, asset: &str) -> Option<&Balance> {
        self.balances.get(asset)
    }

    /// Get a mutable balance for an asset, if present.
    pub fn balance_mut(&mut self, asset: &str) -> Option<&mut Balance> {
        self.balances.get_mut(asset)
    }

    /// Get or create a balance for an asset.
    pub fn ensure_balance(&mut self, asset: &str) -> &mut Balance {
        self.balances
            .entry(asset.to_string())
            .or_insert_with(|| Balance::new(asset, 0.0, 0.0))
    }

    /// Check whether the available balance is sufficient.
    pub fn has_available(&self, asset: &str, amount: f64) -> bool {
        self.balances
            .get(asset)
            .map(|b| b.available >= amount)
            .unwrap_or(false)
    }

    /// Lock available funds (e.g. when placing an order).
    ///
    /// Returns `Err(amount)` if insufficient.
    pub fn lock(&mut self, asset: &str, amount: f64) -> Result<(), f64> {
        let balance = self
            .balances
            .get_mut(asset)
            .ok_or(amount)?;
        if balance.available < amount {
            return Err(amount);
        }
        balance.available -= amount;
        balance.locked += amount;
        Ok(())
    }

    /// Unlock previously locked funds (e.g. when cancelling an order).
    pub fn unlock(&mut self, asset: &str, amount: f64) {
        if let Some(balance) = self.balances.get_mut(asset) {
            let unlocked = amount.min(balance.locked);
            balance.locked -= unlocked;
            balance.available += unlocked;
        }
    }

    /// Settle a fill: reduce locked funds and apply the resulting balance delta.
    ///
    /// For a buy, `asset_delta` is positive (base received); for a sell it is
    /// negative. Locked quote is released separately via [`Wallet::unlock`].
    pub fn apply_fill(&mut self, asset: &str, asset_delta: f64) {
        let balance = self.ensure_balance(asset);
        balance.total += asset_delta;
        balance.available += asset_delta;
    }

    /// Add a realized PnL amount.
    pub fn add_realized_pnl(&mut self, amount: f64) {
        self.realized_pnl += amount;
    }

    /// Add a paid fee.
    pub fn add_fee(&mut self, amount: f64) {
        self.total_fees += amount;
    }

    /// Add a paid/received funding amount.
    pub fn add_funding(&mut self, amount: f64) {
        self.total_funding += amount;
    }

    /// The cumulative realized PnL.
    pub fn realized_pnl(&self) -> f64 {
        self.realized_pnl
    }

    /// The cumulative fees paid.
    pub fn total_fees(&self) -> f64 {
        self.total_fees
    }

    /// The cumulative funding paid (positive = paid out).
    pub fn total_funding(&self) -> f64 {
        self.total_funding
    }

    /// All balances.
    pub fn balances(&self) -> &HashMap<String, Balance> {
        &self.balances
    }

    /// Compute total equity in the quote asset.
    ///
    /// `prices` maps each asset to its price in the quote asset. Assets not in
    /// `prices` are assumed to be denominated in the quote asset itself (price 1.0).
    pub fn equity(&self, prices: &HashMap<String, f64>) -> f64 {
        self.balances
            .iter()
            .map(|(asset, b)| {
                let price = prices.get(asset).copied().unwrap_or(1.0);
                b.total * price
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deposit_and_withdraw() {
        let mut w = Wallet::new();
        w.deposit("USDT", 1000.0);
        assert_eq!(w.balance("USDT").unwrap().available, 1000.0);
        w.withdraw("USDT", 300.0).unwrap();
        assert_eq!(w.balance("USDT").unwrap().available, 700.0);
    }

    #[test]
    fn test_withdraw_insufficient() {
        let mut w = Wallet::new();
        w.deposit("USDT", 100.0);
        assert!(w.withdraw("USDT", 200.0).is_err());
        assert!(w.withdraw("BTC", 1.0).is_err());
    }

    #[test]
    fn test_lock_unlock() {
        let mut w = Wallet::new();
        w.deposit("USDT", 1000.0);
        w.lock("USDT", 400.0).unwrap();
        assert_eq!(w.balance("USDT").unwrap().available, 600.0);
        assert_eq!(w.balance("USDT").unwrap().locked, 400.0);
        w.unlock("USDT", 400.0);
        assert_eq!(w.balance("USDT").unwrap().available, 1000.0);
        assert_eq!(w.balance("USDT").unwrap().locked, 0.0);
    }

    #[test]
    fn test_lock_insufficient() {
        let mut w = Wallet::new();
        w.deposit("USDT", 100.0);
        assert!(w.lock("USDT", 200.0).is_err());
        assert!(w.lock("BTC", 1.0).is_err());
    }

    #[test]
    fn test_apply_fill() {
        let mut w = Wallet::new();
        w.deposit("USDT", 1000.0);
        w.apply_fill("BTC", 0.5);
        assert_eq!(w.balance("BTC").unwrap().total, 0.5);
        w.apply_fill("BTC", -0.2);
        assert_eq!(w.balance("BTC").unwrap().total, 0.3);
    }

    #[test]
    fn test_pnl_fees_funding() {
        let mut w = Wallet::new();
        w.add_realized_pnl(50.0);
        w.add_fee(2.0);
        w.add_funding(1.0);
        assert_eq!(w.realized_pnl(), 50.0);
        assert_eq!(w.total_fees(), 2.0);
        assert_eq!(w.total_funding(), 1.0);
    }

    #[test]
    fn test_equity() {
        let mut w = Wallet::new();
        w.deposit("USDT", 1000.0);
        w.deposit("BTC", 2.0);
        let mut prices = HashMap::new();
        prices.insert("BTC".to_string(), 50_000.0);
        // 1000 USDT (price 1.0) + 2 BTC * 50000 = 101000
        assert!((w.equity(&prices) - 101_000.0).abs() < 1e-9);
    }

    #[test]
    fn test_has_available() {
        let mut w = Wallet::new();
        w.deposit("USDT", 100.0);
        assert!(w.has_available("USDT", 50.0));
        assert!(!w.has_available("USDT", 150.0));
        assert!(!w.has_available("BTC", 1.0));
    }
}
