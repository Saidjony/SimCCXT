//! Broker for simccxt.
//!
//! Orchestrates order execution across the order engine, wallet, and portfolio.
//! Handles order-type semantics (market, limit, stop, stop-limit, take-profit,
//! trailing-stop, post-only, IOC, FOK, reduce-only) and account settlement.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use simccxt_core::{OrderStatus, OrderType, Side, Symbol, TimeInForce};
use simccxt_order_engine::{FeeSchedule, Order, OrderEngine, OrderRequest};
use simccxt_portfolio::{MarginMode, Portfolio};
use simccxt_wallet::Wallet;

/// The kind of market for a symbol, which determines settlement semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketKind {
    /// Spot market: fills settle base/quote into the wallet.
    Spot,
    /// Linear futures: fills update portfolio positions; margin locked in quote.
    Linear,
}

/// Errors produced by the broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BrokerError {
    /// The symbol is not registered.
    UnknownSymbol(String),
    /// The amount is invalid.
    InvalidAmount(f64),
    /// Insufficient funds for the requested order.
    InsufficientFunds { required: f64, available: f64 },
    /// The order id was not found.
    UnknownOrder(String),
    /// The order type is not supported for the given parameters.
    InvalidOrderType(String),
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrokerError::UnknownSymbol(s) => write!(f, "unknown symbol: {s}"),
            BrokerError::InvalidAmount(a) => write!(f, "invalid amount: {a}"),
            BrokerError::InsufficientFunds { required, available } => {
                write!(f, "insufficient funds: required {required}, available {available}")
            }
            BrokerError::UnknownOrder(id) => write!(f, "unknown order: {id}"),
            BrokerError::InvalidOrderType(msg) => write!(f, "invalid order type: {msg}"),
        }
    }
}

impl std::error::Error for BrokerError {}

/// A conditional (triggered) order, e.g. stop or take-profit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConditionalOrder {
    /// Unique conditional id.
    pub id: u64,
    /// Symbol.
    pub symbol: Symbol,
    /// Side once triggered.
    pub side: Side,
    /// Order type once triggered.
    pub order_type: OrderType,
    /// Amount.
    pub amount: f64,
    /// Limit price (for stop-limit / take-profit-limit).
    pub price: Option<f64>,
    /// Trigger price.
    pub trigger_price: f64,
    /// Reduce-only flag.
    pub reduce_only: bool,
    /// Creation timestamp.
    pub timestamp: DateTime<Utc>,
}

/// Parameters for a broker order (maps to CCXT `create_order`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerOrderRequest {
    /// Symbol.
    pub symbol: Symbol,
    /// Side.
    pub side: Side,
    /// Order type.
    pub order_type: OrderType,
    /// Amount in base asset.
    pub amount: f64,
    /// Limit price.
    pub price: Option<f64>,
    /// Trigger/stop price.
    pub stop_price: Option<f64>,
    /// Time in force.
    pub time_in_force: TimeInForce,
    /// Reduce-only flag.
    pub reduce_only: bool,
    /// Post-only flag.
    pub post_only: bool,
}

impl BrokerOrderRequest {
    /// Create a market order.
    pub fn market(symbol: Symbol, side: Side, amount: f64) -> Self {
        Self {
            symbol,
            side,
            order_type: OrderType::Market,
            amount,
            price: None,
            stop_price: None,
            time_in_force: TimeInForce::GTC,
            reduce_only: false,
            post_only: false,
        }
    }

    /// Create a limit order.
    pub fn limit(symbol: Symbol, side: Side, price: f64, amount: f64) -> Self {
        Self {
            symbol,
            side,
            order_type: OrderType::Limit,
            amount,
            price: Some(price),
            stop_price: None,
            time_in_force: TimeInForce::GTC,
            reduce_only: false,
            post_only: false,
        }
    }
}

/// Describes what was locked by the broker for a pending order.
struct LockInfo {
    asset: String,
    /// Amount released per filled unit (limit price for buys, 1.0 for sells).
    unit_release: f64,
}

/// The broker.
pub struct Broker {
    engines: HashMap<String, OrderEngine>,
    market_kinds: HashMap<String, MarketKind>,
    wallet: Wallet,
    portfolio: Portfolio,
    fees: FeeSchedule,
    conditional: Vec<ConditionalOrder>,
    next_conditional_id: u64,
}

impl Default for Broker {
    fn default() -> Self {
        Self {
            engines: HashMap::new(),
            market_kinds: HashMap::new(),
            wallet: Wallet::new(),
            portfolio: Portfolio::new(),
            fees: FeeSchedule::default(),
            conditional: Vec::new(),
            next_conditional_id: 1,
        }
    }
}

