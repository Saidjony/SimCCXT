//! Exchange for simccxt.
//!
//! The CCXT-compatible facade that owns the simulator's market, time, balances,
//! orders, and positions. Strategies interact only with this API.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use simccxt_broker::{Broker, BrokerError, BrokerOrderRequest, MarketKind};
use simccxt_clock::Clock;
use simccxt_core::{
    Balance, Config, OHLCV, OrderStatus, OrderType, Side, Symbol, TimeInForce, Trade,
};
use simccxt_event_bus::{Event, EventBus};
use simccxt_market::{FundingRate, OpenInterest, OrderBook, Ticker};

/// Additional parameters for `create_order`.
#[derive(Debug, Clone, Default)]
pub struct CreateOrderParams {
    /// Trigger price for stop/stop-limit/take-profit orders.
    pub stop_price: Option<f64>,
    /// Time in force.
    pub time_in_force: TimeInForce,
    /// Reduce-only flag.
    pub reduce_only: bool,
    /// Post-only flag.
    pub post_only: bool,
}

impl CreateOrderParams {
    /// Default params.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the stop price.
    pub fn stop_price(mut self, price: f64) -> Self {
        self.stop_price = Some(price);
        self
    }

    /// Set time in force.
    pub fn time_in_force(mut self, tif: TimeInForce) -> Self {
        self.time_in_force = tif;
        self
    }

    /// Set reduce-only.
    pub fn reduce_only(mut self, reduce_only: bool) -> Self {
        self.reduce_only = reduce_only;
        self
    }

    /// Set post-only.
    pub fn post_only(mut self, post_only: bool) -> Self {
        self.post_only = post_only;
        self
    }
}

/// The virtual exchange.
pub struct Exchange {
    clock: Clock,
    bus: EventBus,
    broker: Broker,
    candles: HashMap<(String, String), Vec<OHLCV>>,
    trades: HashMap<String, Vec<Trade>>,
    tickers: HashMap<String, Ticker>,
    funding_rates: HashMap<String, FundingRate>,
    open_interest: HashMap<String, OpenInterest>,
    last_price: HashMap<String, f64>,
}

impl Default for Exchange {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl Exchange {
    /// Create an exchange with the given configuration.
    pub fn new(config: Config) -> Self {
        Self {
            clock: Clock::new(&config),
            bus: EventBus::new(),
            broker: Broker::new(),
            candles: HashMap::new(),
            trades: HashMap::new(),
            tickers: HashMap::new(),
            funding_rates: HashMap::new(),
            open_interest: HashMap::new(),
            last_price: HashMap::new(),
        }
    }

    // ------------------------------------------------------------------
    // Market registration
    // ------------------------------------------------------------------

    /// Register a market with the exchange.
    pub fn add_market(&mut self, symbol: &Symbol, kind: MarketKind) {
        self.broker.add_market(symbol, kind, self.clock.now());
    }

    /// Deposit funds into the exchange wallet.
    pub fn deposit(&mut self, asset: &str, amount: f64) {
        self.broker.wallet_mut().deposit(asset, amount);
    }

    /// Load historical OHLCV candles into the exchange.
    pub fn load_candles(&mut self, symbol: &str, timeframe: &str, candles: Vec<OHLCV>) {
        self.candles.insert((symbol.to_string(), timeframe.to_string()), candles);
    }

    /// Load historical trades into the exchange.
    pub fn load_trades(&mut self, symbol: &str, trades: Vec<Trade>) {
        for t in &trades {
            self.last_price.insert(symbol.to_string(), t.price);
        }
        self.trades.insert(symbol.to_string(), trades);
    }

    /// Set the current last price for a symbol (e.g. from live data).
    pub fn set_last_price(&mut self, symbol: &str, price: f64) {
        self.last_price.insert(symbol.to_string(), price);
    }

    /// Set a funding rate for a symbol.
    pub fn set_funding_rate(&mut self, symbol: &str, rate: FundingRate) {
        self.funding_rates.insert(symbol.to_string(), rate);
    }

