//! Replay engine for simccxt.
//!
//! Replays historical market data in strict chronological order. The engine
//! owns a time-sorted timeline of market events and a [`Clock`], and emits
//! events onto an [`EventBus`] as the clock advances.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use simccxt_clock::Clock;
use simccxt_core::{OHLCV, Symbol, Trade};
use simccxt_event_bus::{Event, EventBus};
use simccxt_market::{FundingRate, OpenInterest, OrderBook};

/// A single replayable market event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReplayEvent {
    /// A new OHLCV candle closed.
    Candle { symbol: Symbol, candle: OHLCV },
    /// A public trade occurred.
    Trade { symbol: Symbol, trade: Trade },
    /// A funding rate was recorded.
    Funding { symbol: Symbol, funding: FundingRate },
    /// Open interest was recorded.
    OpenInterest { symbol: Symbol, open_interest: OpenInterest },
    /// An order book snapshot was taken.
    OrderBookSnapshot { symbol: Symbol, snapshot: simccxt_core::OrderBookSnapshot },
}

impl ReplayEvent {
    /// The timestamp of this event.
    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            ReplayEvent::Candle { candle, .. } => candle.timestamp,
            ReplayEvent::Trade { trade, .. } => trade.timestamp,
            ReplayEvent::Funding { funding, .. } => funding.timestamp,
            ReplayEvent::OpenInterest { open_interest, .. } => open_interest.timestamp,
            ReplayEvent::OrderBookSnapshot { snapshot, .. } => snapshot.timestamp,
        }
    }

    /// The symbol this event belongs to.
    pub fn symbol(&self) -> &Symbol {
        match self {
            ReplayEvent::Candle { symbol, .. }
            | ReplayEvent::Trade { symbol, .. }
            | ReplayEvent::Funding { symbol, .. }
            | ReplayEvent::OpenInterest { symbol, .. }
            | ReplayEvent::OrderBookSnapshot { symbol, .. } => symbol,
        }
    }

    /// Convert into the corresponding bus [`Event`].
    fn into_bus_event(self) -> Event {
        match self {
            ReplayEvent::Candle { symbol, candle } => Event::NewCandle { symbol, candle },
            ReplayEvent::Trade { symbol, trade } => Event::Trade { symbol, trade },
            ReplayEvent::Funding { symbol, funding } => {
                Event::FundingApplied { symbol, funding_rate: funding.funding_rate, timestamp: funding.timestamp }
            }
            ReplayEvent::OpenInterest { symbol, open_interest } => Event::IndicatorUpdated {
                name: format!("{symbol}:open_interest"),
                value: open_interest.open_interest,
                timestamp: open_interest.timestamp,
            },
            ReplayEvent::OrderBookSnapshot { symbol, snapshot } => {
                let book = OrderBook::from_snapshot(&snapshot);
                Event::IndicatorUpdated {
                    name: format!("{symbol}:mid_price"),
                    value: book.mid_price().unwrap_or(0.0),
                    timestamp: snapshot.timestamp,
                }
            }
        }
    }
}

/// A replay engine that drives a [`Clock`] through a timeline of market events.
pub struct ReplayEngine {
    timeline: Vec<ReplayEvent>,
    clock: Clock,
    bus: EventBus,
    cursor: usize,
    events_emitted: usize,
}

impl ReplayEngine {
    /// Create a replay engine from a timeline, clock, and event bus.
    ///
    /// The timeline is sorted by timestamp ascending; sorting is enforced here
    /// so callers may supply data in any order.
    pub fn new(timeline: Vec<ReplayEvent>, clock: Clock, bus: EventBus) -> Self {
        let mut timeline = timeline;
        timeline.sort_by_key(|e| e.timestamp());
        Self {
            timeline,
            clock,
            bus,
            cursor: 0,
            events_emitted: 0,
        }
    }

    /// The current clock time.
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    /// The underlying clock (immutable access).
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// Mutable access to the clock (for pause/resume/seek/step_ms).
    pub fn clock_mut(&mut self) -> &mut Clock {
        &mut self.clock
    }

    /// The number of events emitted so far.
    pub fn events_emitted(&self) -> usize {
        self.events_emitted
    }

    /// The total number of events in the timeline.
    pub fn total_events(&self) -> usize {
        self.timeline.len()
    }

    /// Whether the timeline has been fully replayed.
    pub fn is_done(&self) -> bool {
        self.cursor >= self.timeline.len()
    }

    /// The timestamp of the next unplayed event, if any.
    pub fn next_event_time(&self) -> Option<DateTime<Utc>> {
        self.timeline.get(self.cursor).map(|e| e.timestamp())
    }

    /// Emit all events whose timestamp is at or before the clock's current time.
    ///
    /// Returns the number of events emitted.
    pub fn emit_ready(&mut self) -> usize {
        let now = self.clock.now();
        let mut emitted = 0;
        while let Some(event) = self.timeline.get(self.cursor) {
            if event.timestamp() > now {
                break;
            }
            let event = self.timeline[self.cursor].clone();
            self.cursor += 1;
            self.bus.publish(&event.clone().into_bus_event());
            self.events_emitted += 1;
            emitted += 1;
        }
        emitted
    }

    /// Advance the clock by one step and emit any events that became ready.
    pub fn step(&mut self) -> usize {
        self.clock.step();
        self.emit_ready()
    }