impl Broker {
    /// Create an empty broker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a market for a symbol.
    pub fn add_market(&mut self, symbol: &Symbol, kind: MarketKind, timestamp: DateTime<Utc>) {
        self.market_kinds.insert(symbol.symbol.clone(), kind);
        self.engines.insert(
            symbol.symbol.clone(),
            OrderEngine::with_fees(symbol.clone(), timestamp, self.fees),
        );
    }

    /// The wallet (immutable access).
    pub fn wallet(&self) -> &Wallet {
        &self.wallet
    }

    /// The wallet (mutable access).
    pub fn wallet_mut(&mut self) -> &mut Wallet {
        &mut self.wallet
    }

    /// The portfolio (immutable access).
    pub fn portfolio(&self) -> &Portfolio {
        &self.portfolio
    }

    /// The portfolio (mutable access).
    pub fn portfolio_mut(&mut self) -> &mut Portfolio {
        &mut self.portfolio
    }

    /// Set the fee schedule.
    pub fn set_fees(&mut self, fees: FeeSchedule) {
        self.fees = fees;
    }

    /// Set the margin mode.
    pub fn set_margin_mode(&mut self, mode: MarginMode) {
        self.portfolio.set_margin_mode(mode);
    }

    /// Set leverage for a symbol.
    pub fn set_leverage(&mut self, symbol: &str, leverage: f64) {
        self.portfolio.set_leverage(symbol, leverage);
    }

    /// The order engine for a symbol.
    pub fn engine(&self, symbol: &str) -> Option<&OrderEngine> {
        self.engines.get(symbol)
    }

    fn engine_mut(&mut self, symbol: &str) -> Option<&mut OrderEngine> {
        self.engines.get_mut(symbol)
    }

    /// Seed a bid level into the symbol's order book (for replay/testing).
    pub fn update_bid(&mut self, symbol: &str, price: f64, amount: f64) {
        if let Some(engine) = self.engine_mut(symbol) {
            engine.update_bid(price, amount);
        }
    }

    /// Seed an ask level into the symbol's order book (for replay/testing).
    pub fn update_ask(&mut self, symbol: &str, price: f64, amount: f64) {
        if let Some(engine) = self.engine_mut(symbol) {
            engine.update_ask(price, amount);
        }
    }

    /// Get the kind of a market.
    pub fn market_kind(&self, symbol: &str) -> Option<MarketKind> {
        self.market_kinds.get(symbol).copied()
    }

    /// Submit an order for execution.
    pub fn submit_order(
        &mut self,
        request: BrokerOrderRequest,
        timestamp: DateTime<Utc>,
    ) -> Result<Order, BrokerError> {
        if request.amount <= 0.0 {
            return Err(BrokerError::InvalidAmount(request.amount));
        }
        if !self.engines.contains_key(&request.symbol.symbol) {
            return Err(BrokerError::UnknownSymbol(request.symbol.symbol.clone()));
        }

        // Conditional order types defer to a trigger.
        match request.order_type {
            OrderType::Stop
            | OrderType::StopLimit
            | OrderType::TakeProfit
            | OrderType::TakeProfitLimit
            | OrderType::TrailingStop => {
                let trigger_price = request.stop_price.ok_or_else(|| {
                    BrokerError::InvalidOrderType("trigger order requires a stop price".into())
                })?;
                let conditional = ConditionalOrder {
                    id: self.next_conditional_id,
                    symbol: request.symbol.clone(),
                    side: request.side,
                    order_type: request.order_type,
                    amount: request.amount,
                    price: request.price,
                    trigger_price,
                    reduce_only: request.reduce_only,
                    timestamp,
                };
                self.next_conditional_id += 1;
                self.conditional.push(conditional);
                return Ok(Order {
                    id: "conditional".into(),
                    symbol: request.symbol,
                    side: request.side,
                    order_type: request.order_type,
                    price: request.price,
                    amount: request.amount,
                    remaining: request.amount,
                    filled: 0.0,
                    average_price: 0.0,
                    status: OrderStatus::New,
                    time_in_force: request.time_in_force,
                    reduce_only: request.reduce_only,
                    post_only: request.post_only,
                    timestamp,
                    fee: 0.0,
                });
            }
            _ => {}
        }

        self.execute_immediate(request, timestamp)
    }