    /// Set open interest for a symbol.
    pub fn set_open_interest(&mut self, symbol: &str, oi: OpenInterest) {
        self.open_interest.insert(symbol.to_string(), oi);
    }

    // ------------------------------------------------------------------
    // Time control (delegated to the clock)
    // ------------------------------------------------------------------

    /// The current simulation time.
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    /// The current simulation time in milliseconds.
    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Advance the clock by one step.
    pub fn step(&mut self) -> bool {
        self.clock.step()
    }

    /// Advance the clock by `n` steps.
    pub fn step_n(&mut self, n: u64) -> u64 {
        self.clock.step_n(n)
    }

    /// Seek the clock to a timestamp.
    pub fn seek(&mut self, timestamp: DateTime<Utc>) -> bool {
        self.clock.seek(timestamp)
    }

    /// Run the clock.
    pub fn run(&mut self) -> bool {
        self.clock.run()
    }

    /// Pause the clock.
    pub fn pause(&mut self) {
        self.clock.pause()
    }

    /// Resume the clock.
    pub fn resume(&mut self) {
        self.clock.resume()
    }

    /// Reset the clock.
    pub fn reset(&mut self, timestamp: DateTime<Utc>) {
        self.clock.reset(timestamp)
    }

    /// The event bus (for subscribing to simulator events).
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// The broker (typed access).
    pub fn broker(&self) -> &Broker {
        &self.broker
    }

    /// The broker (mutable access).
    pub fn broker_mut(&mut self) -> &mut Broker {
        &mut self.broker
    }

    // ------------------------------------------------------------------
    // CCXT-compatible public API
    // ------------------------------------------------------------------

    /// `fetch_ohlcv` — returns `[[timestamp, open, high, low, close, volume], ...]`.
    pub fn fetch_ohlcv(
        &self,
        symbol: &str,
        timeframe: &str,
        since: Option<i64>,
        limit: Option<usize>,
    ) -> Vec<Vec<f64>> {
        let candles = self
            .candles
            .get(&(symbol.to_string(), timeframe.to_string()))
            .cloned()
            .unwrap_or_default();

        let mut rows: Vec<Vec<f64>> = candles
            .iter()
            .filter(|c| since.map(|s| c.timestamp.timestamp_millis() >= s).unwrap_or(true))
            .map(|c| {
                vec![
                    c.timestamp.timestamp_millis() as f64,
                    c.open,
                    c.high,
                    c.low,
                    c.close,
                    c.volume,
                ]
            })
            .collect();

        if let Some(limit) = limit {
            rows = rows.into_iter().rev().take(limit).rev().collect();
        }
        rows
    }

    /// Set the simulator clock step size in milliseconds.
    pub fn set_step_ms(&mut self, ms: u64) {
        self.clock.set_step_ms(ms);
    }

