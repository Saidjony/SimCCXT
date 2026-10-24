//! Broker for simccxt.
//!
//! Orchestrates order execution across the order engine, wallet, and portfolio.
//! Handles order-type semantics (market, limit, stop, stop-limit, take-profit,
//! trailing-stop, post-only, IOC, FOK, reduce-only) and account settlement.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use simccxt_core::{OrderStatus, OrderType, PositionSide, Side, Symbol, TimeInForce};
use simccxt_order_engine::{ExecutionReport, FeeSchedule, Fill, Order, OrderEngine, OrderRequest};
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

/// A recorded fill (execution) against the account's own orders.
///
/// The broker is the source of truth for what the account actually traded. Every
/// fill produced by market/limit execution (manual, rest-swept, or a triggered
/// conditional order) is appended here so the UI can show a trades table without
/// computing anything itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillLog {
    /// Symbol traded.
    pub symbol: Symbol,
    /// The order id this fill belongs to.
    pub order_id: String,
    /// Buy or sell.
    pub side: Side,
    /// Order type that produced this fill (market, limit, stop, ...).
    pub order_type: OrderType,
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
    /// Optional take-profit trigger price.
    pub tp_price: Option<f64>,
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
            tp_price: None,
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
            tp_price: None,
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
    triggered_order_sources: HashMap<String, String>,
    /// Chronological log of every fill against the account's own orders.
    fill_log: Vec<FillLog>,
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
            triggered_order_sources: HashMap::new(),
            fill_log: Vec::new(),
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
        if self.market_kinds.get(&symbol.symbol) == Some(&kind) {
            return;
        }
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

    /// The active fee schedule (read-only, for clients that size by risk).
    pub fn fee_schedule(&self) -> FeeSchedule {
        self.fees
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

    /// Seed the order book with a synthetic single bid/ask level at `price`
    /// (midpoint). Used during bar replay when there is no external book, so
    /// market and triggered orders always have a reference price.
    pub fn update_midpoint(&mut self, symbol: &str, price: f64, depth: f64) {
        if let Some(engine) = self.engine_mut(symbol) {
            engine.set_midpoint(price, depth);
        }
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
                let conditional_order_id = conditional.id;
                self.next_conditional_id += 1;
                self.conditional.push(conditional);
                // Optional take-profit conditional.
                if let Some(tp) = request.tp_price {
                    let tp_cond = ConditionalOrder {
                        id: self.next_conditional_id,
                        symbol: request.symbol.clone(),
                        side: if request.side == Side::Buy { Side::Sell } else { Side::Buy },
                        order_type: OrderType::TakeProfit,
                        amount: request.amount,
                        price: None,
                        trigger_price: tp,
                        reduce_only: true,
                        timestamp,
                    };
                    self.next_conditional_id += 1;
                    self.conditional.push(tp_cond);
                }
                return Ok(Order {
                    id: format!("conditional_{conditional_order_id}"),
                    symbol: request.symbol,
                    side: request.side,
                    order_type: request.order_type,
                    price: request.price.or(Some(trigger_price)),
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
            OrderType::Limit => request.price.ok_or_else(|| {
                BrokerError::InvalidOrderType("limit order requires a price".into())
            })?,
            OrderType::Market => self.market_reference_price(&symbol, request.side).unwrap_or(0.0),
            _ => request.price.unwrap_or(0.0),
        };

        // reduce_only may never grow exposure: cap the order at the open
        // position it is allowed to close (0 => nothing to do). Without this a
        // stale sibling SL/TP that triggers on the same bar as its twin would
        // open a position in the opposite direction.
        let mut request = request;
        if request.reduce_only && kind == MarketKind::Linear {
            let held = self
                .portfolio
                .position(&symbol.symbol)
                .filter(|pos| match (pos.side, request.side) {
                    (PositionSide::Long, Side::Sell) => true,
                    (PositionSide::Short, Side::Buy) => true,
                    _ => false,
                })
                .map(|pos| pos.qty)
                .unwrap_or(0.0);
            if held <= f64::EPSILON {
                return Err(BrokerError::InvalidOrderType(
                    "reduce_only order with no open position".into(),
                ));
            }
            if request.amount > held {
                request.amount = held;
            }
        }

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
                self.settle_linear(&symbol, request.side, lock, filled_amount, filled_notional, fee_total, timestamp);
            }
        }

        self.log_fills(&report, timestamp);

        Ok(report.order)
    }

    /// Append execution fills to the account fill log (source of truth for the
    /// UI trades table).
    fn log_fills(&mut self, report: &ExecutionReport, _timestamp: DateTime<Utc>) {
        let symbol = report.order.symbol.clone();
        let order_id = report.order.id.clone();
        for f in &report.fills {
            self.fill_log.push(FillLog {
                symbol: symbol.clone(),
                order_id: order_id.clone(),
                side: report.order.side,
                order_type: report.order.order_type,
                price: f.price,
                amount: f.amount,
                fee: f.fee,
                maker: f.maker,
                timestamp: f.timestamp,
            });
        }
    }

    /// Reference price for a market order (best bid/ask from the book).
    fn market_reference_price(&self, symbol: &Symbol, side: Side) -> Option<f64> {
        let engine = self.engine(&symbol.symbol)?;
        match side {
            Side::Buy => engine.book().best_ask().map(|a| a.price),
            Side::Sell => engine.book().best_bid().map(|b| b.price),
        }
    }

    /// Execute a market order at an explicit reference price (used when there
    /// is no booked liquidity, e.g. fills at the latest simulation bar close).
    ///
    /// The order is fully filled at `price` taker-side, applying fees and
    /// settling into the wallet / portfolio exactly like a booked fill.
    pub fn execute_market_at(
        &mut self,
        symbol: &Symbol,
        side: Side,
        amount: f64,
        price: f64,
        timestamp: DateTime<Utc>,
    ) -> Result<Order, BrokerError> {
        if amount <= 0.0 {
            return Err(BrokerError::InvalidAmount(amount));
        }
        if !self.engines.contains_key(&symbol.symbol) {
            return Err(BrokerError::UnknownSymbol(symbol.symbol.clone()));
        }
        let kind = self.market_kinds.get(&symbol.symbol).copied().unwrap();

        self.validate_market_funds(symbol, kind, side, price, amount)?;

        let engine_request = OrderRequest {
            side,
            order_type: OrderType::Market,
            price: Some(price),
            amount,
            time_in_force: TimeInForce::GTC,
            reduce_only: false,
            post_only: false,
        };
        let report = {
            let engine = self.engine_mut(&symbol.symbol).unwrap();
            engine.record_fill(engine_request, price, timestamp)
        };

        let filled_amount = report.order.filled;
        let filled_notional = filled_amount * price;
        let fee_total = report.fills.iter().map(|f| f.fee).sum();

        match kind {
            MarketKind::Spot => {
                self.settle_spot(symbol, side, None, filled_amount, filled_notional, fee_total);
            }
            MarketKind::Linear => {
                self.settle_linear(symbol, side, None, filled_amount, filled_notional, fee_total, timestamp);
            }
        }

        self.log_fills(&report, timestamp);

        Ok(report.order)
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
        timestamp: DateTime<Utc>,
    ) {
        if let Some(lock) = &lock {
            let release = filled_amount * lock.unit_release;
            self.wallet.unlock(&lock.asset, release);
        }
        if filled_amount > 0.0 {
            let avg_price = filled_notional / filled_amount;
            // Stamp the open time so the chart can show where the position
            // actually was entered (survives adds, resets on a flip).
            self.portfolio.apply_fill_at(
                symbol,
                side,
                filled_amount,
                avg_price,
                Some(timestamp.timestamp_millis()),
            );
        }
        self.wallet.apply_fill(&symbol.quote, -fee_total);
        self.wallet.add_fee(fee_total);
    }

    /// Cancel an open order.
    pub fn cancel_order(&mut self, symbol: &str, id: &str) -> Result<Order, BrokerError> {
        if let Some(raw_id) = id.strip_prefix("conditional_") {
            let conditional_id = raw_id.parse::<u64>()
                .map_err(|_| BrokerError::UnknownOrder(id.to_string()))?;
            let index = self.conditional.iter().position(|c| {
                c.id == conditional_id && c.symbol.symbol == symbol
            }).ok_or_else(|| BrokerError::UnknownOrder(id.to_string()))?;
            let conditional = self.conditional.remove(index);
            return Ok(Order {
                id: id.to_string(),
                symbol: conditional.symbol,
                side: conditional.side,
                order_type: conditional.order_type,
                price: conditional.price.or(Some(conditional.trigger_price)),
                amount: conditional.amount,
                remaining: conditional.amount,
                filled: 0.0,
                average_price: 0.0,
                status: OrderStatus::Canceled,
                time_in_force: TimeInForce::GTC,
                reduce_only: conditional.reduce_only,
                post_only: false,
                timestamp: conditional.timestamp,
                fee: 0.0,
            });
        }
        let engine = self.engine_mut(symbol).ok_or_else(|| BrokerError::UnknownSymbol(symbol.to_string()))?;
        let order = engine.cancel_order(id).ok_or_else(|| BrokerError::UnknownOrder(id.to_string()))?;
        self.release_locked(symbol, &order);
        Ok(order)
    }

    /// Active trigger orders (stop / take-profit) not held by a matching engine.
    pub fn conditional_orders(&self, symbol: Option<&str>) -> Vec<ConditionalOrder> {
        self.conditional.iter()
            .filter(|order| symbol.map(|s| s == order.symbol.symbol).unwrap_or(true))
            .cloned()
            .collect()
    }

    /// Return and forget the trigger order that produced an execution fill.
    pub fn take_triggered_order_source(&mut self, order_id: &str) -> Option<String> {
        self.triggered_order_sources.remove(order_id)
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

    /// Sweep resting limit orders crossed by a fresh `price` (call on every
    /// price update, e.g. each replayed bar).
    ///
    /// A resting buy fills when the market trades at or below its limit; a
    /// resting sell when at or above. Fills match against the current book as
    /// maker and settle against the funds locked at placement (no double
    /// locking). Returns each order that gained fills with the amount filled
    /// by THIS sweep (for cost-basis tracking).
    pub fn sweep_resting(
        &mut self,
        symbol: &Symbol,
        price: f64,
        timestamp: DateTime<Utc>,
    ) -> Vec<(Order, f64)> {
        let kind = self.market_kinds.get(&symbol.symbol).copied().unwrap_or(MarketKind::Spot);
        // Collect crossed resting limits first (borrow-safe snapshot of ids).
        let ids: Vec<(String, Side, f64, f64)> = self
            .engines
            .get(&symbol.symbol)
            .map(|e| {
                e.open_orders()
                    .filter(|o| o.order_type == OrderType::Limit && o.is_open())
                    .filter(|o| {
                        o.price.map(|limit| match o.side {
                            Side::Buy => price <= limit,
                            Side::Sell => price >= limit,
                        })
                        .unwrap_or(false)
                    })
                    .map(|o| (o.id.clone(), o.side, o.price.unwrap_or(0.0), o.remaining))
                    .collect()
            })
            .unwrap_or_default();

        let mut executed = Vec::new();
        for (id, side, limit, remaining) in ids {
            if remaining <= 0.0 {
                continue;
            }
            // Rebuild the lock placed at order creation (same rules).
            // Computed before any mutable borrow.
            let lock = self.sweep_lock(symbol, kind, side, limit);
            let (mut order, fills, leftover, fee_total, filled_amount, filled_notional) = {
                let engine = match self.engine_mut(&symbol.symbol) {
                    Some(e) => e,
                    None => break,
                };
                let Some(order) = engine.take_open_order(&id) else {
                    continue;
                };
                let (fills, leftover) = engine.consume_limit(side, limit, remaining);
                if fills.is_empty() {
                    engine.put_open_order(order);
                    continue;
                }
                let mut filled_amount = 0.0;
                let mut filled_notional = 0.0;
                let mut fee_total = 0.0;
                let mut recs = Vec::new();
                for (fill_price, amount) in fills {
                    let fee = engine.fee_for(amount, fill_price, true);
                    filled_amount += amount;
                    filled_notional += amount * fill_price;
                    fee_total += fee;
                    recs.push(Fill {
                        order_id: order.id.clone(),
                        price: fill_price,
                        amount,
                        fee,
                        maker: true,
                        timestamp,
                    });
                }
                (order, recs, leftover, fee_total, filled_amount, filled_notional)
            };
            match kind {
                MarketKind::Spot => {
                    self.settle_spot(symbol, side, lock, filled_amount, filled_notional, fee_total);
                }
                MarketKind::Linear => {
                    self.settle_linear(symbol, side, lock, filled_amount, filled_notional, fee_total, timestamp);
                }
            }
            for f in &fills {
                self.fill_log.push(FillLog {
                    symbol: symbol.clone(),
                    order_id: order.id.clone(),
                    side,
                    order_type: order.order_type,
                    price: f.price,
                    amount: f.amount,
                    fee: f.fee,
                    maker: true,
                    timestamp: f.timestamp,
                });
            }
            // Weighted average across earlier partial fills.
            let total_filled = order.filled + filled_amount;
            order.average_price = if total_filled > 0.0 {
                (order.average_price * order.filled + filled_notional) / total_filled
            } else {
                0.0
            };
            order.filled = total_filled;
            order.remaining = leftover;
            order.fee += fee_total;
            if leftover <= 0.0 {
                order.status = OrderStatus::Filled;
            } else {
                order.status = OrderStatus::PartialFilled;
                if let Some(engine) = self.engine_mut(&symbol.symbol) {
                    engine.put_open_order(order.clone());
                }
            }
            executed.push((order, filled_amount));
        }
        executed
    }

    /// Rebuild the fund lock of a resting limit order for sweep settlement.
    /// Mirrors `lock_funds` exactly (same asset, same per-unit release).
    fn sweep_lock(
        &self,
        symbol: &Symbol,
        kind: MarketKind,
        side: Side,
        limit: f64,
    ) -> Option<LockInfo> {
        match kind {
            MarketKind::Spot => match side {
                Side::Buy => Some(LockInfo { asset: symbol.quote.clone(), unit_release: limit }),
                Side::Sell => Some(LockInfo { asset: symbol.base.clone(), unit_release: 1.0 }),
            },
            MarketKind::Linear => {
                let leverage = self.portfolio.leverage(&symbol.symbol);
                Some(LockInfo { asset: symbol.quote.clone(), unit_release: limit / leverage })
            }
        }
    }

    /// Notify the broker of a price update, triggering any ready conditional orders.
    ///
    /// Returns the orders that were triggered and executed.
    pub fn on_price_update(&mut self, symbol: &Symbol, price: f64, timestamp: DateTime<Utc>) -> Vec<Order> {        let mut executed = Vec::new();
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
            let triggered_market = matches!(
                c.order_type,
                OrderType::Stop | OrderType::TakeProfit | OrderType::TrailingStop
            );
            // A trigger ENTRY (a breakout order, not reduce-only) fills at its
            // trigger price. The trader sized the position from that price, so
            // filling at whatever the market happens to be would move the entry
            // away from the planned risk by up to the whole bar's range.
            // Protective stop-losses in this replay fill at their trigger so a
            // candle gap cannot turn the configured risk into a larger loss.
            // Other reduce-only exits still use the current market reference.
            let capped_stop_loss =
                c.order_type == OrderType::Stop && c.reduce_only && triggered_market;
            let fill_reference = if triggered_market && (!c.reduce_only || capped_stop_loss) {
                Some(c.trigger_price)
            } else {
                None
            };
            if let Some(reference) = fill_reference {
                // Seed the reference book at the trigger so the market order
                // settles there (same path a resting limit would take).
                self.update_midpoint(&c.symbol.symbol, reference, 1e12);
            }
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
                tp_price: None,
                time_in_force: TimeInForce::GTC,
                reduce_only: c.reduce_only,
                post_only: false,
            };
            if let Ok(order) = self.execute_immediate(request, timestamp) {
                self.triggered_order_sources.insert(
                    order.id.clone(),
                    format!("conditional_{}", c.id),
                );
                executed.push(order);
            }
            if capped_stop_loss {
                // The trigger price is only for this protective fill; do not
                // leave the simulated book behind the actual replay price.
                self.update_midpoint(&c.symbol.symbol, price, 1e12);
            }
        }

        executed
    }

    /// Registered market symbols (have a matching engine).
    pub fn symbols(&self) -> Vec<String> {
        self.engines.keys().cloned().collect()
    }

    /// Open orders for a symbol.
    pub fn open_orders(&self, symbol: &str) -> Vec<Order> {
        self.engines
            .get(symbol)
            .map(|e| e.open_orders().cloned().collect())
            .unwrap_or_default()
    }

    /// The account's fill log (chronological). Optionally filtered to one symbol.
    pub fn fills(&self, symbol: Option<&str>) -> Vec<FillLog> {
        self.fill_log
            .iter()
            .filter(|f| symbol.map(|s| f.symbol.symbol.as_str() == s).unwrap_or(true))
            .cloned()
            .collect()
    }

    /// Number of recorded fills. O(1): the hot path only needs the cursor.
    pub fn fills_len(&self) -> usize {
        self.fill_log.len()
    }

    /// One fill by chronological index. O(1), no clone — used to build only
    /// the new tail of the fill log instead of cloning and serializing all of it.
    pub fn fill_at(&self, index: usize) -> Option<&FillLog> {
        self.fill_log.get(index)
    }
}