    /// Execute a market or limit order immediately.
    fn execute_immediate(
        &mut self,
        request: BrokerOrderRequest,
        timestamp: DateTime<Utc>,
    ) -> Result<Order, BrokerError> {
        let symbol = request.symbol.clone();
        let kind = self.market_kinds.get(&symbol.symbol).copied().unwrap();

        // Determine the reference price (limit price, or book price for market).
        let ref_price = match request.order_type {
            OrderType::Limit => request.price.unwrap(),
            OrderType::Market => self.market_reference_price(&symbol, request.side).unwrap_or(0.0),
            _ => request.price.unwrap_or(0.0),
        };

        // Lock or validate funds.
        let lock = if request.order_type == OrderType::Limit {
            Some(self.lock_funds(&symbol, kind, request.side, ref_price, request.amount)?)
        } else {
            self.validate_market_funds(&symbol, kind, request.side, ref_price, request.amount)?;
            None
        };

        let engine_request = OrderRequest {
            side: request.side,
            order_type: request.order_type,
            price: request.price,
            amount: request.amount,
            time_in_force: request.time_in_force,
            reduce_only: request.reduce_only,
            post_only: request.post_only,
        };

        let report = {
            let engine = self.engine_mut(&symbol.symbol).unwrap();
            engine.place(engine_request, timestamp)
        };

        // Settle fills.
        let filled_amount = report.order.filled;
        let filled_notional: f64 = report.fills.iter().map(|f| f.amount * f.price).sum();
        let fee_total: f64 = report.fills.iter().map(|f| f.fee).sum();

        match kind {
            MarketKind::Spot => {
                self.settle_spot(&symbol, request.side, lock, filled_amount, filled_notional, fee_total);
            }
            MarketKind::Linear => {
                self.settle_linear(&symbol, request.side, lock, filled_amount, filled_notional, fee_total);
            }
        }

        Ok(report.order)
    }

    /// Reference price for a market order (best bid/ask from the book).
    fn market_reference_price(&self, symbol: &Symbol, side: Side) -> Option<f64> {
        let engine = self.engine(&symbol.symbol)?;
        match side {
            Side::Buy => engine.book().best_ask().map(|a| a.price),
            Side::Sell => engine.book().best_bid().map(|b| b.price),
        }
    }

    /// Lock funds for a limit order, returning what was locked.
    fn lock_funds(
        &mut self,
        symbol: &Symbol,
        kind: MarketKind,
        side: Side,
        price: f64,
        amount: f64,
    ) -> Result<LockInfo, BrokerError> {
        match kind {
            MarketKind::Spot => match side {
                Side::Buy => {
                    let required = amount * price;
                    self.lock_asset(&symbol.quote, required)?;
                    Ok(LockInfo { asset: symbol.quote.clone(), unit_release: price })
                }
                Side::Sell => {
                    self.lock_asset(&symbol.base, amount)?;
                    Ok(LockInfo { asset: symbol.base.clone(), unit_release: 1.0 })
                }
            },
            MarketKind::Linear => {
                let notional = amount * price;
                let leverage = self.portfolio.leverage(&symbol.symbol);
                let required = notional / leverage;
                self.lock_asset(&symbol.quote, required)?;
                Ok(LockInfo { asset: symbol.quote.clone(), unit_release: price / leverage })
            }
        }
    }

    fn lock_asset(&mut self, asset: &str, amount: f64) -> Result<(), BrokerError> {
        let available = self.wallet.balance(asset).map(|b| b.available).unwrap_or(0.0);
        if available < amount {
            return Err(BrokerError::InsufficientFunds { required: amount, available });
        }
        self.wallet.lock(asset, amount).map_err(|_| {
            BrokerError::InsufficientFunds { required: amount, available }
        })
    }

    /// Validate (but do not lock) funds for a market order.
    fn validate_market_funds(
        &self,
        symbol: &Symbol,
        kind: MarketKind,
        side: Side,
        ref_price: f64,
        amount: f64,
    ) -> Result<(), BrokerError> {
        match kind {
            MarketKind::Spot => match side {
                Side::Buy => {
                    let required = amount * ref_price;
                    let available = self.wallet.balance(&symbol.quote).map(|b| b.available).unwrap_or(0.0);
                    if available < required {
                        return Err(BrokerError::InsufficientFunds { required, available });
                    }
                }
                Side::Sell => {
                    let available = self.wallet.balance(&symbol.base).map(|b| b.available).unwrap_or(0.0);
                    if available < amount {
                        return Err(BrokerError::InsufficientFunds { required: amount, available });
                    }
                }
            },
            MarketKind::Linear => {
                let notional = amount * ref_price;
                let leverage = self.portfolio.leverage(&symbol.symbol);
                let required = notional / leverage;
                let available = self.wallet.balance(&symbol.quote).map(|b| b.available).unwrap_or(0.0);
                if available < required {
                    return Err(BrokerError::InsufficientFunds { required, available });
                }
            }
        }
        Ok(())
    }

