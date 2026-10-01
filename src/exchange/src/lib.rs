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
    /// Optional take-profit price.
    pub tp_price: Option<f64>,
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
    /// Closed candles already fed into the broker (symbol, timeframe) at some ts.
    replayed: HashMap<(String, String), Option<DateTime<Utc>>>,
    /// Spot cost basis per symbol: (qty held, average entry price).
    spot_cost: HashMap<String, (f64, f64)>,
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
            replayed: HashMap::new(),
            spot_cost: HashMap::new(),
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

    /// Advance the clock by one step and feed any bars that became due.
    pub fn step(&mut self) -> bool {
        self.clock.step();
        self.feed_due(self.clock.now());
        true
    }

    /// Advance the clock by `n` steps, feeding due bars along the way.
    /// Returns the number of steps actually taken (0 if the clock is paused).
    pub fn step_n(&mut self, n: u64) -> u64 {
        let mut taken = 0;
        for _ in 0..n {
            if !self.clock.step() {
                break;
            }
            taken += 1;
        }
        self.feed_due(self.clock.now());
        taken
    }

    /// Seek the clock to a timestamp.
    pub fn seek(&mut self, timestamp: DateTime<Utc>) -> bool {
        self.clock.seek(timestamp);
        self.feed_due(self.clock.now());
        true
    }

    /// Run the clock to its end, feeding due bars along the way.
    pub fn run(&mut self) -> bool {
        // Advance straight to the newest loaded candle so every bar gets fed
        // into the broker (the exchange is the source of truth for the replay).
        let end = self
            .candles
            .values()
            .flatten()
            .map(|c| c.timestamp)
            .max();
        if let Some(end) = end {
            self.clock.seek(end);
        }
        self.clock.resume();
        self.feed_due(self.clock.now());
        true
    }

    /// Pause the clock.
    pub fn pause(&mut self) {
        self.clock.pause()
    }

    /// Resume the clock.
    pub fn resume(&mut self) {
        self.clock.resume()
    }

    /// Whether the simulation clock is currently paused.
    pub fn is_paused(&self) -> bool {
        self.clock.is_paused()
    }

    /// Reset the clock (and the replay feed cursor).
    pub fn reset(&mut self, timestamp: DateTime<Utc>) {
        self.clock.reset(timestamp);
        self.replayed.clear();
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
    // Simulation engine: driving market events into the broker
    // ------------------------------------------------------------------

    /// The last known price for a symbol (from the most recent bar / live feed).
    pub fn last_price_for(&self, symbol: &str) -> Option<f64> {
        self.last_price.get(symbol).copied()
    }

    /// Feed one closed candle into the broker: mark-to-market positions,
    /// trigger stop/take-profit orders, sweep crossed resting limit orders,
    /// and record the reference price for immediate market-order fills.
    pub fn apply_price(&mut self, symbol: &Symbol, price: f64, timestamp: DateTime<Utc>) {
        self.last_price.insert(symbol.symbol.clone(), price);
        // Seed synthetic one-level liquidity so market and triggered orders
        // always have a reference price during a bar replay (no external book).
        self.broker.update_midpoint(&symbol.symbol, price, 1e12);
        // mark-to-market + stop / take-profit triggers + resting limit sweeps
        let executed = self.broker.on_price_update(symbol, price, timestamp);
        let is_spot = self.broker.market_kind(&symbol.symbol) == Some(MarketKind::Spot);
        // Track spot cost basis for orders triggered inside the broker (e.g. a
        // stop that filled on this bar), so spot positions show up in snapshots.
        for order in executed {
            if is_spot && order.filled > 0.0 {
                self.track_spot_cost(symbol, order.side, order.filled, order.average_price);
            }
        }
        // Resting limit orders crossed by this bar fill as maker. Without this
        // sweep they would hang open forever: nothing else re-matches them.
        let swept = self.broker.sweep_resting(symbol, price, timestamp);
        for (order, new_filled) in swept {
            if is_spot && new_filled > 0.0 {
                let avg = if order.filled > 0.0 { order.average_price } else { price };
                self.track_spot_cost(symbol, order.side, new_filled, avg);
            }
            self.publish_order_events(&order);
        }
    }

    /// Replay a candle as a deterministic OHLC path so intrabar triggers work.
    /// For bullish bars the low is visited first; bearish bars visit the high first.
    pub fn apply_candle(&mut self, symbol: &Symbol, candle: &OHLCV) {
        let path = if candle.close >= candle.open {
            [candle.open, candle.low, candle.high, candle.close]
        } else {
            [candle.open, candle.high, candle.low, candle.close]
        };
        for price in path {
            self.apply_price(symbol, price, candle.timestamp);
        }
    }

    /// Whether a candle for (symbol, timeframe) has not yet been fed into the
    /// broker. Used to avoid re-processing bars on repeated step() calls.
    fn needs_feed(&self, symbol: &str, timeframe: &str, ts: DateTime<Utc>) -> bool {
        let key = (symbol.to_string(), timeframe.to_string());
        let last = self.replayed.get(&key);
        last.map(|l| l.map(|replay_ts| replay_ts < ts).unwrap_or(true)).unwrap_or(true)
    }

    /// Process every candle at or before `now` that hasn't been fed yet, for
    /// all symbols. Returns the number of bars processed.
    pub fn feed_due(&mut self, now: DateTime<Utc>) -> usize {
        let mut fed = 0;
        for ((symbol, timeframe), candles) in self.candles.clone() {
            for c in candles {
                if c.timestamp > now {
                    break;
                }
                let marked = self.needs_feed(&symbol, &timeframe, c.timestamp);
                if !marked {
                    continue;
                }
                let sym = Symbol::parse(&symbol).unwrap();
                self.apply_candle(&sym, &c);
                self.replayed.insert((symbol.clone(), timeframe.clone()), Some(c.timestamp));
                fed += 1;
            }
        }
        fed
    }

    /// Whether the order book has meet-side liquidity for a market order.
    fn market_has_liquidity(&self, symbol: &Symbol, side: Side) -> bool {
        self.broker
            .engine(&symbol.symbol)
            .map(|e| match side {
                Side::Buy => e.book().best_ask().is_some(),
                Side::Sell => e.book().best_bid().is_some(),
            })
            .unwrap_or(false)
    }

    /// Update the spot cost basis used for UI spot positions (BUY/SELL of the
    /// base asset against the quote). A reduction on SELL realizes PnL.
    fn track_spot_cost(&mut self, symbol: &Symbol, side: Side, amount: f64, price: f64) {
        if amount <= 0.0 {
            return;
        }
        let (qty, avg) = self.spot_cost.get(&symbol.symbol).copied().unwrap_or((0.0, 0.0));
        match side {
            Side::Buy => {
                let new_qty = qty + amount;
                let new_avg = if new_qty > 0.0 { (qty * avg + amount * price) / new_qty } else { price };
                self.spot_cost.insert(symbol.symbol.clone(), (new_qty, new_avg));
            }
            Side::Sell => {
                let new_qty = (qty - amount).max(0.0);
                self.spot_cost.insert(symbol.symbol.clone(), (new_qty, avg));
            }
        }
    }

    /// The spot cost basis (qty, average entry price) for a symbol.
    pub fn spot_basis(&self, symbol: &str) -> (f64, f64) {
        self.spot_cost.get(symbol).copied().unwrap_or((0.0, 0.0))
    }

    // ------------------------------------------------------------------
    // UI snapshots (compact, one-call account / market state)
    // ------------------------------------------------------------------

    /// A compact account snapshot for the UI: balances, equity, PnL, positions,
    /// open orders. Returns a serde_json value (no heavy candle transfers).
    pub fn account_snapshot(&self) -> Value {
        // Balances.
        let mut balances = json!({});
        for (asset, b) in self.broker.wallet().balances() {
            balances[asset] = json!({ "free": b.available, "locked": b.locked, "total": b.total });
        }

        // Positions: linear (from portfolio) + spot cost basis.
        let mut positions = Vec::new();
        for (sym, pos) in self.broker.portfolio().positions().iter() {
            let mark = pos.mark_price;
            let pnl = if pos.side == simccxt_core::PositionSide::Long {
                (mark - pos.entry_price) * pos.qty
            } else {
                (pos.entry_price - mark) * pos.qty
            };
            positions.push(json!({
                "symbol": sym,
                "side": if pos.side == simccxt_core::PositionSide::Long { "long" } else { "short" },
                "qty": pos.qty,
                "entryPrice": pos.entry_price,
                "markPrice": mark,
                "unrealizedPnl": pnl + pos.realized_pnl,
                "realizedPnl": pos.realized_pnl,
                "leverage": self.broker.portfolio().leverage(sym),
                "liquidationPrice": pos.liquidation_price,
            }));
        }
        for (sym, (qty, avg)) in self.spot_cost.clone() {
            if qty <= 0.0 {
                continue;
            }
            let mark = self.last_price_for(&sym).unwrap_or(avg);
            let unrealized = (mark - avg) * qty;
            positions.push(json!({
                "symbol": sym,
                "side": "long",
                "qty": qty,
                "entryPrice": avg,
                "markPrice": mark,
                "unrealizedPnl": unrealized,
                "realizedPnl": 0.0,
                "leverage": 1.0,
                "liquidationPrice": 0.0,
            }));
        }

        let orders = self.fetch_open_orders(None);

        let total_pnl = self.broker.portfolio().unrealized_pnl()
            + self.broker.portfolio().realized_pnl()
            + self.spot_unrealized();

        json!({
            "time": self.now_ms(),
            "equity": self.equity(),
            "balances": balances,
            "positions": positions,
            "open": orders,
            "pnl": json!({
                "realized": self.broker.portfolio().realized_pnl() + self.spot_realized(),
                "unrealized": total_pnl,
                "fees": self.broker.wallet().total_fees(),
            }),
        })
    }

    /// A compact market snapshot: last price / tick for a symbol.
    pub fn market_snapshot(&self, symbol: &str) -> Value {
        let last = self.last_price_for(symbol).unwrap_or(0.0);
        let ticker = self.fetch_ticker(symbol);
        json!({
            "symbol": symbol,
            "last": last,
            "timestamp": self.now_ms(),
            "ticker": ticker,
        })
    }

    /// The account's executed fills as CCXT-style trade dicts (newest first).
    /// This is the account's own execution history — not the incoming public
    /// trade feed. Optional symbol filter and limit.
    pub fn fetch_fills(&self, symbol: Option<&str>, limit: Option<usize>) -> Vec<Value> {
        let mut fills = self.broker.fills(symbol);
        let mut fill_indices = HashMap::<String, usize>::new();
        let ids: Vec<String> = fills.iter().map(|fill| {
            let index = fill_indices.entry(fill.order_id.clone()).or_default();
            let id = format!("{}_{}", fill.order_id, *index);
            *index += 1;
            id
        }).collect();
        fills.reverse(); // newest first, like a live exchange feed
        let mut ids = ids;
        ids.reverse();
        if let Some(limit) = limit {
            fills.truncate(limit);
            ids.truncate(limit);
        }
        fills
            .into_iter()
            .zip(ids)
            .map(|(f, id)| {
                json!({
                    "id": id,
                    "symbol": f.symbol.symbol,
                    "side": if f.side == Side::Buy { "buy" } else { "sell" },
                    "type": order_type_str(f.order_type),
                    "price": f.price,
                    "amount": f.amount,
                    "cost": f.price * f.amount,
                    "fee": f.fee,
                    "maker": f.maker,
                    "orderId": f.order_id,
                    "timestamp": f.timestamp.timestamp_millis(),
                    "datetime": f.timestamp.to_rfc3339(),
                })
            })
            .collect()
    }

    /// Total account equity: quote free + marked-to-market positions + spot value.
    pub fn equity(&self) -> f64 {
        let mut eq = 0.0;
        // Free quote (USDT) plus the mark value of held base assets.
        for (asset, b) in self.broker.wallet().balances() {
            if asset.eq("USDT") || asset.eq("USDC") || asset.eq("Quote") {
                eq += b.total;
            }
        }
        // Mark value of spot base holdings at last price, in quote.
        for (sym, (qty, _)) in self.spot_cost.clone() {
            let mark = self.last_price_for(&sym).unwrap_or(0.0);
            eq += qty * mark;
        }
        // Unrealized PnL of linear positions + realized PnL from closed positions.
        eq += self.broker.portfolio().unrealized_pnl();
        eq += self.broker.portfolio().realized_pnl();
        eq
    }

    /// Sum of unrealized spot PnL (from cost basis vs current mark).
    fn spot_unrealized(&self) -> f64 {
        let mut total = 0.0;
        for (sym, (qty, avg)) in self.spot_cost.clone() {
            if qty <= 0.0 {
                continue;
            }
            let mark = self.last_price_for(&sym).unwrap_or(avg);
            total += (mark - avg) * qty;
        }
        total
    }

    /// Sum of realized spot PnL (tracked at SELL).
    fn spot_realized(&self) -> f64 {
        // Realized is folded into the wallet's quote balance on spot sells;
        // the wallet already reflects it. Keep a stable 0 here to avoid double
        // counting — spot PnL is realised directly into the quote balance.
        0.0
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
        orders.extend(self.broker.conditional_orders(symbol).iter().map(conditional_order_json));
        orders
    }

    /// Conditional order ID responsible for a newly triggered execution fill.
    pub fn take_triggered_order_source(&mut self, order_id: &str) -> Option<String> {
        self.broker.take_triggered_order_source(order_id)
    }

    fn all_open_orders(&self) -> Vec<simccxt_order_engine::Order> {
        // Iterate all registered markets (positions may not exist yet for spot).
        let mut out = Vec::new();
        for symbol in self.broker.symbols() {
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

    /// Place the order through the broker, falling back to an immediate
    /// marked-to-market fill when a market order has no booked liquidity.
    fn place_order(
        &mut self,
        request: BrokerOrderRequest,
        symbol: &Symbol,
        side: Side,
        order_type: OrderType,
        amount: f64,
    ) -> Result<simccxt_order_engine::Order, BrokerError> {
        if order_type == OrderType::Market && !self.market_has_liquidity(symbol, side) {
            let sym_key = &symbol.symbol;
            let maybe_price = self.last_price_for(sym_key);
            if let Some(price) = maybe_price {
                if price <= 0.0 {
                    return Err(BrokerError::InvalidOrderType(format!(
                        "no reference price for {sym_key} yet (replay not started)"
                    )));
                }
                let ts = self.clock.now();
                return self.broker.execute_market_at(symbol, side, amount, price, ts);
            }
            return Err(BrokerError::InvalidOrderType(format!(
                "no reference price for {sym_key} yet (advance the clock / replay first)"
            )));
        }
        self.broker.submit_order(request, self.clock.now())
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
            tp_price: params.tp_price,
            time_in_force: params.time_in_force,
            reduce_only: params.reduce_only,
            post_only: params.post_only,
        };

        let order = self.place_order(request, &symbol, side, order_type, amount)?;

        // Track spot cost basis for every immediate fill (market AND
        // marketable limit orders) so the UI can show a spot position + PnL.
        // Later sweep fills are tracked separately with their own delta.
        if self.broker.market_kind(&symbol.symbol) == Some(MarketKind::Spot) && order.filled > 0.0 {
            let avg = if order.average_price > 0.0 { order.average_price } else { price.unwrap_or(0.0) };
            self.track_spot_cost(&symbol, side, order.filled, avg);
        }

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
        "triggerPrice": if order.order_type == OrderType::Stop
            || order.order_type == OrderType::StopLimit
            || order.order_type == OrderType::TakeProfit
            || order.order_type == OrderType::TakeProfitLimit
            || order.order_type == OrderType::TrailingStop
        { order.price } else { None },
        "amount": order.amount,
        "filled": order.filled,
        "remaining": order.remaining,
        "average": order.average_price,
        "status": order_status_str(order.status),
        "timestamp": order.timestamp.timestamp_millis(),
        "datetime": order.timestamp.to_rfc3339(),
        "fee": order.fee,
        "timeInForce": format!("{:?}", order.time_in_force),
        "reduceOnly": order.reduce_only,
        "postOnly": order.post_only,
    })
}

