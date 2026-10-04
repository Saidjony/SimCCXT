//! Read-only reconstruction of round-trip trades from raw broker fills (FIFO).
//!
//! The engine is the only source of truth: this module never touches session
//! state — it replays the fill log the same way a real clearing house would:
//! per symbol, oldest first, closing the oldest open lot first (FIFO), with
//! pro-rata fee allocation on partial closes and support for shorts, flips
//! (long → short in one fill) and simultaneous positions across symbols.

use std::collections::{HashMap, VecDeque};

use serde_json::Value;

/// Quantity comparison slack (float fills rarely sum to exactly zero).
const EPS: f64 = 1e-9;

/// A completed round-trip trade (one closed lot).
pub struct Trade {
    pub symbol: String,
    /// "long" | "short"
    pub side: &'static str,
    /// "buy→sell" | "sell→buy" — computed here so callers cannot botch it.
    pub direction: &'static str,
    pub qty: f64,
    pub entry_price: f64,
    pub exit_price: f64,
    pub pnl: f64,
    pub r: f64,
    pub entry_time_ms: i64,
    pub exit_time_ms: i64,
    pub entry_fee: f64,
    pub exit_fee: f64,
    pub fee: f64,
    pub entry_order_id: String,
    pub exit_order_id: String,
}

/// One still-open position (leftover lots of a single symbol).
pub struct OpenPosition {
    pub symbol: String,
    /// "long" | "short"
    pub side: &'static str,
    pub qty: f64,
    /// Quantity-weighted average entry across the open lots.
    pub entry_price: f64,
    /// Time of the oldest open lot (chart entry marker).
    pub entry_time_ms: i64,
    pub mark_price: f64,
    pub unrealized: f64,
}

/// One unfilled entry lot waiting to be closed (FIFO queue element).
struct Lot {
    qty: f64,
    price: f64,
    fee: f64,
    time_ms: i64,
    order_id: String,
}

/// A single fill, parsed from the JSON rows of `Exchange::fetch_fills`.
struct Leg {
    side: String, // "buy" | "sell"
    price: f64,
    amount: f64,
    fee: f64,
    time_ms: i64,
    fill_index: u64,
    order_id: String,
}

/// Per-symbol reconstruction state: open lots + which way we are positioned.
struct SymState {
    side: i32, // +1 long, -1 short (valid while `lots` is non-empty)
    lots: VecDeque<Lot>,
    last_price: f64,
    legs: Vec<Leg>,
}