    /// Settle a spot fill.
    fn settle_spot(
        &mut self,
        symbol: &Symbol,
        side: Side,
        lock: Option<LockInfo>,
        filled_amount: f64,
        filled_notional: f64,
        fee_total: f64,
    ) {
        if let Some(lock) = &lock {
            let release = filled_amount * lock.unit_release;
            self.wallet.unlock(&lock.asset, release);
        }
        match side {
            Side::Buy => {
                self.wallet.apply_fill(&symbol.base, filled_amount);
                self.wallet.apply_fill(&symbol.quote, -(filled_notional + fee_total));
            }
            Side::Sell => {
                self.wallet.apply_fill(&symbol.base, -filled_amount);
                self.wallet.apply_fill(&symbol.quote, filled_notional - fee_total);
            }
        }
        self.wallet.add_fee(fee_total);
    }

    /// Settle a linear (futures) fill: update position and deduct fees.
    fn settle_linear(
        &mut self,
        symbol: &Symbol,
        side: Side,
        lock: Option<LockInfo>,
        filled_amount: f64,
        filled_notional: f64,
        fee_total: f64,
    ) {
        if let Some(lock) = &lock {
            let release = filled_amount * lock.unit_release;
            self.wallet.unlock(&lock.asset, release);
        }
        if filled_amount > 0.0 {
            let avg_price = filled_notional / filled_amount;
            self.portfolio.apply_fill(symbol, side, filled_amount, avg_price);
        }
        self.wallet.add_fee(fee_total);
    }

    /// Cancel an open order.
    pub fn cancel_order(&mut self, symbol: &str, id: &str) -> Result<Order, BrokerError> {
        let engine = self.engine_mut(symbol).ok_or_else(|| BrokerError::UnknownSymbol(symbol.to_string()))?;
        let order = engine.cancel_order(id).ok_or_else(|| BrokerError::UnknownOrder(id.to_string()))?;
        self.release_locked(symbol, &order);
        Ok(order)
    }

    fn release_locked(&mut self, symbol: &str, order: &Order) {
        let kind = self.market_kinds.get(symbol).copied().unwrap_or(MarketKind::Spot);
        let price = order.price.unwrap_or(0.0);
        let remaining = order.remaining;
        match kind {
            MarketKind::Spot => match order.side {
                Side::Buy => {
                    let locked = remaining * price;
                    self.wallet.unlock(&order.symbol.quote, locked);
                }
                Side::Sell => {
                    self.wallet.unlock(&order.symbol.base, remaining);
                }
            },
            MarketKind::Linear => {
                let notional = remaining * price;
                let leverage = self.portfolio.leverage(symbol);
                self.wallet.unlock(&order.symbol.quote, notional / leverage);
            }
        }
    }

    /// Notify the broker of a price update, triggering any ready conditional orders.
    ///
    /// Returns the orders that were triggered and executed.
    pub fn on_price_update(&mut self, symbol: &Symbol, price: f64, timestamp: DateTime<Utc>) -> Vec<Order> {
        let mut executed = Vec::new();
        let mut to_execute = Vec::new();

        // Update mark price for the portfolio.
        self.portfolio.update_mark_price(&symbol.symbol, price);

        // Determine which conditional orders are triggered.
        self.conditional.retain(|c| {
            if c.symbol.symbol != symbol.symbol {
                return true;
            }
            let triggered = is_triggered(c, price);
            if triggered {
                to_execute.push(c.clone());
                false
            } else {
                true
            }
        });

        for c in to_execute {
            let request = BrokerOrderRequest {
                symbol: c.symbol.clone(),
                side: c.side,
                order_type: match c.order_type {
                    OrderType::Stop | OrderType::TakeProfit | OrderType::TrailingStop => OrderType::Market,
                    OrderType::StopLimit | OrderType::TakeProfitLimit => OrderType::Limit,
                    other => other,
                },
                amount: c.amount,
                price: c.price,
                stop_price: None,
                time_in_force: TimeInForce::GTC,
                reduce_only: c.reduce_only,
                post_only: false,
            };
            if let Ok(order) = self.execute_immediate(request, timestamp) {
                executed.push(order);
            }
        }

        executed
    }

    /// Open orders for a symbol.
    pub fn open_orders(&self, symbol: &str) -> Vec<Order> {
        self.engines
            .get(symbol)
            .map(|e| e.open_orders().cloned().collect())
            .unwrap_or_default()
    }
}

