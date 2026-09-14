//! Order engine for simccxt.
//!
//! The matching core: matches incoming orders against an order book, produces
//! fills, and maintains the set of open (resting) orders. Deterministic by
//! construction: order IDs are sequential and matching is price-time priority.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use simccxt_core::{OrderStatus, OrderType, Side, Symbol, TimeInForce};
use simccxt_market::OrderBook;

/// A fully-specified order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    /// Exchange-assigned order id.
    pub id: String,
    /// Symbol.
    pub symbol: Symbol,
    /// Buy or sell.
    pub side: Side,
    /// Order type.
    pub order_type: OrderType,
    /// Limit price, if applicable.
    pub price: Option<f64>,
    /// Original amount in base asset.
    pub amount: f64,
    /// Remaining amount to fill.
    pub remaining: f64,
    /// Filled amount.
    pub filled: f64,
    /// Average fill price.
    pub average_price: f64,
    /// Current status.
    pub status: OrderStatus,
    /// Time in force.
    pub time_in_force: TimeInForce,
    /// Reduce-only flag.
    pub reduce_only: bool,
    /// Post-only flag.
    pub post_only: bool,
    /// Creation timestamp.
    pub timestamp: DateTime<Utc>,
    /// Cumulative fee paid.
    pub fee: f64,
}

impl Order {
    /// Whether the order is open (still active).
    pub fn is_open(&self) -> bool {
        matches!(self.status, OrderStatus::New | OrderStatus::PartialFilled)
    }

    /// Whether the order is fully filled.
    pub fn is_filled(&self) -> bool {
        self.status == OrderStatus::Filled
    }
}

/// A single fill (execution) resulting from matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    /// The order id this fill belongs to.
    pub order_id: String,
    /// Execution price.
    pub price: f64,
    /// Executed amount in base asset.
    pub amount: f64,
    /// Fee paid for this fill.
    pub fee: f64,
    /// Whether this fill took the maker side (taker = false).
    pub maker: bool,
    /// Execution timestamp.
    pub timestamp: DateTime<Utc>,
}

/// A request to place an order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    /// Buy or sell.
    pub side: Side,
    /// Order type.
    pub order_type: OrderType,
    /// Limit price (required for limit and stop-limit orders).
    pub price: Option<f64>,
    /// Amount in base asset.
    pub amount: f64,
    /// Time in force (only relevant for limit orders).
    pub time_in_force: TimeInForce,
    /// Reduce-only flag.
    pub reduce_only: bool,
    /// Post-only flag.
    pub post_only: bool,
}

impl OrderRequest {
    /// Create a simple market order.
    pub fn market(side: Side, amount: f64) -> Self {
        Self {
            side,
            order_type: OrderType::Market,
            price: None,
            amount,
            time_in_force: TimeInForce::GTC,
            reduce_only: false,
            post_only: false,
        }
    }

    /// Create a simple limit order.
    pub fn limit(side: Side, price: f64, amount: f64) -> Self {
        Self {
            side,
            order_type: OrderType::Limit,
            price: Some(price),
            amount,
            time_in_force: TimeInForce::GTC,
            reduce_only: false,
            post_only: false,
        }
    }
}

/// The result of placing an order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReport {
    /// The order after matching.
    pub order: Order,
    /// Fills produced by matching.
    pub fills: Vec<Fill>,
}

/// Fee schedule.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FeeSchedule {
    /// Maker fee rate.
    pub maker: f64,
    /// Taker fee rate.
    pub taker: f64,
}

impl Default for FeeSchedule {
    fn default() -> Self {
        Self { maker: 0.001, taker: 0.0025 }
    }
}

/// The matching engine for a single symbol.
#[derive(Debug, Clone)]
pub struct OrderEngine {
    symbol: Symbol,
    book: OrderBook,
    open_orders: HashMap<String, Order>,
    fees: FeeSchedule,
    next_order_id: u64,
}

impl OrderEngine {
    /// Create an empty matching engine for a symbol.
    pub fn new(symbol: Symbol, timestamp: DateTime<Utc>) -> Self {
        let book = OrderBook::new(symbol.clone(), timestamp);
        Self {
            symbol,
            book,
            open_orders: HashMap::new(),
            fees: FeeSchedule::default(),
            next_order_id: 1,
        }
    }