/// Reconstruct round-trips from raw fill records (FIFO per symbol).
///
/// `fills`: rows from `Exchange::fetch_fills` (any order), each carrying a
/// `time_ms` field (mirrored from `timestamp` by the caller) and a
/// `fill_index` = chronological position (older fill ⇒ smaller index), used
/// to break ties inside one millisecond. `mark_prices` supplies the current mark
/// per symbol for unrealized PnL; when a symbol is missing there, the last
/// fill price is used, and as a last resort the entry price.
///
/// Returns all closed trades (sorted by exit time) and every open position
/// (sorted by symbol).
pub fn reconstruct_trades(
    fills: Vec<Value>,
    risk_usdt: f64,
    mark_prices: HashMap<String, f64>,
) -> (Vec<Trade>, Vec<OpenPosition>) {
    // Group legs per symbol, oldest first (time_ms, then age tie-break).
    let mut by_sym: HashMap<String, SymState> = HashMap::new();
    for f in &fills {
        let symbol = match f["symbol"].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let leg = Leg {
            side: f["side"].as_str().unwrap_or("buy").to_string(),
            price: f["price"].as_f64().unwrap_or(0.0),
            amount: f["amount"].as_f64().unwrap_or(0.0),
            fee: f["fee"].as_f64().unwrap_or(0.0),
            time_ms: f.get("time_ms").and_then(|v| v.as_i64()).unwrap_or(0),
            fill_index: f.get("fill_index").and_then(|v| v.as_u64()).unwrap_or(0),
            order_id: f["orderId"].as_str().unwrap_or("").to_string(),
        };
        if leg.amount <= 0.0 || !leg.price.is_finite() {
            continue;
        }
        by_sym
            .entry(symbol.clone())
            .or_insert_with(|| SymState {
                side: 0,
                lots: VecDeque::new(),
                last_price: 0.0,
                legs: Vec::new(),
            })
            .legs
            .push(leg);
    }

    let mut trades: Vec<Trade> = Vec::new();
    let mut positions: Vec<OpenPosition> = Vec::new();

    for (symbol, state) in &mut by_sym {
        state
            .legs
            .sort_by_key(|l| (l.time_ms, l.fill_index));
        let legs = std::mem::take(&mut state.legs);

        for leg in legs {
            state.last_price = leg.price;
            let dir: i32 = if leg.side == "sell" { -1 } else { 1 };

            // Flat: every fill opens (long or short).
            if state.lots.is_empty() {
                state.side = dir;
                push_lot(state, &leg, leg.amount, leg.fee);
                continue;
            }

            // Same direction: add to the position.
            if dir == state.side {
                push_lot(state, &leg, leg.amount, leg.fee);
                continue;
            }

            // Opposite direction: close lots FIFO, then maybe flip.
            let mut left = leg.amount;
            let exit_fee_total = leg.fee;
            while left > EPS && !state.lots.is_empty() {
                let lot = state.lots.front_mut().expect("checked non-empty");
                let close_qty = lot.qty.min(left);
                let entry_fee = if lot.qty > 0.0 {
                    lot.fee * close_qty / lot.qty
                } else {
                    0.0
                };
                let exit_fee = exit_fee_total * close_qty / leg.amount;
                let pnl = (leg.price - lot.price) * close_qty * state.side as f64
                    - entry_fee
                    - exit_fee;
                let r = if risk_usdt > 0.0 && risk_usdt.is_finite() {
                    pnl / risk_usdt
                } else {
                    0.0
                };
                trades.push(Trade {
                    symbol: symbol.clone(),
                    side: if state.side > 0 { "long" } else { "short" },
                    direction: if state.side > 0 { "buy→sell" } else { "sell→buy" },
                    qty: close_qty,
                    entry_price: lot.price,
                    exit_price: leg.price,
                    pnl,
                    r,
                    entry_time_ms: lot.time_ms,
                    exit_time_ms: leg.time_ms,
                    entry_fee,
                    exit_fee,
                    fee: entry_fee + exit_fee,
                    entry_order_id: lot.order_id.clone(),
                    exit_order_id: leg.order_id.clone(),
                });
                lot.qty -= close_qty;
                lot.fee -= entry_fee;
                left -= close_qty;
                if lot.qty <= EPS {
                    state.lots.pop_front();
                }
            }

            if left > EPS {
                // The fill overshot the position: flip to the other side
                // with the remainder (the fee share not yet spent closes).
                state.side = dir;
                push_lot(state, &leg, left, exit_fee_total * left / leg.amount);
            } else if state.lots.is_empty() {
                state.side = 0;
            }
        }

        // Leftover lots = an open position for this symbol.
        if !state.lots.is_empty() && state.side != 0 {
            let qty: f64 = state.lots.iter().map(|l| l.qty).sum();
            let notional: f64 = state.lots.iter().map(|l| l.qty * l.price).sum();
            let fee_open: f64 = state.lots.iter().map(|l| l.fee).sum();
            let entry_price = if qty > EPS { notional / qty } else { 0.0 };
            let entry_time_ms = state.lots.front().map(|l| l.time_ms).unwrap_or(0);
            // Fallback chain: snapshot mark → last fill price → entry price
            // (unrealized ≈ −fees) — never a garbage zero mark.
            let mark_price = match mark_prices.get(symbol).copied().filter(|p| *p > 0.0) {
                Some(p) => p,
                None if state.last_price > 0.0 => state.last_price,
                None => entry_price,
            };
            let unrealized = (mark_price - entry_price) * qty * state.side as f64 - fee_open;
            positions.push(OpenPosition {
                symbol: symbol.clone(),
                side: if state.side > 0 { "long" } else { "short" },
                qty,
                entry_price,
                entry_time_ms,
                mark_price,
                unrealized,
            });
        }
    }

    trades.sort_by_key(|t| (t.exit_time_ms, t.entry_time_ms));
    positions.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    (trades, positions)
}

