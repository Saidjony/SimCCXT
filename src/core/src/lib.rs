use serde::{Deserialize, Serialize};
use std::fmt;
use chrono::{DateTime, Utc};

/// Global configuration for the simulator
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Default seed for random operations
    pub default_seed: u64,
    /// Default leverage multiplier
    pub default_leverage: f64,
    /// Maximum order size before rejection
    pub max_order_size: f64,
    /// Minimum order size
    pub min_order_size: f64,
    /// Fee tier configuration
    pub fee_tiers: FeeTiers,
    /// Time configuration
    pub time_config: TimeConfig,
}

/// Fee tiers configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeTiers {
    /// Maker fee rate
    pub maker: f64,
    /// Taker fee rate
    pub taker: f64,
    /// Funding fee rate
    pub funding: f64,
}

/// Time configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeConfig {
    /// Initial simulation time
    pub initial_time: DateTime<Utc>,
    /// Time step size in milliseconds
    pub step_ms: u64,
    /// Whether time is paused
    pub paused: bool,
}

impl Default for Config {
    fn default() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let initial_time = Utc::now();
        #[cfg(target_arch = "wasm32")]
        let initial_time =
            DateTime::<Utc>::from_timestamp_millis(0).expect("the Unix epoch is a valid timestamp");
        Self {
            default_seed: 42,
            default_leverage: 1.0,
            max_order_size: 1_000_000.0,
            min_order_size: 0.001,
            fee_tiers: FeeTiers {
                // Fee-free simulation: the trader's PnL is exactly the price
                // move, and risk sizing has no commission term to add.
                maker: 0.0,
                taker: 0.0,
                funding: 0.0001,
            },
            time_config: TimeConfig {
                initial_time,
                step_ms: 1000,
                paused: false,
            },
        }
    }
}

/// Clock time type alias
pub type SimTime = DateTime<Utc>;

/// Errors that can occur during simulation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SimError {
    /// Order validation failed
    OrderValidation(String),
    /// Insufficient balance
    InsufficientBalance { available: f64, required: f64 },
    /// Market not found
    MarketNotFound(String),
    /// Order not found
    OrderNotFound(String),
    /// Invalid state transition
    InvalidStateTransition(String),
    /// Configuration error
    ConfigError(String),
}

impl fmt::Display for SimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SimError::OrderValidation(msg) => write!(f, "OrderValidation: {}", msg),
            SimError::InsufficientBalance { available, required } => {
                write!(f, "InsufficientBalance: available={}, required={}", available, required)
            }
            SimError::MarketNotFound(msg) => write!(f, "MarketNotFound: {}", msg),
            SimError::OrderNotFound(msg) => write!(f, "OrderNotFound: {}", msg),
            SimError::InvalidStateTransition(msg) => write!(f, "InvalidStateTransition: {}", msg),
            SimError::ConfigError(msg) => write!(f, "ConfigError: {}", msg),
        }
    }
}

impl std::error::Error for SimError {}

/// Market symbol representation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Symbol {
    /// Base asset (e.g., "BTC", "ETH")
    pub base: String,
    /// Quote asset (e.g., "USD", "USDT")
    pub quote: String,
    /// Full symbol string (e.g., "BTC/USD")
    pub symbol: String,
}

impl Symbol {
    /// Parse a symbol string like "BTC/USD"
    pub fn parse(s: &str) -> Result<Self, SimError> {
        let parts: Vec<&str> = s.split('/').collect();
        if parts.len() != 2 {
            return Err(SimError::MarketNotFound(s.to_string()));
        }
        Ok(Self {
            base: parts[0].to_string(),
            quote: parts[1].to_string(),
            symbol: s.to_string(),
        })
    }

    /// Format as "BASE/QUOTE"
    pub fn format(&self) -> String {
        format!("{}/{}", self.base, self.quote)
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.symbol)
    }
}

/// Side of an order
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

/// Order type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderType {
    Market,
    Limit,
    Stop,
    StopLimit,
    TakeProfit,
    TakeProfitLimit,
    TrailingStop,
    PostOnly,
    IOC,  // Immediate or Cancel
    FOK,  // Fill or Kill
}

/// Time in force
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TimeInForce {
    #[default]
    GTC,  // Good Til Cancelled
    IOC,
    FOK,
    GTX,  // Good Till Crossing
    DAY,
    GTD,  // Good Till Date
}