    /// Create an engine with a custom fee schedule.
    pub fn with_fees(symbol: Symbol, timestamp: DateTime<Utc>, fees: FeeSchedule) -> Self {
        let mut engine = Self::new(symbol, timestamp);
        engine.fees = fees;
        engine
    }

    /// The symbol this engine matches.
    pub fn symbol(&self) -> &Symbol {
        &self.symbol
    }

    /// Immutable access to the order book.
    pub fn book(&self) -> &OrderBook {
        &self.book
    }

    /// Apply an order book update (bid or ask).
    pub fn update_bid(&mut self, price: f64, amount: f64) {
        self.book.update_bid(price, amount);
    }

    /// Apply an order book update (ask).
    pub fn update_ask(&mut self, price: f64, amount: f64) {
        self.book.update_ask(price, amount);
    }

    /// Get an open order by id.
    pub fn get_order(&self, id: &str) -> Option<&Order> {
        self.open_orders.get(id)
    }

    /// All open orders.
    pub fn open_orders(&self) -> impl Iterator<Item = &Order> {
        self.open_orders.values()
    }

    /// The number of open (resting) orders.
    pub fn open_order_count(&self) -> usize {
        self.open_orders.len()
    }

    /// Place an order, matching it against the book.
    pub fn place(&mut self, request: OrderRequest, timestamp: DateTime<Utc>) -> ExecutionReport {
        let id = self.next_order_id.to_string();
        self.next_order_id += 1;

        let order = Order {
            id: id.clone(),
            symbol: self.symbol.clone(),
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
        };

        let mut report = ExecutionReport { order, fills: Vec::new() };

        match request.order_type {
            OrderType::Market => self.match_market(&mut report, timestamp),
            OrderType::Limit => self.match_limit(&mut report, timestamp),
            _ => {
                // Other order types are handled by the broker; here they rest
                // unconditionally for now.
                report.order.status = OrderStatus::New;
                self.rest(&report.order);
            }
        }

        report
    }

    /// Match a market order against the book.
    fn match_market(&mut self, report: &mut ExecutionReport, timestamp: DateTime<Utc>) {
        let remaining = report.order.remaining;
        let (fills, leftover) = self.book.consume(report.order.side, remaining);
        let mut total_fee = 0.0;
        let mut notional = 0.0;
        for (price, amount) in fills {
            let fee = self.fee_for(amount, price, false);
            total_fee += fee;
            notional += amount * price;
            report.fills.push(Fill {
                order_id: report.order.id.clone(),
                price,
                amount,
                fee,
                maker: false,
                timestamp,
            });
        }
        report.order.filled = report.order.amount - leftover;
        report.order.remaining = leftover;
        report.order.average_price = if report.order.filled > 0.0 {
            notional / report.order.filled
        } else {
            0.0
        };
        report.order.fee = total_fee;
        report.order.status = if leftover <= 0.0 {
            OrderStatus::Filled
        } else {
            // Market order with unfilled remainder is cancelled (IOC semantics).
            OrderStatus::Canceled
        };
    }

    /// Match a limit order against the book, resting any remainder.
    fn match_limit(&mut self, report: &mut ExecutionReport, timestamp: DateTime<Utc>) {
        let price = report.order.price.expect("limit order requires a price");

        if report.order.post_only {
            // Post-only orders never take liquidity: if they would cross,
            // they are cancelled immediately without matching.
            let crosses = match report.order.side {
                Side::Buy => self.book.best_ask().map(|a| a.price <= price).unwrap_or(false),
                Side::Sell => self.book.best_bid().map(|b| b.price >= price).unwrap_or(false),
            };
            if crosses {
                report.order.status = OrderStatus::Canceled;
            } else {
                report.order.status = OrderStatus::New;
                self.rest(&report.order);
            }
            return;
        }

        let remaining = report.order.remaining;
        let (fills, leftover) = self.consume_limit(report.order.side, price, remaining);
        let mut total_fee = 0.0;
        let mut notional = 0.0;
        for (fill_price, amount) in fills {
            let fee = self.fee_for(amount, fill_price, true);
            total_fee += fee;
            notional += amount * fill_price;
            report.fills.push(Fill {
                order_id: report.order.id.clone(),
                price: fill_price,
                amount,
                fee,
                maker: true,
                timestamp,
            });
        }
        report.order.filled = report.order.amount - leftover;
        report.order.remaining = leftover;
        report.order.average_price = if report.order.filled > 0.0 {
            notional / report.order.filled
        } else {
            0.0
        };
        report.order.fee = total_fee;

        if leftover <= 0.0 {
            report.order.status = OrderStatus::Filled;
        } else {
            report.order.status = OrderStatus::New;
            self.rest(&report.order);
        }
    }