/// Append `qty` of `leg` as a fresh lot (fee = the given share).
fn push_lot(state: &mut SymState, leg: &Leg, qty: f64, fee: f64) {
    state.lots.push_back(Lot {
        qty,
        price: leg.price,
        fee,
        time_ms: leg.time_ms,
        order_id: leg.order_id.clone(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Newest-first fill row, mirroring `Exchange::fetch_fills` + the
    /// `time_ms`/`fill_index` metadata the engine attaches.
    fn fill(side: &str, price: f64, amount: f64, fee: f64, time_ms: i64, age: u64) -> Value {
        json!({
            "symbol": "BTC/USDT",
            "side": side,
            "price": price,
            "amount": amount,
            "fee": fee,
            "orderId": format!("ord-{age}"),
            "timestamp": time_ms,
            "time_ms": time_ms,
            "fill_index": age,
        })
    }

    /// Feed chronological fills (oldest first) and reverse to mimic the
    /// newest-first shape of `fetch_fills`. `fill_index` = chronological
    /// position (older ⇒ smaller), assigned by the engine caller the same way.
    fn run(chrono: Vec<Value>) -> (Vec<Trade>, Vec<OpenPosition>) {
        let mut with_index: Vec<Value> = chrono
            .into_iter()
            .enumerate()
            .map(|(i, mut f)| {
                f["fill_index"] = json!(i as u64);
                f
            })
            .collect();
        with_index.reverse(); // newest first, like fetch_fills
        reconstruct_trades(with_index, 100.0, HashMap::new())
    }

    #[test]
    fn long_round_trip_full_close() {
        let (trades, positions) = run(vec![
            fill("buy", 100.0, 1.0, 1.0, 1_000, 0),
            fill("sell", 110.0, 1.0, 1.0, 2_000, 0),
        ]);
        assert_eq!(trades.len(), 1);
        let t = &trades[0];
        assert_eq!(t.side, "long");
        assert_eq!(t.direction, "buy→sell");
        assert_eq!(t.qty, 1.0);
        assert!((t.pnl - ((110.0 - 100.0) * 1.0 - 2.0)).abs() < 1e-9);
        assert!((t.r - (10.0 - 2.0) / 100.0).abs() < 1e-9);
        assert_eq!(t.entry_time_ms, 1_000);
        assert_eq!(t.exit_time_ms, 2_000);
        assert!(positions.is_empty(), "fully closed ⇒ no open position");
    }

    #[test]
    fn partial_closes_do_not_hang_or_double_count() {
        // Regression: the old loop spun forever when sell.amount < buy.amount.
        let (trades, positions) = run(vec![
            fill("buy", 100.0, 2.0, 2.0, 1_000, 0),
            fill("sell", 110.0, 1.0, 1.0, 2_000, 0),
            fill("sell", 120.0, 1.0, 1.0, 3_000, 0),
        ]);
        assert_eq!(trades.len(), 2);
        assert!((trades[0].qty - 1.0).abs() < 1e-9);
        assert!((trades[1].qty - 1.0).abs() < 1e-9);
        // Fees are split pro-rata: each half of the 2.0 entry fee.
        assert!((trades[0].entry_fee - 1.0).abs() < 1e-9);
        assert!((trades[1].entry_fee - 1.0).abs() < 1e-9);
        assert!(positions.is_empty());
    }

    #[test]
    fn one_sell_spanning_many_buys_is_fifo() {
        let (trades, positions) = run(vec![
            fill("buy", 100.0, 1.0, 0.0, 1_000, 0),
            fill("buy", 105.0, 1.0, 0.0, 1_500, 0),
            fill("sell", 110.0, 2.0, 0.0, 2_000, 0),
        ]);
        assert_eq!(trades.len(), 2);
        assert_eq!(trades[0].entry_price, 100.0); // oldest lot first
        assert_eq!(trades[1].entry_price, 105.0);
        let total: f64 = trades.iter().map(|t| t.pnl).sum();
        assert!((total - 15.0).abs() < 1e-9);
        assert!(positions.is_empty(), "the sell must not double-count");
    }

    #[test]
    fn short_round_trip() {
        let (trades, positions) = run(vec![
            fill("sell", 110.0, 1.0, 1.0, 1_000, 0),
            fill("buy", 100.0, 1.0, 1.0, 2_000, 0),
        ]);
        assert_eq!(trades.len(), 1);
        let t = &trades[0];
        assert_eq!(t.side, "short");
        assert_eq!(t.direction, "sell→buy");
        assert!((t.pnl - ((110.0 - 100.0) - 2.0)).abs() < 1e-9);
        assert!(positions.is_empty());
    }

    #[test]
    fn flip_long_to_short_in_one_fill() {
        let (trades, positions) = run(vec![
            fill("buy", 100.0, 1.0, 0.0, 1_000, 0),
            fill("sell", 110.0, 3.0, 0.0, 2_000, 0),
        ]);
        // 1 long closed, 2 left short.
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].side, "long");
        assert!((trades[0].pnl - 10.0).abs() < 1e-9);
        assert_eq!(positions.len(), 1);
        let p = &positions[0];
        assert_eq!(p.side, "short");
        assert!((p.qty - 2.0).abs() < 1e-9);
        assert_eq!(p.entry_price, 110.0);
        assert_eq!(p.entry_time_ms, 2_000);
    }

    #[test]
    fn open_long_position_with_mark() {
        let mut marks = HashMap::new();
        marks.insert("BTC/USDT".to_string(), 120.0);
        let mut newest_first = vec![fill("buy", 100.0, 1.0, 1.0, 1_000, 0)];
        newest_first[0]["fill_index"] = json!(0);
        let (trades, positions) = reconstruct_trades(newest_first, 100.0, marks);
        assert!(trades.is_empty());
        assert_eq!(positions.len(), 1);
        let p = &positions[0];
        assert_eq!(p.side, "long");
        assert!((p.unrealized - ((120.0 - 100.0) * 1.0 - 1.0)).abs() < 1e-9);
    }

    #[test]
    fn mark_falls_back_to_last_fill_price_when_snapshot_misses() {
        // No mark in the snapshot ⇒ use the last fill (105), not 0.
        let (trades, positions) = run(vec![
            fill("buy", 100.0, 1.0, 0.0, 1_000, 0),
            fill("buy", 105.0, 1.0, 0.0, 1_500, 0),
        ]);
        assert!(trades.is_empty());
        let p = &positions[0];
        assert_eq!(p.mark_price, 105.0);
        assert!((p.qty - 2.0).abs() < 1e-9);
        assert!((p.entry_price - 102.5).abs() < 1e-9);
        assert!((p.unrealized - 5.0).abs() < 1e-9);
    }

    #[test]
    fn positions_open_in_different_symbols_both_survive() {
        let mut newest_first = vec![
            fill("buy", 100.0, 1.0, 0.0, 1_000, 0),
            fill("buy", 50.0, 2.0, 0.0, 1_100, 0),
        ];
        newest_first[1]["symbol"] = json!("ETH/USDT");
        newest_first[0]["fill_index"] = json!(0);
        newest_first[1]["fill_index"] = json!(1);
        let (trades, positions) = reconstruct_trades(newest_first, 100.0, HashMap::new());
        assert!(trades.is_empty());
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].symbol, "BTC/USDT");
        assert_eq!(positions[1].symbol, "ETH/USDT");
    }

    #[test]
    fn same_millisecond_fills_keep_age_order() {
        // Three fills inside one millisecond: age tie-break must keep FIFO.
        let (trades, _positions) = run(vec![
            fill("buy", 100.0, 1.0, 0.0, 1_000, 0),
            fill("buy", 101.0, 1.0, 0.0, 1_000, 0),
            fill("sell", 110.0, 2.0, 0.0, 1_000, 0),
        ]);
        assert_eq!(trades.len(), 2);
        assert_eq!(trades[0].entry_price, 100.0);
        assert_eq!(trades[1].entry_price, 101.0);
    }

    #[test]
    fn zero_risk_does_not_panic_and_gives_zero_r() {
        let mut chrono = vec![
            fill("buy", 100.0, 1.0, 0.0, 1_000, 0),
            fill("sell", 110.0, 1.0, 0.0, 2_000, 0),
        ];
        for (i, f) in chrono.iter_mut().enumerate() {
            f["fill_index"] = json!(i as u64);
        }
        chrono.reverse();
        let (trades, _) = reconstruct_trades(chrono, 0.0, HashMap::new());
        assert_eq!(trades[0].r, 0.0);
    }
}