/// Whether a conditional order is triggered at the given price.
fn is_triggered(c: &ConditionalOrder, price: f64) -> bool {
    match c.side {
        Side::Buy => price >= c.trigger_price,
        Side::Sell => price <= c.trigger_price,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1_704_067_200_000).unwrap()
    }

    fn btc() -> Symbol {
        Symbol::parse("BTC/USDT").unwrap()
    }

    fn spot_broker() -> Broker {
        let mut b = Broker::new();
        b.add_market(&btc(), MarketKind::Spot, ts());
        b.wallet_mut().deposit("USDT", 100_000.0);
        b.wallet_mut().deposit("BTC", 10.0);
        b
    }

    #[test]
    fn test_market_buy_settles_spot() {
        let mut b = spot_broker();
        b.engine_mut("BTC/USDT").unwrap().update_ask(100.0, 1.0);
        let order = b
            .submit_order(BrokerOrderRequest::market(btc(), Side::Buy, 1.0), ts())
            .unwrap();
        assert_eq!(order.status, OrderStatus::Filled);
        assert_eq!(b.wallet().balance("BTC").unwrap().total, 11.0);
        let usdt = b.wallet().balance("USDT").unwrap().total;
        assert!((usdt - (100_000.0 - 100.0 - 0.25)).abs() < 1e-6);
    }

    #[test]
    fn test_market_sell_settles_spot() {
        let mut b = spot_broker();
        b.engine_mut("BTC/USDT").unwrap().update_bid(100.0, 1.0);
        let order = b
            .submit_order(BrokerOrderRequest::market(btc(), Side::Sell, 1.0), ts())
            .unwrap();
        assert_eq!(order.status, OrderStatus::Filled);
        assert_eq!(b.wallet().balance("BTC").unwrap().total, 9.0);
    }

    #[test]
    fn test_insufficient_funds_rejected() {
        let mut b = Broker::new();
        b.add_market(&btc(), MarketKind::Spot, ts());
        b.wallet_mut().deposit("USDT", 10.0);
        b.engine_mut("BTC/USDT").unwrap().update_ask(100.0, 1.0);
        let result = b.submit_order(BrokerOrderRequest::market(btc(), Side::Buy, 1.0), ts());
        assert!(matches!(result, Err(BrokerError::InsufficientFunds { .. })));
    }

    #[test]
    fn test_unknown_symbol_rejected() {
        let mut b = Broker::new();
        let result = b.submit_order(BrokerOrderRequest::market(btc(), Side::Buy, 1.0), ts());
        assert!(matches!(result, Err(BrokerError::UnknownSymbol(_))));
    }

    #[test]
    fn test_limit_order_cancel_releases_funds() {
        let mut b = spot_broker();
        let order = b
            .submit_order(BrokerOrderRequest::limit(btc(), Side::Buy, 90.0, 1.0), ts())
            .unwrap();
        let id = order.id.clone();
        assert!(b.wallet().balance("USDT").unwrap().locked > 0.0);
        b.cancel_order("BTC/USDT", &id).unwrap();
        assert_eq!(b.wallet().balance("USDT").unwrap().locked, 0.0);
        assert_eq!(b.wallet().balance("USDT").unwrap().available, 100_000.0);
    }

    #[test]
    fn test_linear_futures_position() {
        let mut b = Broker::new();
        b.add_market(&btc(), MarketKind::Linear, ts());
        b.wallet_mut().deposit("USDT", 100_000.0);
        b.set_leverage("BTC/USDT", 10.0);
        b.engine_mut("BTC/USDT").unwrap().update_ask(100.0, 10.0);
        let order = b
            .submit_order(BrokerOrderRequest::market(btc(), Side::Buy, 1.0), ts())
            .unwrap();
        assert_eq!(order.status, OrderStatus::Filled);
        let pos = b.portfolio().position("BTC/USDT").unwrap();
        assert_eq!(pos.qty, 1.0);
    }

    #[test]
    fn test_conditional_stop_order_triggers() {
        let mut b = spot_broker();
        b.engine_mut("BTC/USDT").unwrap().update_ask(120.0, 10.0);
        let mut req = BrokerOrderRequest::market(btc(), Side::Buy, 1.0);
        req.order_type = OrderType::Stop;
        req.stop_price = Some(110.0);
        let order = b.submit_order(req, ts()).unwrap();
        assert_eq!(order.status, OrderStatus::New);
        assert!(b.on_price_update(&btc(), 105.0, ts()).is_empty());
        let exec = b.on_price_update(&btc(), 111.0, ts());
        assert_eq!(exec.len(), 1);
        assert_eq!(exec[0].status, OrderStatus::Filled);
    }
}