    /// Consume liquidity for a limit order, respecting the limit price.
    fn consume_limit(
        &mut self,
        side: Side,
        limit_price: f64,
        amount: f64,
    ) -> (Vec<(f64, f64)>, f64) {
        let mut fills = Vec::new();
        let mut remaining = amount;

        // Determine the opposing levels and whether each crosses the limit.
        let levels = match side {
            Side::Buy => &mut self.book.asks,
            Side::Sell => &mut self.book.bids,
        };

        for level in levels.iter_mut() {
            if remaining <= 0.0 {
                break;
            }
            let crosses = match side {
                Side::Buy => level.price <= limit_price,
                Side::Sell => level.price >= limit_price,
            };
            if !crosses {
                break;
            }
            if level.amount <= 0.0 {
                continue;
            }
            let fill = remaining.min(level.amount);
            fills.push((level.price, fill));
            level.amount -= fill;
            remaining -= fill;
        }

        let levels = match side {
            Side::Buy => &mut self.book.asks,
            Side::Sell => &mut self.book.bids,
        };
        levels.retain(|l| l.amount > 0.0);

        (fills, remaining)
    }

    /// Add a non-crossing order to the book (and to the open-order set).
    fn rest(&mut self, order: &Order) {
        let price = order.price.expect("resting order requires a price");
        match order.side {
            Side::Buy => self.book.update_bid(price, order.remaining),
            Side::Sell => self.book.update_ask(price, order.remaining),
        }
        self.open_orders.insert(order.id.clone(), order.clone());
    }

    /// Cancel an open order, returning the cancelled order if found.
    pub fn cancel_order(&mut self, id: &str) -> Option<Order> {
        let order = self.open_orders.remove(id)?;
        // Release its resting liquidity from the book.
        if let Some(price) = order.price {
            let amount = order.remaining;
            match order.side {
                Side::Buy => self.remove_bid(price, amount),
                Side::Sell => self.remove_ask(price, amount),
            }
        }
        let mut cancelled = order;
        cancelled.status = OrderStatus::Canceled;
        Some(cancelled)
    }

    fn remove_bid(&mut self, price: f64, amount: f64) {
        if let Some(level) = self.book.bids.iter_mut().find(|l| l.price == price) {
            level.amount -= amount;
        }
        self.book.bids.retain(|l| l.amount > 0.0);
    }

    fn remove_ask(&mut self, price: f64, amount: f64) {
        if let Some(level) = self.book.asks.iter_mut().find(|l| l.price == price) {
            level.amount -= amount;
        }
        self.book.asks.retain(|l| l.amount > 0.0);
    }

