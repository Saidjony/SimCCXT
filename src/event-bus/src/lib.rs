//! Event bus for simccxt.
//!
//! All communication between simulator components flows through events.
//! The bus is synchronous and single-threaded to guarantee deterministic,
//! reproducible replays.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use simccxt_core::{Balance, OHLCV, Position, Symbol, Trade};

/// The set of event categories, used for filtering subscriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventType {
    /// A new candle was formed
    NewCandle,
    /// A public trade occurred
    Trade,
    /// An order was filled (fully or partially)
    OrderFilled,
    /// A position was updated
    PositionUpdated,
    /// A balance was updated
    BalanceUpdated,
    /// Funding was applied to a position
    FundingApplied,
    /// A position was liquidated
    Liquidation,
}

impl EventType {
    /// All event types.
    pub fn all() -> [EventType; 7] {
        [
            EventType::NewCandle,
            EventType::Trade,
            EventType::OrderFilled,
            EventType::PositionUpdated,
            EventType::BalanceUpdated,
            EventType::FundingApplied,
            EventType::Liquidation,
        ]
    }
}

/// A single event flowing through the bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// A new candle was formed.
    NewCandle { symbol: Symbol, candle: OHLCV },
    /// A public trade occurred.
    Trade { symbol: Symbol, trade: Trade },
    /// An order was filled.
    OrderFilled {
        symbol: Symbol,
        order_id: String,
        price: f64,
        filled_amount: f64,
        remaining: f64,
        timestamp: DateTime<Utc>,
    },
    /// A position was updated.
    PositionUpdated { position: Position },
    /// A balance was updated.
    BalanceUpdated { balance: Balance },
    /// Funding was applied.
    FundingApplied { symbol: Symbol, funding_rate: f64, timestamp: DateTime<Utc> },
    /// A position was liquidated.
    Liquidation { position: Position, timestamp: DateTime<Utc> },
}

impl Event {
    /// The category of this event.
    pub fn event_type(&self) -> EventType {
        match self {
            Event::NewCandle { .. } => EventType::NewCandle,
            Event::Trade { .. } => EventType::Trade,
            Event::OrderFilled { .. } => EventType::OrderFilled,
            Event::PositionUpdated { .. } => EventType::PositionUpdated,
            Event::BalanceUpdated { .. } => EventType::BalanceUpdated,
            Event::FundingApplied { .. } => EventType::FundingApplied,
            Event::Liquidation { .. } => EventType::Liquidation,
        }
    }
}

/// A subscriber callback that receives events.
type SubscriberCell = Rc<RefCell<dyn FnMut(&Event)>>;

/// A handle to an active subscription.
///
/// The subscription is active as long as the handle is alive; dropping it
/// unsubscribes from the bus. This prevents leaking stale callbacks.
#[derive(Clone)]
pub struct Subscription {
    id: usize,
    bus: Rc<RefCell<EventBusInner>>,
}

impl Subscription {
    /// The subscription id.
    pub fn id(&self) -> usize {
        self.id
    }

    /// Explicitly unsubscribe.
    pub fn unsubscribe(self) -> bool {
        self.bus.borrow_mut().subscribers.remove(&self.id).is_some()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.bus.borrow_mut().subscribers.remove(&self.id);
    }
}

/// Internal state of the event bus.
#[derive(Default)]
struct EventBusInner {
    subscribers: HashMap<usize, (Option<EventType>, SubscriberCell)>,
    next_id: usize,
}

/// The event bus.
///
/// Subscribers register callbacks that are invoked synchronously when an event
/// is published. The bus is single-threaded (via `Rc`/`RefCell`), which keeps
/// replay deterministic.
#[derive(Clone, Default)]
pub struct EventBus {
    inner: Rc<RefCell<EventBusInner>>,
}