/// Order status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    New,
    PartialFilled,
    Filled,
    Canceled,
    Expired,
    Rejected,
}

/// Order side for display
impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Side::Buy => write!(f, "BUY"),
            Side::Sell => write!(f, "SELL"),
        }
    }
}

/// Order type for display
impl fmt::Display for OrderType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OrderType::Market => write!(f, "MARKET"),
            OrderType::Limit => write!(f, "LIMIT"),
            OrderType::Stop => write!(f, "STOP"),
            OrderType::StopLimit => write!(f, "STOP_LIMIT"),
            OrderType::TakeProfit => write!(f, "TAKE_PROFIT"),
            OrderType::TakeProfitLimit => write!(f, "TAKE_PROFIT_LIMIT"),
            OrderType::TrailingStop => write!(f, "TRAILING_STOP"),
            OrderType::PostOnly => write!(f, "POST_ONLY"),
            OrderType::IOC => write!(f, "IOC"),
            OrderType::FOK => write!(f, "FOK"),
        }
    }
}

/// Time in force for display
impl fmt::Display for TimeInForce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimeInForce::GTC => write!(f, "GTC"),
            TimeInForce::IOC => write!(f, "IOC"),
            TimeInForce::FOK => write!(f, "FOK"),
            TimeInForce::GTX => write!(f, "GTX"),
            TimeInForce::DAY => write!(f, "DAY"),
            TimeInForce::GTD => write!(f, "GTD"),
        }
    }
}

/// Order status for display
impl fmt::Display for OrderStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OrderStatus::New => write!(f, "NEW"),
            OrderStatus::PartialFilled => write!(f, "PARTIAL_FILLED"),
            OrderStatus::Filled => write!(f, "FILLED"),
            OrderStatus::Canceled => write!(f, "CANCELED"),
            OrderStatus::Expired => write!(f, "EXPIRED"),
            OrderStatus::Rejected => write!(f, "REJECTED"),
        }
    }
}

/// Order side side enum with display
pub trait SideDisplay {
    fn side_str(&self) -> &str;
}

/// Order side trait implementation
impl SideDisplay for Side {
    fn side_str(&self) -> &str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

/// Order price and quantity
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderPrice {
    /// Order price in quote asset
    pub price: f64,
    /// Original order quantity in base asset
    pub qty: f64,
    /// Remaining quantity to fill
    pub remaining: f64,
    /// Filled quantity
    pub filled: f64,
}

impl OrderPrice {
    /// Create new order price
    pub fn new(price: f64, qty: f64) -> Self {
        Self {
            price,
            qty,
            remaining: qty,
            filled: 0.0,
        }
    }

    /// Check if order is fully filled
    pub fn is_fully_filled(&self) -> bool {
        self.remaining <= 0.0
    }

    /// Check if order is still active
    pub fn is_active(&self) -> bool {
        self.remaining > 0.0
    }
}

/// Trade execution record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    /// Trade timestamp
    pub timestamp: SimTime,
    /// Trade price
    pub price: f64,
    /// Trade quantity
    pub qty: f64,
    /// Trade side
    pub side: Side,
    /// Order ID that this trade belongs to
    pub order_id: String,
    /// Trade ID
    pub trade_id: String,
}

impl Trade {
    /// Create a new trade
    pub fn new(timestamp: SimTime, price: f64, qty: f64, side: Side, order_id: String) -> Self {
        let trade_id = format!("{}_{}", order_id, timestamp.timestamp());
        Self {
            timestamp,
            price,
            qty,
            side,
            order_id,
            trade_id,
        }
    }
}

/// Order book snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookSnapshot {
    /// Symbol
    pub symbol: Symbol,
    /// Bids (price, quantity)
    pub bids: Vec<(f64, f64)>,
    /// Asks (price, quantity)
    pub asks: Vec<(f64, f64)>,
    /// Timestamp
    pub timestamp: SimTime,
}

impl OrderBookSnapshot {
    /// Create new order book snapshot
    pub fn new(symbol: Symbol, bids: Vec<(f64, f64)>, asks: Vec<(f64, f64)>, timestamp: SimTime) -> Self {
        Self { symbol, bids, asks, timestamp }
    }
}

/// OHLCV candle data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OHLCV {
    /// Open time
    pub timestamp: SimTime,
    /// Open price
    pub open: f64,
    /// High price
    pub high: f64,
    /// Low price
    pub low: f64,
    /// Close price
    pub close: f64,
    /// Volume in base asset
    pub volume: f64,
}