    /// `fetch_ohlcv_live` — returns OHLCV rows only for candles with
    /// `timestamp <= current simulator time`. Combined with `step()`,
    /// `reset()`, `pause()`, `resume()` this gives step-by-step delivery of
    /// stored data exactly like a live exchange feed, but sourced from local
    /// memory/disk instead of the internet.
    pub fn fetch_ohlcv_live(&self, symbol: &str, timeframe: &str) -> Vec<Vec<f64>> {
        let now = self.clock.now();
        self.candles
            .get(&(symbol.to_string(), timeframe.to_string()))
            .map(|candles| {
                candles
                    .iter()
                    .filter(|c| c.timestamp <= now)
                    .map(|c| {
                        vec![
                            c.timestamp.timestamp_millis() as f64,
                            c.open,
                            c.high,
                            c.low,
                            c.close,
                            c.volume,
                        ]
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether there are no more candles at or after the current simulator
    /// time for a symbol/timeframe (i.e. the replay has been consumed).
    pub fn replay_done(&self, symbol: &str, timeframe: &str) -> bool {
        let now = self.clock.now();
        self.candles
            .get(&(symbol.to_string(), timeframe.to_string()))
            .map(|c| c.last().map(|last| now >= last.timestamp).unwrap_or(true))
            .unwrap_or(true)
    }

    /// `fetch_ticker` — returns a CCXT-style ticker dict.
    pub fn fetch_ticker(&self, symbol: &str) -> Value {
        if let Some(ticker) = self.tickers.get(symbol) {
            return ticker_json(ticker);
        }
        // Build a ticker from the last candle and last price.
        let candles = self
            .candles
            .get(&(symbol.to_string(), "1m".to_string()))
            .or_else(|| self.candles.iter().find(|(k, _)| k.0 == symbol).map(|(_, v)| v));

        let last = self.last_price.get(symbol).copied();
        if let Some(candles) = candles {
            if let Some(last_candle) = candles.last() {
                let close = last.unwrap_or(last_candle.close);
                let high = candles.iter().map(|c| c.high).fold(f64::MIN, f64::max);
                let low = candles.iter().map(|c| c.low).fold(f64::MAX, f64::min);
                let base_volume: f64 = candles.iter().map(|c| c.volume).sum();
                let quote_volume: f64 = candles.iter().map(|c| c.volume * c.close).sum();
                let bid = self
                    .broker
                    .engine(symbol)
                    .and_then(|e| e.book().best_bid())
                    .map(|l| l.price)
                    .unwrap_or(close);
                let ask = self
                    .broker
                    .engine(symbol)
                    .and_then(|e| e.book().best_ask())
                    .map(|l| l.price)
                    .unwrap_or(close);
                let ticker = Ticker::new(
                    Symbol::parse(symbol).unwrap(),
                    close,
                    bid,
                    ask,
                    high,
                    low,
                    base_volume,
                    quote_volume,
                    last_candle.timestamp,
                );
                return ticker_json(&ticker);
            }
        }
        let ts = self.now();
        let ticker = Ticker::new(
            Symbol::parse(symbol).unwrap(),
            last.unwrap_or(0.0),
            last.unwrap_or(0.0),
            last.unwrap_or(0.0),
            last.unwrap_or(0.0),
            last.unwrap_or(0.0),
            0.0,
            0.0,
            ts,
        );
        ticker_json(&ticker)
    }

    /// `fetch_order_book` — returns a CCXT-style order book dict.
    pub fn fetch_order_book(&self, symbol: &str, limit: Option<usize>) -> Value {
        let empty = OrderBook::new(Symbol::parse(symbol).unwrap(), self.now());
        let book = self.broker.engine(symbol).map(|e| e.book()).unwrap_or(&empty);
        let bids: Vec<Vec<f64>> = book
            .bids
            .iter()
            .take(limit.unwrap_or(usize::MAX))
            .map(|l| vec![l.price, l.amount])
            .collect();
        let asks: Vec<Vec<f64>> = book
            .asks
            .iter()
            .take(limit.unwrap_or(usize::MAX))
            .map(|l| vec![l.price, l.amount])
            .collect();
        json!({
            "symbol": symbol,
            "bids": bids,
            "asks": asks,
            "timestamp": self.now_ms(),
            "datetime": self.now().to_rfc3339(),
            "nonce": json!(null),
        })
    }

    /// `fetch_trades` — returns a list of CCXT-style trade dicts.
    pub fn fetch_trades(&self, symbol: &str, limit: Option<usize>) -> Vec<Value> {
        let trades = self.trades.get(symbol).cloned().unwrap_or_default();
        trades
            .iter()
            .rev()
            .take(limit.unwrap_or(usize::MAX))
            .map(|t| {
                json!({
                    "id": t.trade_id,
                    "symbol": symbol,
                    "timestamp": t.timestamp.timestamp_millis(),
                    "datetime": t.timestamp.to_rfc3339(),
                    "side": if t.side == Side::Buy { "buy" } else { "sell" },
                    "price": t.price,
                    "amount": t.qty,
                    "cost": t.price * t.qty,
                })
            })
            .collect()
    }

    /// `fetch_balance` — returns a CCXT-style balance dict.
    pub fn fetch_balance(&self) -> Value {
        let mut free = json!({});
        let mut used = json!({});
        let mut total = json!({});
        let mut per_asset = json!({});

        for (asset, balance) in self.broker.wallet().balances() {
            free[asset] = json!(balance.available);
            used[asset] = json!(balance.locked);
            total[asset] = json!(balance.total);
            per_asset[asset] = json!({
                "free": balance.available,
                "used": balance.locked,
                "total": balance.total,
            });
        }

        json!({
            "info": json!(null),
            "free": free,
            "used": used,
            "total": total,
            "assets": per_asset,
        })
    }

    /// `fetch_positions` — returns a list of CCXT-style position dicts.
    pub fn fetch_positions(&self, symbols: Option<&[String]>) -> Vec<Value> {
        self.broker
            .portfolio()
            .positions()
            .iter()
            .filter(|(sym, _)| symbols.map(|s| s.contains(sym)).unwrap_or(true))
            .map(|(sym, pos)| {
                json!({
                    "symbol": sym,
                    "side": if pos.side == simccxt_core::PositionSide::Long { "long" } else { "short" },
                    "contracts": pos.qty,
                    "contractSize": 1.0,
                    "entryPrice": pos.entry_price,
                    "markPrice": pos.mark_price,
                    "unrealizedPnl": pos.unrealized_pnl,
                    "realizedPnl": pos.realized_pnl,
                    "liquidationPrice": pos.liquidation_price,
                    "leverage": self.broker.portfolio().leverage(sym),
                })
            })
            .collect()
    }

    /// `fetch_open_orders` — returns a list of CCXT-style order dicts.
    pub fn fetch_open_orders(&self, symbol: Option<&str>) -> Vec<Value> {
        let mut orders = Vec::new();
        if let Some(symbol) = symbol {
            for order in self.broker.open_orders(symbol) {
                if order.is_open() {
                    orders.push(order_json(&order));
                }
            }
        } else {
            // All symbols.
            for order in self.all_open_orders() {
                if order.is_open() {
                    orders.push(order_json(&order));
                }
            }
        }
        orders
    }

    fn all_open_orders(&self) -> Vec<simccxt_order_engine::Order> {
        // Iterate all registered symbols' engines.
        let symbols: Vec<String> = self
            .broker
            .portfolio()
            .positions()
            .keys()
            .cloned()
            .collect();
        let mut out = Vec::new();
        for symbol in symbols {
            out.extend(self.broker.open_orders(&symbol));
        }
        out
    }

    /// `fetch_funding_rate` — returns a CCXT-style funding rate dict.
    pub fn fetch_funding_rate(&self, symbol: &str) -> Value {
        if let Some(rate) = self.funding_rates.get(symbol) {
            return json!({
                "symbol": symbol,
                "fundingRate": rate.funding_rate,
                "timestamp": rate.timestamp.timestamp_millis(),
                "datetime": rate.timestamp.to_rfc3339(),
            });
        }
        json!({
            "symbol": symbol,
            "fundingRate": 0.0,
            "timestamp": self.now_ms(),
            "datetime": self.now().to_rfc3339(),
        })
    }

    /// `fetch_open_interest` — returns a CCXT-style open interest dict.
    pub fn fetch_open_interest(&self, symbol: &str) -> Value {
        if let Some(oi) = self.open_interest.get(symbol) {
            return json!({
                "symbol": symbol,
                "openInterestAmount": oi.open_interest,
                "openInterestValue": oi.open_interest,
                "timestamp": oi.timestamp.timestamp_millis(),
                "datetime": oi.timestamp.to_rfc3339(),
            });
        }
        json!({
            "symbol": symbol,
            "openInterestAmount": json!(null),
            "openInterestValue": json!(null),
            "timestamp": self.now_ms(),
            "datetime": self.now().to_rfc3339(),
        })
    }

    /// `create_order` — place an order and return a CCXT-style order dict.
    pub fn create_order(
        &mut self,
        symbol: &str,
        order_type: &str,
        side: &str,
        amount: f64,
        price: Option<f64>,
        params: CreateOrderParams,
    ) -> Result<Value, BrokerError> {
        let symbol = Symbol::parse(symbol).map_err(|_| {
            BrokerError::UnknownSymbol("invalid symbol".into())
        })?;
        let side = parse_side(side)?;
        let order_type = parse_order_type(order_type)?;

        let request = BrokerOrderRequest {
            symbol: symbol.clone(),
            side,
            order_type,
            amount,
            price,
            stop_price: params.stop_price,
            time_in_force: params.time_in_force,
            reduce_only: params.reduce_only,
            post_only: params.post_only,
        };

        let order = self.broker.submit_order(request, self.clock.now())?;

        // Emit events for fills.
        self.publish_order_events(&order);

        Ok(order_json(&order))
    }

    /// `cancel_order` — cancel an order and return a CCXT-style order dict.
    pub fn cancel_order(&mut self, id: &str, symbol: &str) -> Result<Value, BrokerError> {
        let order = self.broker.cancel_order(symbol, id)?;
        Ok(order_json(&order))
    }

    /// `fetch_balance_typed` — typed access to the wallet balances.
    pub fn balances(&self) -> Vec<Balance> {
        self.broker.wallet().balances().values().cloned().collect()
    }

    fn publish_order_events(&self, order: &simccxt_order_engine::Order) {
        if order.is_filled() || order.status == OrderStatus::PartialFilled {
            self.bus.publish(&Event::OrderFilled {
                symbol: order.symbol.clone(),
                order_id: order.id.clone(),
                price: order.average_price,
                filled_amount: order.filled,
                remaining: order.remaining,
                timestamp: order.timestamp,
            });
        }
    }
}

fn ticker_json(t: &Ticker) -> Value {
    json!({
        "symbol": t.symbol.symbol,
        "timestamp": t.timestamp.timestamp_millis(),
        "datetime": t.timestamp.to_rfc3339(),
        "high": t.high,
        "low": t.low,
        "bid": t.bid,
        "ask": t.ask,
        "last": t.last,
        "close": t.last,
        "baseVolume": t.base_volume,
        "quoteVolume": t.quote_volume,
        "percentage": t.percentage,
    })
}

fn order_json(order: &simccxt_order_engine::Order) -> Value {
    json!({
        "id": order.id,
        "symbol": order.symbol.symbol,
        "type": order_type_str(order.order_type),
        "side": if order.side == Side::Buy { "buy" } else { "sell" },
        "price": order.price,
        "amount": order.amount,
        "filled": order.filled,
        "remaining": order.remaining,
        "average": order.average_price,
        "status": order_status_str(order.status),
        "timestamp": order.timestamp.timestamp_millis(),
        "datetime": order.timestamp.to_rfc3339(),
        "fee": order.fee,
    })
}

fn order_type_str(t: OrderType) -> &'static str {
    match t {
        OrderType::Market => "market",
        OrderType::Limit => "limit",
        OrderType::Stop => "stop",
        OrderType::StopLimit => "stop_limit",
        OrderType::TakeProfit => "take_profit",
        OrderType::TakeProfitLimit => "take_profit_limit",
        OrderType::TrailingStop => "trailing_stop",
        OrderType::PostOnly => "post_only",
        OrderType::IOC => "ioc",
        OrderType::FOK => "fok",
    }
}

fn order_status_str(s: OrderStatus) -> &'static str {
    match s {
        OrderStatus::New => "open",
        OrderStatus::PartialFilled => "open",
        OrderStatus::Filled => "closed",
        OrderStatus::Canceled => "canceled",
        OrderStatus::Expired => "expired",
        OrderStatus::Rejected => "rejected",
    }
}

fn parse_side(s: &str) -> Result<Side, BrokerError> {
    match s.to_lowercase().as_str() {
        "buy" => Ok(Side::Buy),
        "sell" => Ok(Side::Sell),
        other => Err(BrokerError::InvalidOrderType(format!("unknown side: {other}"))),
    }
}

fn parse_order_type(s: &str) -> Result<OrderType, BrokerError> {
    match s.to_lowercase().as_str() {
        "market" => Ok(OrderType::Market),
        "limit" => Ok(OrderType::Limit),
        "stop" | "stop_market" => Ok(OrderType::Stop),
        "stop_limit" => Ok(OrderType::StopLimit),
        "take_profit" | "take_profit_market" => Ok(OrderType::TakeProfit),
        "take_profit_limit" => Ok(OrderType::TakeProfitLimit),
        "trailing_stop" => Ok(OrderType::TrailingStop),
        "post_only" => Ok(OrderType::PostOnly),
        "ioc" => Ok(OrderType::IOC),
        "fok" => Ok(OrderType::FOK),
        other => Err(BrokerError::InvalidOrderType(format!("unknown order type: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).unwrap()
    }

    fn candle(ms: i64, close: f64) -> OHLCV {
        OHLCV::new(ts(ms), close - 1.0, close + 1.0, close - 2.0, close, 10.0)
    }

    fn exchange_with_data() -> Exchange {
        let mut ex = Exchange::default();
        ex.add_market(&Symbol::parse("BTC/USDT").unwrap(), MarketKind::Spot);
        ex.deposit("USDT", 100_000.0);
        ex.deposit("BTC", 10.0);
        ex.load_candles(
            "BTC/USDT",
            "1m",
            vec![candle(1_704_067_200_000, 100.0), candle(1_704_067_260_000, 110.0)],
        );
        ex.broker_mut().update_bid("BTC/USDT", 100.0, 1.0);
        ex.broker_mut().update_ask("BTC/USDT", 101.0, 1.0);
        ex
    }

    #[test]
    fn test_fetch_ohlcv() {
        let ex = exchange_with_data();
        let rows = ex.fetch_ohlcv("BTC/USDT", "1m", None, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], 1_704_067_200_000.0);
        assert_eq!(rows[1][4], 110.0);
    }

    #[test]
    fn test_fetch_ohlcv_since_limit() {
        let ex = exchange_with_data();
        let rows = ex.fetch_ohlcv("BTC/USDT", "1m", Some(1_704_067_260_000), None);
        assert_eq!(rows.len(), 1);
        let rows = ex.fetch_ohlcv("BTC/USDT", "1m", None, Some(1));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][4], 110.0);
    }

    #[test]
    fn test_fetch_ticker() {
        let ex = exchange_with_data();
        let ticker = ex.fetch_ticker("BTC/USDT");
        assert_eq!(ticker["symbol"], "BTC/USDT");
        assert_eq!(ticker["bid"], 100.0);
        assert_eq!(ticker["ask"], 101.0);
        assert!(ticker["last"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn test_fetch_order_book() {
        let ex = exchange_with_data();
        let book = ex.fetch_order_book("BTC/USDT", None);
        assert_eq!(book["symbol"], "BTC/USDT");
        assert_eq!(book["bids"][0][0], 100.0);
        assert_eq!(book["asks"][0][0], 101.0);
    }

    #[test]
    fn test_fetch_balance() {
        let ex = exchange_with_data();
        let balance = ex.fetch_balance();
        assert_eq!(balance["total"]["USDT"], 100_000.0);
        assert_eq!(balance["total"]["BTC"], 10.0);
        assert_eq!(balance["assets"]["USDT"]["free"], 100_000.0);
    }

    #[test]
    fn test_create_market_order() {
        let mut ex = exchange_with_data();
        let order = ex
            .create_order("BTC/USDT", "market", "buy", 1.0, None, CreateOrderParams::new())
            .unwrap();
        assert_eq!(order["status"], "closed");
        assert_eq!(order["side"], "buy");
        assert_eq!(order["filled"], 1.0);
    }

    #[test]
    fn test_create_limit_order_rests() {
        let mut ex = exchange_with_data();
        let order = ex
            .create_order("BTC/USDT", "limit", "buy", 1.0, Some(50.0), CreateOrderParams::new())
            .unwrap();
        assert_eq!(order["status"], "open");
        let open = ex.fetch_open_orders(Some("BTC/USDT"));
        assert_eq!(open.len(), 1);
    }

    #[test]
    fn test_cancel_order() {
        let mut ex = exchange_with_data();
        let order = ex
            .create_order("BTC/USDT", "limit", "buy", 1.0, Some(50.0), CreateOrderParams::new())
            .unwrap();
        let id = order["id"].as_str().unwrap().to_string();
        let cancelled = ex.cancel_order(&id, "BTC/USDT").unwrap();
        assert_eq!(cancelled["status"], "canceled");
    }

    #[test]
    fn test_fetch_positions_empty_for_spot() {
        let ex = exchange_with_data();
        assert!(ex.fetch_positions(None).is_empty());
    }

    #[test]
    fn test_clock_control() {
        let mut ex = Exchange::default();
        let start = ts(1_000);
        ex.reset(start);
        ex.resume();
        ex.step();
        assert_eq!(ex.now(), start + chrono::Duration::milliseconds(1000));
        ex.pause();
        assert_eq!(ex.now(), start + chrono::Duration::milliseconds(1000));
    }

    #[test]
    fn test_set_step_ms() {
        let mut ex = Exchange::default();
        ex.set_step_ms(60000);
        ex.reset(ts(1_000));
        ex.resume();
        ex.step();
        assert_eq!(ex.now(), ts(61_000));
    }

    #[test]
    fn test_fetch_ohlcv_live_is_stepwise() {
        let mut ex = Exchange::default();
        ex.add_market(&Symbol::parse("BTC/USDT").unwrap(), MarketKind::Spot);
        ex.load_candles(
            "BTC/USDT",
            "1s",
            vec![candle(1_000, 100.0), candle(2_000, 101.0), candle(3_000, 102.0)],
        );
        ex.set_step_ms(1000);
        ex.reset(ts(1_000));
        ex.resume();

        // At t=1000 the first candle is visible only.
        assert_eq!(ex.fetch_ohlcv_live("BTC/USDT", "1s").len(), 1);
        assert!(!ex.replay_done("BTC/USDT", "1s"));

        ex.step(); // now = 2000
        assert_eq!(ex.fetch_ohlcv_live("BTC/USDT", "1s").len(), 2);

        ex.step(); // now = 3000
        assert_eq!(ex.fetch_ohlcv_live("BTC/USDT", "1s").len(), 3);
        assert!(ex.replay_done("BTC/USDT", "1s"));
    }

    #[test]
    fn test_fetch_ohlcv_live_respects_pause() {
        let mut ex = Exchange::default();
        ex.add_market(&Symbol::parse("BTC/USDT").unwrap(), MarketKind::Spot);
        ex.load_candles(
            "BTC/USDT",
            "1s",
            vec![candle(1_000, 100.0), candle(2_000, 101.0), candle(3_000, 102.0)],
        );
        ex.set_step_ms(1000);
        ex.reset(ts(1_000));
        ex.resume();
        ex.step();
        ex.pause();
        let frozen = ex.now();
        ex.step();
        ex.step();
        assert_eq!(ex.now(), frozen);
        assert_eq!(ex.fetch_ohlcv_live("BTC/USDT", "1s").len(), 2);
    }

    #[test]
    fn test_fetch_funding_rate_default_zero() {
        let ex = Exchange::default();
        let rate = ex.fetch_funding_rate("BTC/USDT");
        assert_eq!(rate["fundingRate"], 0.0);
    }

    #[test]
    fn test_create_order_unknown_symbol() {
        let mut ex = exchange_with_data();
        let result = ex.create_order("DOGE/USDT", "market", "buy", 1.0, None, CreateOrderParams::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_create_order_invalid_side() {
        let mut ex = exchange_with_data();
        let result = ex.create_order("BTC/USDT", "market", "up", 1.0, None, CreateOrderParams::new());
        assert!(result.is_err());
    }
}