impl EventBus {
    /// Create a new empty event bus.
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(EventBusInner {
                subscribers: HashMap::new(),
                next_id: 0,
            })),
        }
    }

    /// Subscribe to all events.
    pub fn subscribe<F>(&self, callback: F) -> Subscription
    where
        F: FnMut(&Event) + 'static,
    {
        self.subscribe_inner(None, callback)
    }

    /// Subscribe only to events of a specific type.
    pub fn subscribe_filtered<F>(&self, event_type: EventType, callback: F) -> Subscription
    where
        F: FnMut(&Event) + 'static,
    {
        self.subscribe_inner(Some(event_type), callback)
    }

    fn subscribe_inner<F>(&self, filter: Option<EventType>, callback: F) -> Subscription
    where
        F: FnMut(&Event) + 'static,
    {
        let mut inner = self.inner.borrow_mut();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.subscribers.insert(id, (filter, Rc::new(RefCell::new(callback))));
        Subscription {
            id,
            bus: Rc::clone(&self.inner),
        }
    }

    /// Publish an event to all matching subscribers.
    ///
    /// Subscribers are invoked in subscription order. A snapshot of subscriber
    /// cells is taken before iteration so that subscribers may safely
    /// subscribe or unsubscribe during dispatch.
    pub fn publish(&self, event: &Event) {
        let cells: Vec<(Option<EventType>, SubscriberCell)> = {
            let inner = self.inner.borrow();
            inner
                .subscribers
                .values()
                .cloned()
                .collect()
        };

        for (filter, cell) in cells {
            if let Some(ft) = filter {
                if ft != event.event_type() {
                    continue;
                }
            }
            (cell.borrow_mut())(event);
        }
    }

    /// The number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.inner.borrow().subscribers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simccxt_core::{Side, Trade};

    fn btc() -> Symbol {
        Symbol::parse("BTC/USDT").unwrap()
    }

    fn ts() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1_704_067_200_000).unwrap()
    }

    fn make_trade() -> Trade {
        Trade::new(ts(), 100.0, 1.0, Side::Buy, "order1".to_string())
    }

    #[test]
    fn test_subscribe_and_publish() {
        let bus = EventBus::new();
        let received = Rc::new(RefCell::new(0));
        let r = Rc::clone(&received);
        let _sub = bus.subscribe(move |_| *r.borrow_mut() += 1);

        let event = Event::Trade { symbol: btc(), trade: make_trade() };
        bus.publish(&event);
        bus.publish(&event);
        assert_eq!(*received.borrow(), 2);
    }

    #[test]
    fn test_filtered_subscription() {
        let bus = EventBus::new();
        let candle_count = Rc::new(RefCell::new(0));
        let c = Rc::clone(&candle_count);
        let _sub = bus.subscribe_filtered(EventType::NewCandle, move |_| *c.borrow_mut() += 1);

        bus.publish(&Event::Trade { symbol: btc(), trade: make_trade() });
        bus.publish(&Event::Trade { symbol: btc(), trade: make_trade() });
        assert_eq!(*candle_count.borrow(), 0);

        let candle = OHLCV::new(ts(), 1.0, 2.0, 0.5, 1.5, 10.0);
        bus.publish(&Event::NewCandle { symbol: btc(), candle });
        assert_eq!(*candle_count.borrow(), 1);
    }

    #[test]
    fn test_unsubscribe_stops_delivery() {
        let bus = EventBus::new();
        let received = Rc::new(RefCell::new(0));
        let r = Rc::clone(&received);
        let sub = bus.subscribe(move |_| *r.borrow_mut() += 1);

        bus.publish(&Event::Trade { symbol: btc(), trade: make_trade() });
        assert_eq!(bus.subscriber_count(), 1);

        sub.unsubscribe();
        assert_eq!(bus.subscriber_count(), 0);

        bus.publish(&Event::Trade { symbol: btc(), trade: make_trade() });
        assert_eq!(*received.borrow(), 1);
    }

    #[test]
    fn test_drop_unsubscribes() {
        let bus = EventBus::new();
        {
            let _sub = bus.subscribe(|_| {});
            assert_eq!(bus.subscriber_count(), 1);
        }
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn test_subscribe_during_publish() {
        let bus = EventBus::new();
        let bus2 = bus.clone();
        let slot: Rc<RefCell<Option<Subscription>>> = Rc::new(RefCell::new(None));
        let slot2 = Rc::clone(&slot);
        let _outer = bus.subscribe(move |_| {
            if slot2.borrow().is_none() {
                let sub = bus2.subscribe(|_| {});
                *slot2.borrow_mut() = Some(sub);
            }
        });
        bus.publish(&Event::Trade { symbol: btc(), trade: make_trade() });
        // Outer subscription plus the inner subscription created during dispatch.
        assert_eq!(bus.subscriber_count(), 2);
    }

    #[test]
    fn test_event_type() {
        let candle = OHLCV::new(ts(), 1.0, 2.0, 0.5, 1.5, 10.0);
        let e = Event::NewCandle { symbol: btc(), candle };
        assert_eq!(e.event_type(), EventType::NewCandle);

        let e = Event::Trade { symbol: btc(), trade: make_trade() };
        assert_eq!(e.event_type(), EventType::Trade);
    }

    #[test]
    fn test_publish_order_is_preserved() {
        let bus = EventBus::new();
        let order = Rc::new(RefCell::new(Vec::new()));
        let o = Rc::clone(&order);
        let _sub = bus.subscribe(move |e| {
            if let Event::Trade { trade, .. } = e {
                o.borrow_mut().push(trade.price);
            }
        });

        let trades = vec![100.0, 101.0, 102.0, 103.0];
        for p in &trades {
            let t = Trade::new(ts(), *p, 1.0, Side::Buy, "order1".to_string());
            bus.publish(&Event::Trade { symbol: btc(), trade: t });
        }
        assert_eq!(*order.borrow(), trades);
    }
}