    /// Advance the clock by `n` steps, emitting ready events as we go.
    pub fn step_n(&mut self, n: u64) -> usize {
        let mut total = 0;
        for _ in 0..n {
            if self.clock.is_paused() {
                break;
            }
            self.clock.step();
            total += self.emit_ready();
        }
        total
    }

    /// Seek the clock to a timestamp and emit any events up to that point.
    pub fn seek(&mut self, timestamp: DateTime<Utc>) -> usize {
        self.clock.seek(timestamp);
        self.emit_ready()
    }

    /// Run to completion: repeatedly step until the timeline is exhausted.
    ///
    /// Returns the total number of events emitted.
    pub fn run(&mut self) -> usize {
        self.clock.run();
        let mut total = 0;
        while !self.is_done() {
            total += self.step();
            if self.clock.is_paused() {
                // Reached the clock's end time before the timeline ended.
                break;
            }
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simccxt_core::Side;

    fn ts(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).unwrap()
    }

    fn btc() -> Symbol {
        Symbol::parse("BTC/USDT").unwrap()
    }

    fn candle(ms: i64, close: f64) -> ReplayEvent {
        ReplayEvent::Candle {
            symbol: btc(),
            candle: OHLCV::new(ts(ms), close - 1.0, close + 1.0, close - 2.0, close, 10.0),
        }
    }

    fn trade(ms: i64, price: f64) -> ReplayEvent {
        ReplayEvent::Trade {
            symbol: btc(),
            trade: Trade::new(ts(ms), price, 1.0, Side::Buy, "o1".to_string()),
        }
    }

    fn engine(timeline: Vec<ReplayEvent>, start_ms: i64, step_ms: u64) -> ReplayEngine {
        let clock = Clock::simulated(ts(start_ms), step_ms);
        ReplayEngine::new(timeline, clock, EventBus::new())
    }

    #[test]
    fn test_timeline_is_sorted_on_construction() {
        let mut eng = engine(
            vec![candle(3_000, 3.0), candle(1_000, 1.0), candle(2_000, 2.0)],
            0,
            1000,
        );
        eng.clock_mut().resume();
        eng.run();
        assert_eq!(eng.total_events(), 3);
        assert_eq!(eng.events_emitted(), 3);
    }

    #[test]
    fn test_chronological_order_preserved() {
        let bus = EventBus::new();
        let received: std::rc::Rc<std::cell::RefCell<Vec<f64>>> = Default::default();
        let r = received.clone();
        let _sub = bus.subscribe(move |e| {
            if let Event::Trade { trade, .. } = e {
                r.borrow_mut().push(trade.price);
            }
        });
        let clock = Clock::simulated(ts(0), 1000);
        let mut eng = ReplayEngine::new(
            vec![trade(3_000, 3.0), trade(1_000, 1.0), trade(2_000, 2.0)],
            clock,
            bus,
        );
        eng.clock_mut().resume();
        eng.run();
        assert_eq!(*received.borrow(), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_step_emits_ready_events() {
        let mut eng = engine(vec![candle(500, 1.0), candle(1_500, 2.0), candle(3_500, 3.0)], 0, 1000);
        eng.clock_mut().resume();
        // After first step (now = 1000), the candle at 500 is ready.
        assert_eq!(eng.step(), 1);
        assert_eq!(eng.now(), ts(1000));
        // After second step (now = 2000), the candle at 1500 is ready.
        assert_eq!(eng.step(), 1);
        // After third step (now = 3000), nothing new.
        assert_eq!(eng.step(), 0);
        // After fourth step (now = 4000), the candle at 3500 is ready.
        assert_eq!(eng.step(), 1);
        assert!(eng.is_done());
    }

    #[test]
    fn test_run_replays_everything() {
        let mut eng = engine(vec![candle(500, 1.0), trade(900, 10.0), candle(1_500, 2.0)], 0, 1000);
        let emitted = eng.run();
        assert_eq!(emitted, 3);
        assert!(eng.is_done());
    }

    #[test]
    fn test_seek_jumps_forward() {
        let mut eng = engine(vec![candle(500, 1.0), trade(900, 10.0), candle(1_500, 2.0)], 0, 1000);
        let emitted = eng.seek(ts(2_000));
        assert_eq!(emitted, 3);
        assert!(eng.is_done());
    }

    #[test]
    fn test_events_emitted_tracking() {
        let mut eng = engine(vec![candle(500, 1.0)], 0, 1000);
        assert_eq!(eng.events_emitted(), 0);
        eng.clock_mut().resume();
        eng.step();
        assert_eq!(eng.events_emitted(), 1);
    }

    #[test]
    fn test_next_event_time() {
        let mut eng = engine(vec![candle(500, 1.0), candle(1_500, 2.0)], 0, 1000);
        assert_eq!(eng.next_event_time(), Some(ts(500)));
        eng.clock_mut().resume();
        eng.step();
        assert_eq!(eng.next_event_time(), Some(ts(1_500)));
    }

    #[test]
    fn test_empty_timeline_is_done() {
        let mut eng = engine(vec![], 0, 1000);
        assert!(eng.is_done());
        eng.clock_mut().resume();
        assert_eq!(eng.run(), 0);
    }

    #[test]
    fn test_emit_ready_does_not_advance_clock() {
        let mut eng = engine(vec![candle(500, 1.0)], 0, 1000);
        // Seek the clock but don't step: emit_ready only reads current time.
        eng.clock_mut().seek(ts(1000));
        let before = eng.now();
        let emitted = eng.emit_ready();
        assert_eq!(emitted, 1);
        assert_eq!(eng.now(), before);
    }
}