impl OHLCV {
    /// Create new OHLCV candle
    pub fn new(timestamp: SimTime, open: f64, high: f64, low: f64, close: f64, volume: f64) -> Self {
        Self { timestamp, open, high, low, close, volume }
    }
}

/// Position side
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PositionSide {
    Long,
    Short,
}

impl fmt::Display for PositionSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PositionSide::Long => write!(f, "LONG"),
            PositionSide::Short => write!(f, "SHORT"),
        }
    }
}

/// Position information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    /// Symbol
    pub symbol: Symbol,
    /// Position side
    pub side: PositionSide,
    /// Position quantity
    pub qty: f64,
    /// Entry price
    pub entry_price: f64,
    /// Mark price
    pub mark_price: f64,
    /// Unrealized PnL
    pub unrealized_pnl: f64,
    /// Realized PnL
    pub realized_pnl: f64,
    /// Liquidation price
    pub liquidation_price: f64,
    /// Funding fee
    pub funding: f64,
    /// When this position was opened (flat -> long/short). Kept across adds
    /// and partial closes, reset on a flip. Zero = unknown (not set yet).
    pub opened_at: DateTime<Utc>,
}

impl Position {
    /// Create new position
    pub fn new(symbol: Symbol, side: PositionSide, qty: f64, entry_price: f64, mark_price: f64) -> Self {
        let (unrealized_pnl, liquidation_price) = match side {
            PositionSide::Long => {
                let pnl = (mark_price - entry_price) * qty;
                let liq = entry_price * 0.8; // 20% move to liquidation
                (pnl, liq)
            }
            PositionSide::Short => {
                let pnl = (entry_price - mark_price) * qty;
                let liq = entry_price * 1.2; // 20% move to liquidation
                (pnl, liq)
            }
        };
        
        Self {
            symbol,
            side,
            qty,
            entry_price,
            mark_price,
            unrealized_pnl,
            realized_pnl: 0.0,
            liquidation_price,
            funding: 0.0,
            opened_at: DateTime::<Utc>::UNIX_EPOCH,
        }
    }

    /// Update position with new mark price
    pub fn update_mark_price(&mut self, new_mark: f64) {
        self.mark_price = new_mark;
        match self.side {
            PositionSide::Long => {
                self.unrealized_pnl = (new_mark - self.entry_price) * self.qty;
            }
            PositionSide::Short => {
                self.unrealized_pnl = (self.entry_price - new_mark) * self.qty;
            }
        }
    }

    /// Check if position is liquidatable
    pub fn is_liquidatable(&self) -> bool {
        match self.side {
            PositionSide::Long => self.mark_price <= self.liquidation_price,
            PositionSide::Short => self.mark_price >= self.liquidation_price,
        }
    }
}

/// Balance information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Balance {
    /// Asset name
    pub asset: String,
    /// Total balance
    pub total: f64,
    /// Available balance
    pub available: f64,
    /// Locked balance (in orders)
    pub locked: f64,
}

impl Balance {
    /// Create new balance
    pub fn new(asset: &str, total: f64, available: f64) -> Self {
        Self {
            asset: asset.to_string(),
            total,
            available,
            locked: total - available,
        }
    }

    /// Check if sufficient available balance
    pub fn has_available(&self, amount: f64) -> bool {
        self.available >= amount
    }

    /// Check if sufficient total balance
    pub fn has_total(&self, amount: f64) -> bool {
        self.total >= amount
    }
}

/// Fee calculation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fee {
    /// Fee amount in quote asset
    pub fee: f64,
    /// Fee rate applied
    pub rate: f64,
    /// Whether maker fee
    pub maker: bool,
}