    fn fee_for(&self, amount: f64, price: f64, maker: bool) -> f64 {
        let rate = if maker { self.fees.maker } else { self.fees.taker };
        amount * price * rate
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

    #[test]
    fn test_limit_order_rests_when_no_cross() {
        let mut engine = OrderEngine::new(btc(), ts());
        let report = engine.place(OrderRequest::limit(Side::Buy, 100.0, 1.0), ts());
        assert_eq!(report.order.status, OrderStatus::New);
        assert_eq!(report.fills.len(), 0);
        assert_eq!(engine.open_order_count(), 1);
        assert_eq!(engine.book().best_bid().unwrap().price, 100.0);
    }

    #[test]
    fn test_market_buy_fills_against_ask() {
        let mut engine = OrderEngine::new(btc(), ts());
        engine.update_ask(100.0, 2.0);
        let report = engine.place(OrderRequest::market(Side::Buy, 1.0), ts());
        assert_eq!(report.order.status, OrderStatus::Filled);
        assert_eq!(report.fills.len(), 1);
        assert_eq!(report.fills[0].price, 100.0);
        assert_eq!(report.fills[0].amount, 1.0);
        assert_eq!(engine.book().best_ask().unwrap().amount, 1.0);
    }

    #[test]
    fn test_market_sell_fills_against_bid() {
        let mut engine = OrderEngine::new(btc(), ts());
        engine.update_bid(100.0, 2.0);
        let report = engine.place(OrderRequest::market(Side::Sell, 1.5), ts());
        assert_eq!(report.order.status, OrderStatus::Filled);
        assert_eq!(report.fills[0].price, 100.0);
        assert_eq!(report.fills[0].amount, 1.5);
        assert_eq!(engine.book().best_bid().unwrap().amount, 0.5);
    }

    #[test]
    fn test_market_order_partial_fill_cancelled() {
        let mut engine = OrderEngine::new(btc(), ts());
        engine.update_ask(100.0, 1.0);
        let report = engine.place(OrderRequest::market(Side::Buy, 5.0), ts());
        assert_eq!(report.order.filled, 1.0);
        assert_eq!(report.order.remaining, 4.0);
        assert_eq!(report.order.status, OrderStatus::Canceled);
    }

    #[test]
    fn test_limit_cross_matches_at_limit() {
        let mut engine = OrderEngine::new(btc(), ts());
        engine.update_ask(100.0, 2.0);
        // Buy limit at 101 crosses the ask at 100.
        let report = engine.place(OrderRequest::limit(Side::Buy, 101.0, 1.0), ts());
        assert_eq!(report.order.status, OrderStatus::Filled);
        assert_eq!(report.fills[0].price, 100.0);
    }

    #[test]
    fn test_limit_no_cross_at_worse_price() {
        let mut engine = OrderEngine::new(btc(), ts());
        engine.update_ask(100.0, 2.0);
        // Buy limit at 99 does not cross.
        let report = engine.place(OrderRequest::limit(Side::Buy, 99.0, 1.0), ts());
        assert_eq!(report.order.status, OrderStatus::New);
        assert_eq!(report.fills.len(), 0);
    }

    #[test]
    fn test_partial_limit_fill_rests_remainder() {
        let mut engine = OrderEngine::new(btc(), ts());
        engine.update_ask(100.0, 1.0);
        let report = engine.place(OrderRequest::limit(Side::Buy, 100.0, 3.0), ts());
        assert_eq!(report.order.filled, 1.0);
        assert_eq!(report.order.remaining, 2.0);
        assert_eq!(report.order.status, OrderStatus::New);
        // Remainder rests on the book.
        assert_eq!(engine.book().best_bid().unwrap().price, 100.0);
        assert_eq!(engine.book().best_bid().unwrap().amount, 2.0);
    }

    #[test]
    fn test_cancel_order_releases_liquidity() {
        let mut engine = OrderEngine::new(btc(), ts());
        let report = engine.place(OrderRequest::limit(Side::Buy, 100.0, 2.0), ts());
        let id = report.order.id.clone();
        assert_eq!(engine.book().best_bid().unwrap().amount, 2.0);
        let cancelled = engine.cancel_order(&id).unwrap();
        assert_eq!(cancelled.status, OrderStatus::Canceled);
        assert!(engine.book().bids.is_empty());
        assert_eq!(engine.open_order_count(), 0);
    }

    #[test]
    fn test_cancel_unknown_order() {
        let mut engine = OrderEngine::new(btc(), ts());
        assert!(engine.cancel_order("nope").is_none());
    }

    #[test]
    fn test_fees_applied() {
        let mut engine = OrderEngine::with_fees(btc(), ts(), FeeSchedule { maker: 0.001, taker: 0.0025 });
        engine.update_ask(100.0, 10.0);
        let report = engine.place(OrderRequest::market(Side::Buy, 1.0), ts());
        // taker fee = 100 * 1 * 0.0025 = 0.25
        assert!((report.order.fee - 0.25).abs() < 1e-9);
        assert!((report.fills[0].fee - 0.25).abs() < 1e-9);
    }

    #[test]
    fn test_post_only_cancelled_when_crosses() {
        let mut engine = OrderEngine::new(btc(), ts());
        engine.update_ask(100.0, 2.0);
        let mut req = OrderRequest::limit(Side::Buy, 101.0, 1.0);
        req.post_only = true;
        let report = engine.place(req, ts());
        assert_eq!(report.order.status, OrderStatus::Canceled);
        assert!(report.fills.is_empty());
    }

    #[test]
    fn test_average_price_weighted() {
        let mut engine = OrderEngine::new(btc(), ts());
        engine.update_ask(100.0, 1.0);
        engine.update_ask(102.0, 1.0);
        let report = engine.place(OrderRequest::market(Side::Buy, 2.0), ts());
        assert_eq!(report.order.status, OrderStatus::Filled);
        // (100*1 + 102*1) / 2 = 101
        assert!((report.order.average_price - 101.0).abs() < 1e-9);
    }
}