fn conditional_order_json(order: &simccxt_broker::ConditionalOrder) -> Value {
    json!({
        "id": format!("conditional_{}", order.id),
        "symbol": order.symbol.symbol,
        "type": order_type_str(order.order_type),
        "side": if order.side == Side::Buy { "buy" } else { "sell" },
        "price": order.price,
        "triggerPrice": order.trigger_price,
        "amount": order.amount,
        "remaining": order.amount,
        "filled": 0.0,
        "average": 0.0,
        "status": "open",
        "timeInForce": "GTC",
        "reduceOnly": order.reduce_only,
        "postOnly": false,
        "timestamp": order.timestamp.timestamp_millis(),
        "datetime": order.timestamp.to_rfc3339(),
        "fee": 0.0,
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
    fn fill_ids_remain_stable_as_new_trades_arrive() {
        let mut ex = exchange_with_data();
        ex.create_order("BTC/USDT", "market", "buy", 1.0, None, CreateOrderParams::new())
            .unwrap();
        let first_before = ex.fetch_fills(None, None);
        let original_id = first_before[0]["id"].clone();
        let order_id = first_before[0]["orderId"].clone();
        let timestamp = ex.now();
        ex.apply_price(&Symbol::parse("BTC/USDT").unwrap(), 100.0, timestamp);
        ex.create_order("BTC/USDT", "market", "buy", 0.5, None, CreateOrderParams::new())
            .unwrap();
        let first_after = ex.fetch_fills(None, None).into_iter()
            .find(|fill| fill["orderId"] == order_id)
            .expect("original fill remains in history");
        assert_eq!(first_after["id"], original_id);
    }

    #[test]
    fn conditional_stops_are_visible_and_cancelable_as_open_orders() {
        let mut ex = exchange_with_data();
        let stop = ex.create_order(
            "BTC/USDT",
            "stop",
            "sell",
            0.5,
            None,
            CreateOrderParams::new().stop_price(90.0).reduce_only(true),
        ).unwrap();
        assert_eq!(stop["type"], "stop");
        assert_eq!(stop["triggerPrice"], 90.0);
        let open = ex.fetch_open_orders(Some("BTC/USDT"));
        assert_eq!(open.len(), 1);
        assert_eq!(open[0]["reduceOnly"], true);
        let canceled = ex.cancel_order(stop["id"].as_str().unwrap(), "BTC/USDT").unwrap();
        assert_eq!(canceled["status"], "canceled");
        assert!(ex.fetch_open_orders(Some("BTC/USDT")).is_empty());
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
    fn test_run_feeds_all_candles_and_triggers_stop() {
        let mut ex = Exchange::default();
        ex.add_market(&Symbol::parse("BTC/USDT").unwrap(), MarketKind::Spot);
        ex.deposit("USDT", 100_000.0);
        // Price rises strongly: 100 -> ~110.
        ex.load_candles(
            "BTC/USDT",
            "1s",
            vec![candle(1_000, 100.0), candle(2_000, 102.0), candle(3_000, 105.0), candle(4_000, 110.0)],
        );
        ex.set_step_ms(1000);
        ex.reset(ts(1_000));
        ex.resume();
        ex.step(); // feed bar @1000
        // Market buy at ~100, then a take-profit SELL at 108 (price reaches 110).
        ex.create_order("BTC/USDT", "market", "buy", 1.0, None, CreateOrderParams::new()).unwrap();
        ex.create_order(
            "BTC/USDT",
            "take_profit",
            "sell",
            1.0,
            None,
            CreateOrderParams::new().stop_price(108.0),
        )
        .unwrap();
        ex.run();
        assert!(ex.replay_done("BTC/USDT", "1s"));
        // The buy + the triggered TP sell should both be in the fill log.
        let fills = ex.fetch_fills(None, None);
        assert_eq!(fills.len(), 2, "expected buy + triggered TP sell fills, got {fills:?}");
    }

    #[test]
    fn protective_stop_triggers_on_intrabar_low_when_close_recovers() {
        let symbol = Symbol::parse("BTC/USDT").unwrap();
        let mut ex = Exchange::default();
        ex.add_market(&symbol, MarketKind::Spot);
        ex.deposit("USDT", 100_000.0);
        ex.load_candles(
            "BTC/USDT",
            "1s",
            vec![
                OHLCV::new(ts(1_000), 100.0, 101.0, 99.0, 100.0, 10.0),
                OHLCV::new(ts(2_000), 100.0, 105.0, 85.0, 100.0, 10.0),
            ],
        );
        ex.set_step_ms(1_000);
        ex.reset(ts(0));
        ex.resume();
        ex.step();
        ex.create_order("BTC/USDT", "market", "buy", 1.0, None, CreateOrderParams::new()).unwrap();
        ex.create_order(
            "BTC/USDT",
            "stop",
            "sell",
            1.0,
            None,
            CreateOrderParams::new().stop_price(90.0).reduce_only(true),
        )
        .unwrap();

        ex.step();

        let fills = ex.fetch_fills(None, None);
        assert_eq!(fills.len(), 2, "the stop must fire on the candle low: {fills:?}");
        assert_eq!(fills[0]["side"], "sell");
        assert!(ex.account_snapshot()["positions"].as_array().unwrap().is_empty());
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

    #[test]
    fn test_resting_limit_fills_when_replay_trades_through() {
        let mut ex = Exchange::default();
        ex.add_market(&Symbol::parse("BTC/USDT").unwrap(), MarketKind::Spot);
        ex.deposit("USDT", 100_000.0);
        // Falling market: 100 -> 95 -> 90.
        ex.load_candles(
            "BTC/USDT",
            "1s",
            vec![
                OHLCV::new(ts(1_000), 100.0, 101.0, 99.0, 100.0, 10.0),
                OHLCV::new(ts(2_000), 100.0, 101.0, 94.0, 95.0, 10.0),
                OHLCV::new(ts(3_000), 95.0, 96.0, 90.0, 90.0, 10.0),
            ],
        );
        ex.set_step_ms(1000);
        ex.reset(ts(0)); // before the first bar: each step feeds exactly one bar
        ex.resume();
        ex.step(); // feed 100
        // Resting buy limit below the market: must NOT fill immediately.
        let order = ex
            .create_order("BTC/USDT", "limit", "buy", 1.0, Some(93.0), CreateOrderParams::new())
            .unwrap();
        assert_eq!(order["status"], "open");
        assert!(ex.fetch_fills(None, None).is_empty());
        ex.step(); // feed 95: still above the limit.
        assert!(ex.fetch_fills(None, None).is_empty());
        ex.step(); // feed 90: trades through 93 -> fills.
        let fills = ex.fetch_fills(None, None);
        assert_eq!(fills.len(), 1, "resting limit should fill, got {fills:?}");
        assert_eq!(fills[0]["side"], "buy");
        assert!(ex.fetch_open_orders(Some("BTC/USDT")).is_empty());
        // Spot position shows up (cost basis tracked for sweep fills too).
        let shot = ex.account_snapshot();
        let positions = shot["positions"].as_array().unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0]["qty"], 1.0);
    }

    #[test]
    fn test_marketable_limit_fills_immediately_with_position() {
        let mut ex = Exchange::default();
        ex.add_market(&Symbol::parse("BTC/USDT").unwrap(), MarketKind::Spot);
        ex.deposit("USDT", 100_000.0);
        ex.load_candles("BTC/USDT", "1s", vec![candle(1_000, 100.0)]);
        ex.set_step_ms(1000);
        ex.reset(ts(1_000));
        ex.resume();
        ex.step(); // feed 100, midpoint liquidity seeded
        // Buy limit ABOVE the market: fills at once, position appears.
        let order = ex
            .create_order("BTC/USDT", "limit", "buy", 1.0, Some(101.0), CreateOrderParams::new())
            .unwrap();
        assert_eq!(order["status"], "closed");
        let shot = ex.account_snapshot();
        assert_eq!(shot["positions"].as_array().unwrap().len(), 1);
    }
}