impl Fee {
    /// Calculate fee for order
    pub fn calculate(price: f64, qty: f64, rate: f64, maker: bool) -> Self {
        let fee = price * qty * rate;
        Self {
            fee,
            rate,
            maker,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_parse() {
        let s = Symbol::parse("BTC/USDT").unwrap();
        assert_eq!(s.base, "BTC");
        assert_eq!(s.quote, "USDT");
        assert_eq!(s.symbol, "BTC/USDT");
        assert_eq!(s.format(), "BTC/USDT");
    }

    #[test]
    fn test_symbol_parse_invalid() {
        assert!(Symbol::parse("BTC").is_err());
        assert!(Symbol::parse("BTC/USDT/EXTRA").is_err());
    }

    #[test]
    fn test_order_price() {
        let op = OrderPrice::new(100.0, 10.0);
        assert_eq!(op.price, 100.0);
        assert_eq!(op.qty, 10.0);
        assert_eq!(op.remaining, 10.0);
        assert_eq!(op.filled, 0.0);
        assert!(op.is_active());
        assert!(!op.is_fully_filled());
    }

    #[test]
    fn test_trade_creation() {
        let ts = Utc::now();
        let trade = Trade::new(ts, 100.0, 1.0, Side::Buy, "order1".to_string());
        assert_eq!(trade.price, 100.0);
        assert_eq!(trade.qty, 1.0);
        assert_eq!(trade.side, Side::Buy);
        assert_eq!(trade.order_id, "order1");
        assert!(!trade.trade_id.is_empty());
    }

    #[test]
    fn test_position_long_pnl() {
        let s = Symbol::parse("BTC/USDT").unwrap();
        let mut pos = Position::new(s, PositionSide::Long, 1.0, 100.0, 100.0);
        assert_eq!(pos.unrealized_pnl, 0.0);
        pos.update_mark_price(110.0);
        assert!((pos.unrealized_pnl - 10.0).abs() < 1e-9);
        assert!(!pos.is_liquidatable());
    }

    #[test]
    fn test_position_short_pnl() {
        let s = Symbol::parse("BTC/USDT").unwrap();
        let mut pos = Position::new(s, PositionSide::Short, 1.0, 100.0, 100.0);
        pos.update_mark_price(90.0);
        assert!((pos.unrealized_pnl - 10.0).abs() < 1e-9);
    }

    #[test]
    fn test_position_liquidation() {
        let s = Symbol::parse("BTC/USDT").unwrap();
        let mut pos = Position::new(s, PositionSide::Long, 1.0, 100.0, 100.0);
        pos.update_mark_price(75.0);
        assert!(pos.is_liquidatable());
    }

    #[test]
    fn test_balance() {
        let b = Balance::new("USDT", 1000.0, 800.0);
        assert_eq!(b.asset, "USDT");
        assert_eq!(b.total, 1000.0);
        assert_eq!(b.available, 800.0);
        assert_eq!(b.locked, 200.0);
        assert!(b.has_available(500.0));
        assert!(!b.has_available(900.0));
        assert!(b.has_total(900.0));
    }

    #[test]
    fn test_fee_calculation() {
        let fee = Fee::calculate(100.0, 1.0, 0.001, true);
        assert!((fee.fee - 0.1).abs() < 1e-9);
        assert_eq!(fee.rate, 0.001);
        assert!(fee.maker);
    }

    #[test]
    fn test_config_default() {
        let c = Config::default();
        assert_eq!(c.default_seed, 42);
        assert_eq!(c.default_leverage, 1.0);
        // The simulator trades without commissions.
        assert_eq!(c.fee_tiers.maker, 0.0);
        assert_eq!(c.fee_tiers.taker, 0.0);
    }

    #[test]
    fn test_ohlcv() {
        let ts = Utc::now();
        let c = OHLCV::new(ts, 1.0, 2.0, 0.5, 1.5, 100.0);
        assert_eq!(c.open, 1.0);
        assert_eq!(c.high, 2.0);
        assert_eq!(c.low, 0.5);
        assert_eq!(c.close, 1.5);
        assert_eq!(c.volume, 100.0);
    }

    #[test]
    fn test_order_book_snapshot() {
        let s = Symbol::parse("ETH/USDT").unwrap();
        let ts = Utc::now();
        let ob = OrderBookSnapshot::new(
            s,
            vec![(2000.0, 1.0), (1999.0, 2.0)],
            vec![(2001.0, 1.5), (2002.0, 2.5)],
            ts,
        );
        assert_eq!(ob.bids.len(), 2);
        assert_eq!(ob.asks.len(), 2);
    }

    #[test]
    fn test_side_display() {
        assert_eq!(Side::Buy.side_str(), "buy");
        assert_eq!(Side::Sell.side_str(), "sell");
        assert_eq!(format!("{}", Side::Buy), "BUY");
        assert_eq!(format!("{}", OrderType::Market), "MARKET");
        assert_eq!(format!("{}", OrderStatus::Filled), "FILLED");
    }
}