/// Whether a conditional order is triggered at the given price.
///
/// Direction depends on both the side and the order type:
/// - Stop-type orders (stop / stop-limit / trailing-stop) trigger toward the
///   trigger price from the prevailing price:
///     * a BUY stop triggers when price rises to the stop  (`price >= trigger`),
///     * a SELL stop triggers when price falls to the stop (`price <= trigger`).
/// - Take-profit-type orders move the opposite way:
///     * a BUY take-profit triggers when price falls   (`price <= trigger`),
///     * a SELL take-profit triggers when price rises   (`price >= trigger`).
fn is_triggered(c: &ConditionalOrder, price: f64) -> bool {
    let is_take_profit = matches!(
        c.order_type,
        OrderType::TakeProfit | OrderType::TakeProfitLimit
    );
    match c.side {
        Side::Buy => {
            if is_take_profit {
                price <= c.trigger_price
            } else {
                price >= c.trigger_price
            }
        }
        Side::Sell => {
            if is_take_profit {
                price >= c.trigger_price
            } else {
                price <= c.trigger_price
            }
        }
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
    fn reduce_only_never_opens_or_flips() {
        // A stale sibling SL/TP that triggers on the same bar as its twin
        // must not open exposure in the opposite direction.
        let mut b = Broker::new();
        b.add_market(&btc(), MarketKind::Linear, ts());
        b.wallet_mut().deposit("USDT", 100_000.0);
        b.engine_mut("BTC/USDT").unwrap().update_ask(100.0, 10.0);

        // No position at all: reduce_only is rejected outright.
        let mut req = BrokerOrderRequest::market(btc(), Side::Sell, 1.0);
        req.reduce_only = true;
        assert!(b.submit_order(req, ts()).is_err());
        assert!(b.portfolio().positions().is_empty());

        // Long 1.0, then a reduce_only sell for 5.0: capped at the position.
        b.submit_order(BrokerOrderRequest::market(btc(), Side::Buy, 1.0), ts())
            .unwrap();
        b.engine_mut("BTC/USDT").unwrap().update_bid(100.0, 10.0);
        let mut over = BrokerOrderRequest::market(btc(), Side::Sell, 5.0);
        over.reduce_only = true;
        b.submit_order(over, ts()).unwrap();
        assert!(
            b.portfolio().position("BTC/USDT").is_none(),
            "reduce_only oversell must close, not flip to short"
        );

        // Same on the short side.
        b.submit_order(BrokerOrderRequest::market(btc(), Side::Sell, 2.0), ts())
            .unwrap();
        assert_eq!(b.portfolio().position("BTC/USDT").unwrap().side, PositionSide::Short);
        b.engine_mut("BTC/USDT").unwrap().update_ask(100.0, 10.0);
        let mut over_buy = BrokerOrderRequest::market(btc(), Side::Buy, 9.0);
        over_buy.reduce_only = true;
        b.submit_order(over_buy, ts()).unwrap();
        assert!(
            b.portfolio().position("BTC/USDT").is_none(),
            "reduce_only overbuy must close, not flip to long"
        );
    }

    #[test]
    fn trigger_entry_fills_at_its_trigger_price() {
        // Regression: a breakout (stop) entry was executed as a market order at
        // whatever the market was when the trigger fired, so the position opened
        // away from the price the trader sized the risk from.
        let mut b = Broker::new();
        b.add_market(&btc(), MarketKind::Linear, ts());
        b.wallet_mut().deposit("USDT", 100_000.0);
        b.engine_mut("BTC/USDT").unwrap().update_ask(100.0, 10.0);

        let mut req = BrokerOrderRequest::market(btc(), Side::Buy, 1.0);
        req.order_type = OrderType::Stop;
        req.stop_price = Some(110.0);
        let order = b.submit_order(req, ts()).unwrap();
        assert_eq!(order.status, OrderStatus::New, "the trigger rests first");

        // The market jumps past the trigger in one update.
        b.on_price_update(&btc(), 130.0, ts());
        let pos = b.portfolio().position("BTC/USDT").expect("the trigger opened a position");
        assert!(
            (pos.entry_price - 110.0).abs() < 1e-9,
            "a trigger entry must fill at its trigger, got {}",
            pos.entry_price
        );
    }

    #[test]
    fn protective_stop_fills_at_its_trigger_to_cap_replay_loss() {
        let mut b = Broker::new();
        b.add_market(&btc(), MarketKind::Linear, ts());
        b.wallet_mut().deposit("USDT", 100_000.0);
        b.engine_mut("BTC/USDT").unwrap().update_ask(100.0, 10.0);
        b.submit_order(BrokerOrderRequest::market(btc(), Side::Buy, 1.0), ts()).unwrap();
        // A sell needs a bid to reference (the replay seeds both sides itself).
        b.engine_mut("BTC/USDT").unwrap().update_bid(100.0, 10.0);

        let mut stop = BrokerOrderRequest::market(btc(), Side::Sell, 1.0);
        stop.order_type = OrderType::Stop;
        stop.stop_price = Some(90.0);
        stop.reduce_only = true;
        b.submit_order(stop, ts()).unwrap();

        let executed = b.on_price_update(&btc(), 80.0, ts());
        assert!(
            b.portfolio().position("BTC/USDT").is_none(),
            "the protective stop must have closed the position"
        );
        assert_eq!(executed.len(), 1);
        assert!(
            (executed[0].average_price - 90.0).abs() < 1e-9,
            "protective stop should fill at the stop, not the gapped price: {:?}",
            executed[0]
        );
    }

    #[test]
    fn protective_short_stop_fills_at_its_trigger_after_a_gap() {
        let mut b = Broker::new();
        b.add_market(&btc(), MarketKind::Linear, ts());
        b.wallet_mut().deposit("USDT", 100_000.0);
        b.engine_mut("BTC/USDT").unwrap().update_bid(100.0, 10.0);
        b.submit_order(BrokerOrderRequest::market(btc(), Side::Sell, 1.0), ts()).unwrap();

        let mut stop = BrokerOrderRequest::market(btc(), Side::Buy, 1.0);
        stop.order_type = OrderType::Stop;
        stop.stop_price = Some(110.0);
        stop.reduce_only = true;
        b.submit_order(stop, ts()).unwrap();

        let executed = b.on_price_update(&btc(), 120.0, ts());
        assert_eq!(executed.len(), 1);
        assert!(
            (executed[0].average_price - 110.0).abs() < 1e-9,
            "short protective stop should fill at the stop after a gap: {:?}",
            executed[0]
        );
        assert!(b.portfolio().position("BTC/USDT").is_none());
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
        assert!(order.id.starts_with("conditional_"));
        assert!(b.on_price_update(&btc(), 105.0, ts()).is_empty());
        let exec = b.on_price_update(&btc(), 111.0, ts());
        assert_eq!(exec.len(), 1);
        assert_eq!(exec[0].status, OrderStatus::Filled);
        assert_eq!(b.take_triggered_order_source(&exec[0].id), Some(order.id));
    }

    #[test]
    fn test_conditional_order_can_be_cancelled() {
        let mut b = spot_broker();
        let mut req = BrokerOrderRequest::market(btc(), Side::Sell, 1.0);
        req.order_type = OrderType::Stop;
        req.stop_price = Some(90.0);
        let order = b.submit_order(req, ts()).unwrap();
        assert_eq!(b.conditional_orders(Some("BTC/USDT")).len(), 1);
        let canceled = b.cancel_order("BTC/USDT", &order.id).unwrap();
        assert_eq!(canceled.status, OrderStatus::Canceled);
        assert!(b.conditional_orders(Some("BTC/USDT")).is_empty());
    }

    #[test]
    fn registering_existing_market_preserves_protective_orders() {
        let mut b = spot_broker();
        let mut request = BrokerOrderRequest::market(btc(), Side::Sell, 1.0);
        request.order_type = OrderType::Stop;
        request.stop_price = Some(90.0);
        let stop = b.submit_order(request, ts()).unwrap();

        b.add_market(&btc(), MarketKind::Spot, ts());

        assert_eq!(b.conditional_orders(Some("BTC/USDT")).len(), 1);
        assert!(b.cancel_order("BTC/USDT", &stop.id).is_ok());
    }

    #[test]
    fn test_conditional_take_profit_sell_triggers_on_rise() {
        let mut b = spot_broker();
        // Mirror replay: the bar close seeds a synthetic bid+ask midpoint.
        b.update_midpoint("BTC/USDT", 100.0, 1e12);
        let mut req = BrokerOrderRequest::market(btc(), Side::Sell, 0.5);
        req.order_type = OrderType::TakeProfit;
        req.stop_price = Some(110.0);
        let order = b.submit_order(req, ts()).unwrap();
        assert_eq!(order.status, OrderStatus::New);
        // A sell take-profit triggers when price rises to the TP level.
        assert!(b.on_price_update(&btc(), 109.0, ts()).is_empty());
        let exec = b.on_price_update(&btc(), 111.0, ts());
        assert_eq!(exec.len(), 1);
        assert_eq!(exec[0].status, OrderStatus::Filled);
    }

    #[test]
    fn test_fill_log_records_executions() {        let mut b = spot_broker();
        // Market buy against the book -> a taker fill is logged.
        b.engine_mut("BTC/USDT").unwrap().update_ask(100.0, 1.0);
        b.submit_order(BrokerOrderRequest::market(btc(), Side::Buy, 1.0), ts()).unwrap();
        // Market sell at an explicit reference price (no booked liquidity path).
        b.update_midpoint("BTC/USDT", 101.0, 1e12);
        b.submit_order(
            BrokerOrderRequest {
                symbol: btc(),
                side: Side::Sell,
                order_type: OrderType::Market,
                amount: 0.5,
                price: None,
                stop_price: None,
                tp_price: None,
                time_in_force: TimeInForce::GTC,
                reduce_only: false,
                post_only: false,
            },
            ts(),
        )
        .unwrap();
        let fills = b.fills(None);
        // The sell went through the book's synthetic midpoint, so it consumed
        // book bid liquidity (a real fill). At least the first buy is recorded.
        assert!(!fills.is_empty(), "expected at least one recorded fill");
        assert_eq!(fills[0].side, Side::Buy);
        assert_eq!(fills[0].price, 100.0);
        // Filter by symbol.
        let btc_fills = b.fills(Some("BTC/USDT"));
        assert_eq!(btc_fills.len(), fills.len());
    }

    #[test]
    fn test_sweep_fills_crossed_resting_limit() {
        let mut b = spot_broker();
        // Resting buy limit at 90 (no cross against empty book).
        let order = b
            .submit_order(BrokerOrderRequest::limit(btc(), Side::Buy, 90.0, 1.0), ts())
            .unwrap();
        assert_eq!(order.status, OrderStatus::New);
        // Price still above: no fill.
        assert!(b.sweep_resting(&btc(), 100.0, ts()).is_empty());
        // Price drops through the limit: fills as maker at the market price.
        b.update_midpoint("BTC/USDT", 89.0, 1e12);
        let swept = b.sweep_resting(&btc(), 89.0, ts());
        assert_eq!(swept.len(), 1);
        let (filled_order, new_filled) = &swept[0];
        assert_eq!(filled_order.status, OrderStatus::Filled);
        assert_eq!(*new_filled, 1.0);
        assert_eq!(filled_order.average_price, 89.0);
        assert!(b.open_orders("BTC/USDT").is_empty());
        // Fill is logged and the base asset landed in the wallet.
        assert_eq!(b.fills(None).len(), 1);
        assert_eq!(b.wallet().balance("BTC").unwrap().total, 11.0);
    }

    #[test]
    fn test_sweep_partial_fill_keeps_remainder_open() {
        let mut b = spot_broker();
        b.submit_order(BrokerOrderRequest::limit(btc(), Side::Buy, 90.0, 2.0), ts()).unwrap();
        // Thin book: only 0.5 available at the touch.
        b.engine_mut("BTC/USDT").unwrap().set_midpoint(89.0, 0.5);
        let swept = b.sweep_resting(&btc(), 89.0, ts());
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].1, 0.5);
        assert_eq!(swept[0].0.status, OrderStatus::PartialFilled);
        assert_eq!(b.open_orders("BTC/USDT").len(), 1);
        // Locked funds still cover the remainder: cancel releases them.
        let id = b.open_orders("BTC/USDT")[0].id.clone();
        b.cancel_order("BTC/USDT", &id).unwrap();
        assert_eq!(b.wallet().balance("USDT").unwrap().locked, 0.0);
    }

    #[test]
    fn test_sweep_resting_sell_fills_on_rise() {
        let mut b = spot_broker();
        let order = b
            .submit_order(BrokerOrderRequest::limit(btc(), Side::Sell, 110.0, 1.0), ts())
            .unwrap();
        assert_eq!(order.status, OrderStatus::New);
        b.update_midpoint("BTC/USDT", 111.0, 1e12);
        let swept = b.sweep_resting(&btc(), 111.0, ts());
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].0.status, OrderStatus::Filled);
        assert_eq!(b.wallet().balance("BTC").unwrap().total, 9.0);
    }
}
