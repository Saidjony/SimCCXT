use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::slice;

use chrono::{TimeZone, Utc};
use serde_json::{json, Value};
use simccxt_broker::MarketKind;
use simccxt_core::{Config, FeeTiers, Symbol, TimeConfig, TimeInForce, OHLCV};
use simccxt_exchange::{CreateOrderParams, Exchange};

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

struct Engine {
    exchange: Exchange,
    symbol: String,
    timeframe: String,
    exchange_name: String,
    /// Random id of this browser engine instance (one per page load).
    ///
    /// Order ids restart at 1 every time the page (and therefore the engine)
    /// is rebuilt, and a reloaded tab replays from the same bar, so the same
    /// `symbol|orderId|time` describes two different trades. Everything the
    /// diary keys per trade carries this id so an older session's stop/TP or
    /// exit verdict can never be applied to a newer trade.
    engine_id: String,
    step_ms: u64,
    speed: f64,
    reveal_reset_ms: i64,
    range_end_ms: i64,
    candles: Vec<Vec<f64>>,
    oi: Vec<[f64; 2]>,
    oi_period: String,
    fills_seen: usize,
    fills_reported: usize,
    candles_reported: usize,
    pending_protections: Vec<PendingProtection>,
    active_protections: Vec<ActiveProtection>,
    entry_protections: HashMap<String, Value>,
    exit_reasons: HashMap<String, String>,
    reported_entry_protections: HashSet<String>,
    reported_exit_reasons: HashSet<String>,
    total: usize,
}

struct PendingProtection {
    entry_order_id: String,
    symbol: String,
    entry_side: String,
    remaining: f64,
    stop_price: f64,
    take_price: Option<f64>,
}

struct ActiveProtection {
    order_id: String,
    symbol: String,
    entry_side: String,
    kind: &'static str,
}

fn init(message: &Value) -> Result<(), String> {
    let symbol = message["symbol"]
        .as_str()
        .ok_or_else(|| "symbol is required".to_string())?;
    let timeframe = message["timeframe"]
        .as_str()
        .ok_or_else(|| "timeframe is required".to_string())?;
    let start_ms = message["start_ms"]
        .as_i64()
        .ok_or_else(|| "start_ms is required".to_string())?;
    let step_ms = message["step_ms"]
        .as_u64()
        .filter(|step_ms| *step_ms > 0)
        .ok_or_else(|| "step_ms must be greater than zero".to_string())?;
    let market_kind = message["market_kind"]
        .as_str()
        .ok_or_else(|| "market_kind is required".to_string())?;
    let exchange_name = message["exchange"].as_str().unwrap_or("binance");
    let engine_id = message["engine_id"].as_str().unwrap_or_default().to_string();
    let speed = message["speed"].as_f64().unwrap_or(1.0);
    let reveal_reset_ms = message["reveal_reset_ms"].as_i64().unwrap_or(start_ms);
    let range_end_ms = message["range_end_ms"].as_i64().unwrap_or(i64::MAX);
    let oi = serde_json::from_value(message["oi"].clone()).unwrap_or_default();
    let oi_period = message["oi_period"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let rows: Vec<Vec<f64>> = serde_json::from_value(message["candles"].clone())
        .map_err(|error| format!("invalid candles: {error}"))?;
    let candles = parse_candles(&rows)?;

    let symbol_value = Symbol::parse(symbol).map_err(|error| error.to_string())?;
    let kind = match market_kind {
        "spot" => MarketKind::Spot,
        "linear" => MarketKind::Linear,
        _ => return Err("market_kind must be 'spot' or 'linear'".to_string()),
    };
    let initial_time = Utc
        .timestamp_millis_opt(start_ms)
        .single()
        .ok_or_else(|| "start_ms is out of range".to_string())?;
    let config = Config {
        default_seed: 42,
        default_leverage: 1.0,
        max_order_size: 1_000_000.0,
        min_order_size: 0.001,
        fee_tiers: FeeTiers {
            maker: 0.0,
            taker: 0.0,
            funding: 0.0001,
        },
        time_config: TimeConfig {
            initial_time,
            step_ms,
            paused: true,
        },
    };
    let mut exchange = Exchange::new(config);
    exchange.deposit("USDT", 1_000_000.0);
    exchange.add_market(&symbol_value, kind);
    exchange.load_candles(symbol, timeframe, candles);
    exchange.set_step_ms(step_ms);
    let start_time = Utc
        .timestamp_millis_opt(start_ms)
        .single()
        .ok_or_else(|| "start_ms is out of range".to_string())?;
    exchange.mark_history_played(&symbol_value, timeframe, start_time);

    ENGINE.with(|engine| {
        *engine.borrow_mut() = Some(Engine {
            exchange,
            symbol: symbol.to_string(),
            timeframe: timeframe.to_string(),
            exchange_name: exchange_name.to_string(),
            engine_id,
            step_ms,
            speed,
            reveal_reset_ms,
            range_end_ms,
            total: rows.len(),
            candles: rows,
            oi,
            oi_period,
            fills_seen: 0,
            fills_reported: 0,
            candles_reported: 0,
            pending_protections: Vec::new(),
            active_protections: Vec::new(),
            entry_protections: HashMap::new(),
            exit_reasons: HashMap::new(),
            reported_entry_protections: HashSet::new(),
            reported_exit_reasons: HashSet::new(),
        });
    });
    Ok(())
}

fn parse_candles(rows: &[Vec<f64>]) -> Result<Vec<OHLCV>, String> {
    rows.iter()
        .map(|row| {
            if row.len() < 6 {
                return Err("each candle must contain six values".to_string());
            }
            let timestamp = Utc
                .timestamp_millis_opt(row[0] as i64)
                .single()
                .ok_or_else(|| "candle timestamp is out of range".to_string())?;
            Ok(OHLCV::new(
                timestamp, row[1], row[2], row[3], row[4], row[5],
            ))
        })
        .collect()
}

fn command(command: &Value) -> Result<(), String> {
    ENGINE.with(|engine| {
        let mut engine = engine.borrow_mut();
        let engine = engine
            .as_mut()
            .ok_or_else(|| "engine is not initialized".to_string())?;
        match command["type"].as_str().unwrap_or_default() {
            "play" => engine.exchange.resume(),
            "pause" => engine.exchange.pause(),
            "step" => {
                let steps = command["steps"].as_u64().unwrap_or(1);
                engine.exchange.resume();
                engine.exchange.step_n(steps);
                engine.exchange.pause();
                if engine.exchange.now_ms() >= engine.range_end_ms {
                    engine.exchange.pause();
                }
            }
            "tick" => {
                engine.exchange.resume();
                engine
                    .exchange
                    .step_n(command["steps"].as_u64().unwrap_or(1));
                if engine.exchange.now_ms() >= engine.range_end_ms {
                    engine.exchange.pause();
                }
            }
            "set_step" => {
                let step_ms = command["step_ms"]
                    .as_u64()
                    .filter(|step_ms| *step_ms > 0)
                    .ok_or_else(|| "step_ms must be greater than zero".to_string())?;
                engine.exchange.set_step_ms(step_ms);
                engine.step_ms = step_ms;
            }
            "set_speed" => {
                let speed = command["speed"]
                    .as_f64()
                    .filter(|speed| speed.is_finite() && *speed > 0.0)
                    .ok_or_else(|| "speed must be greater than zero".to_string())?;
                engine.speed = speed;
            }
            "reset" => {
                let start_ms = command["timestamp_ms"]
                    .as_i64()
                    .unwrap_or(engine.reveal_reset_ms);
                let timestamp = Utc
                    .timestamp_millis_opt(start_ms)
                    .single()
                    .ok_or_else(|| "timestamp_ms is out of range".to_string())?;
                engine.exchange.reset(timestamp);
                engine.candles_reported = 0;
                // `reset` clears every replay cursor; mark the one series this
                // tab owns as already played, so bars before the barrier do not
                // fire stops/TPs again.
                if let Ok(symbol) = Symbol::parse(&engine.symbol) {
                    engine.exchange.mark_history_played(
                        &symbol,
                        &engine.timeframe,
                        timestamp,
                    );
                }
            }
            "switch_series" => {
                let symbol = command["symbol"]
                    .as_str()
                    .ok_or_else(|| "symbol is required".to_string())?;
                let timeframe = command["timeframe"]
                    .as_str()
                    .ok_or_else(|| "timeframe is required".to_string())?;
                let exchange = command["exchange"]
                    .as_str()
                    .unwrap_or(&engine.exchange_name);
                let step_ms = command["step_ms"]
                    .as_u64()
                    .filter(|step_ms| *step_ms > 0)
                    .ok_or_else(|| "step_ms must be greater than zero".to_string())?;
                let rows: Vec<Vec<f64>> = serde_json::from_value(command["candles"].clone())
                    .map_err(|error| format!("invalid candles: {error}"))?;
                let candles = parse_candles(&rows)?;
                let symbol_value = Symbol::parse(symbol).map_err(|error| error.to_string())?;
                let kind = match command["market_kind"].as_str().unwrap_or("linear") {
                    "spot" => MarketKind::Spot,
                    "linear" => MarketKind::Linear,
                    _ => return Err("market_kind must be 'spot' or 'linear'".to_string()),
                };
                let now_ms = engine.exchange.now_ms();
                engine.exchange.add_market(&symbol_value, kind);
                engine.exchange.load_candles(symbol, timeframe, candles);
                // This tab replays ONE series. Dropping every previously loaded
                // coin/interval matters: `feed_due` walks all loaded series, so
                // otherwise pressing play advanced the old coins in the
                // background too (their stops/TPs fired off-screen).
                engine.exchange.retain_candles(symbol, timeframe);
                engine.exchange.set_step_ms(step_ms);
                let same_window = command["preserve_clock"].as_bool().unwrap_or(false);
                let reveal_reset_ms = command["reveal_reset_ms"].as_i64().unwrap_or(now_ms);
                // A different coin/venue starts at its OWN reveal boundary. The
                // price and the "already played" mark must use that same time:
                // taking them from the previous coin's clock left the new series
                // showing a bar (and a watchlist price) from the future relative
                // to its own replay start.
                let clock_ms = if same_window { now_ms } else { reveal_reset_ms };
                let current_time = Utc
                    .timestamp_millis_opt(clock_ms)
                    .single()
                    .ok_or_else(|| "current replay time is out of range".to_string())?;
                let rows_before_clock = rows
                    .iter()
                    .filter(|row| {
                        row.first()
                            .is_some_and(|timestamp| *timestamp <= clock_ms as f64)
                    })
                    .collect::<Vec<_>>();
                if let Some(last) = rows_before_clock.last() {
                    let price = engine
                        .exchange
                        .last_price_for(symbol)
                        .filter(|price| *price > 0.0)
                        .unwrap_or(last[4]);
                    engine.exchange.set_last_price(symbol, price);
                    engine
                        .exchange
                        .apply_price(&symbol_value, price, current_time);
                }
                if !same_window {
                    engine.exchange.reset(current_time);
                }
                // AFTER `reset`, which clears the replay marks: otherwise the
                // next tick would feed the pre-revealed history through the
                // broker and fire old stops/TPs at months-old prices.
                engine
                    .exchange
                    .mark_history_played(&symbol_value, timeframe, current_time);
                engine.symbol = symbol.to_string();
                engine.timeframe = timeframe.to_string();
                engine.exchange_name = exchange.to_string();
                engine.step_ms = step_ms;
                engine.range_end_ms = command["range_end_ms"].as_i64().unwrap_or(i64::MAX);
                engine.reveal_reset_ms = reveal_reset_ms;
                engine.candles = rows;
                engine.total = engine.candles.len();
                engine.candles_reported = 0;
                engine.oi = serde_json::from_value(command["oi"].clone()).unwrap_or_default();
                engine.oi_period = command["oi_period"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
            }
            "place_order" => {
                let symbol = command["symbol"].as_str().unwrap_or(&engine.symbol);
                let order_type = command["order_type"].as_str().unwrap_or("market");
                let side = command["side"].as_str().unwrap_or_default();
                let mut amount = command["quantity"].as_f64().unwrap_or_default();
                let price = command["price"].as_f64();
                let reduce_only = command["reduce_only"].as_bool().unwrap_or(false);
                let stop_loss_price = command["stop_loss_price"]
                    .as_f64()
                    .filter(|value| value.is_finite() && *value > 0.0);
                // Risk-based sizing at the ACTUAL entry reference. The panel
                // computes its preview from the last rendered price, but the
                // replay may tick between the click and this command. Sizing
                // from that stale price makes a normal stop-out cost >1R;
                // re-size here so the stop is always the trader's budget.
                let risk_usdt = command["risk_usdt"].as_f64().unwrap_or(0.0);
                if let Some(stop) = stop_loss_price {
                    if risk_usdt > 0.0 && !reduce_only {
                        let entry_reference = price
                            .or_else(|| command["stop_price"].as_f64())
                            .or_else(|| engine.exchange.last_price_for(symbol));
                        if let Some(entry) = entry_reference.filter(|value| *value > 0.0) {
                            let taker = engine.exchange.broker().fee_schedule().taker;
                            let per_unit = (entry - stop).abs()
                                + (entry.abs() + stop.abs()) * taker;
                            if per_unit > 0.0 {
                                amount = risk_usdt / per_unit;
                            }
                        }
                    }
                }
                let mut params = CreateOrderParams::new();
                if let Some(value) = command["stop_price"].as_f64() {
                    params = params.stop_price(value);
                }
                // NOTE: `tp_price` is deliberately NOT attached to the entry
                // order. For a stop/limit entry the broker would arm that
                // take-profit immediately, before the entry ever fills: the
                // trade then had TWO take-profits (this one and the one armed
                // by `pending_protections`), the untracked one could fire while
                // flat or survive a manual close, and cancelling the entry left
                // it behind. The pending protection arms stop and target from
                // the fill that actually opened the position.
                params.time_in_force = match command["time_in_force"]
                    .as_str()
                    .unwrap_or("GTC")
                    .to_ascii_uppercase()
                    .as_str()
                {
                    "IOC" => TimeInForce::IOC,
                    "FOK" => TimeInForce::FOK,
                    "GTX" => TimeInForce::GTX,
                    "DAY" => TimeInForce::DAY,
                    "GTD" => TimeInForce::GTD,
                    _ => TimeInForce::GTC,
                };
                params.reduce_only = reduce_only;
                params.post_only = command["post_only"].as_bool().unwrap_or(false);
                let order = engine
                    .exchange
                    .create_order(symbol, order_type, side, amount, price, params)
                    .map_err(|error| error.to_string())?;
                let stop_price = stop_loss_price.unwrap_or(0.0);
                let take_price = command["tp_price"].as_f64();
                if stop_price > 0.0 || take_price.is_some() {
                    engine.pending_protections.push(PendingProtection {
                        entry_order_id: order["id"].as_str().unwrap_or_default().to_string(),
                        symbol: symbol.to_string(),
                        entry_side: side.to_string(),
                        remaining: amount,
                        stop_price,
                        take_price,
                    });
                }
                process_fresh_fills(engine)?;
            }
            "cancel_order" => {
                let order_id = command["order_id"].as_str().unwrap_or_default();
                let symbol = command["symbol"].as_str().unwrap_or(&engine.symbol);
                engine
                    .exchange
                    .cancel_order(order_id, symbol)
                    .map_err(|error| error.to_string())?;
            }
            "close_position" => {
                // Own the string: `engine` is borrowed mutably further down
                // (`process_fresh_fills`, cancelling the protections).
                let symbol = command["symbol"]
                    .as_str()
                    .unwrap_or(&engine.symbol)
                    .to_string();
                let quantity = command["quantity"].as_f64();
                let position = engine.exchange.account_snapshot()["positions"]
                    .as_array()
                    .and_then(|positions| {
                        positions.iter().find(|position| {
                            position["symbol"] == symbol
                                && position["qty"].as_f64().unwrap_or(0.0) > 0.0
                        })
                    })
                    .cloned()
                    .ok_or_else(|| format!("no open position for {symbol}"))?;
                let side = if position["side"] == "short" {
                    "buy"
                } else {
                    "sell"
                };
                let amount = quantity
                    .unwrap_or_else(|| position["qty"].as_f64().unwrap_or(0.0))
                    .min(position["qty"].as_f64().unwrap_or(0.0));
                let mut params = CreateOrderParams::new();
                params.reduce_only = true;
                engine
                    .exchange
                    .create_order(&symbol, "market", side, amount, None, params)
                    .map_err(|error| error.to_string())?;
                process_fresh_fills(engine)?;
                // The armed stop/target are reduce-only orders of their own and
                // a manual close never triggers them, so nothing would ever
                // retire them: they would keep drawing lines on the chart and,
                // worse, could close the NEXT position. A flat position owns no
                // protection — cancel and forget them.
                let flat = !engine.exchange.account_snapshot()["positions"]
                    .as_array()
                    .is_some_and(|positions| {
                        positions.iter().any(|position| {
                            position["symbol"] == symbol
                                && position["qty"].as_f64().unwrap_or(0.0) > f64::EPSILON
                        })
                    });
                if flat {
                    let mut stale = Vec::new();
                    engine.active_protections.retain(|active| {
                        if active.symbol == symbol {
                            stale.push(active.order_id.clone());
                            false
                        } else {
                            true
                        }
                    });
                    for order_id in stale {
                        // A protection that already fired is gone from the book.
                        let _ = engine.exchange.cancel_order(&order_id, &symbol);
                    }
                }
            }
            _ => return Err("unsupported engine command".to_string()),
        }
        process_fresh_fills(engine)?;
        Ok(())
    })
}

fn process_fresh_fills(engine: &mut Engine) -> Result<(), String> {
    let fill_count = engine.exchange.fill_count();
    if fill_count <= engine.fills_seen {
        return Ok(());
    }
    let mut fresh: Vec<Value> = engine
        .exchange
        .fetch_fills(None, None)
        .into_iter()
        .take(fill_count - engine.fills_seen)
        .collect();
    fresh.reverse();
    for fill in fresh {
        let symbol = fill["symbol"].as_str().unwrap_or_default().to_string();
        let side = fill["side"].as_str().unwrap_or_default().to_string();
        let order_id = fill["orderId"].as_str().unwrap_or_default().to_string();
        let fill_timestamp = fill["timestamp"].as_i64().unwrap_or_default();
        let source_order_id = engine.exchange.take_triggered_order_source(&order_id);
        let protection_kind = engine
            .active_protections
            .iter()
            .find(|active| {
                active.order_id == order_id
                    || source_order_id.as_deref() == Some(active.order_id.as_str())
            })
            .map(|active| active.kind);
        if let Some(kind) = protection_kind {
            // Order ids restart at 1 for every symbol engine and every replay
            // session, so neither the bare id nor `symbol|id` is unique: a
            // reloaded session's stop #2 would overwrite the previous stop #2
            // of the same symbol and the diary would label the newer trade
            // with the older trade's level. The engine id separates page loads
            // and the fill time pins the execution within one engine.
            let engine_id = engine.engine_id.clone();
            engine.exit_reasons.insert(
                format!("{engine_id}|{symbol}|{order_id}|{fill_timestamp}"),
                kind.to_string(),
            );
        }

        let amount = fill["amount"].as_f64().unwrap_or(0.0);
        let pending_index = engine.pending_protections.iter().position(|pending| {
            pending.symbol == symbol
                && pending.entry_side == side
                && (pending.entry_order_id == order_id
                    || source_order_id.as_deref() == Some(pending.entry_order_id.as_str()))
        });
        if let Some(index) = pending_index {
            let pending = &engine.pending_protections[index];
            let protect_amount = amount.min(pending.remaining);
            let stop_price = pending.stop_price;
            let take_price = pending.take_price;
            let entry_side = pending.entry_side.clone();
            let protect_side = if entry_side == "buy" { "sell" } else { "buy" };
            // Key the plan by the fill that actually opened the trade, not by
            // the order the user clicked: a stop/limit entry that triggers is
            // executed by a NEW market order, and the fill reports that id.
            // Order ids restart at 1 per symbol AND per page load (the engine
            // id separates the loads), so both are part of the key.
            let plan_key = format!(
                "{}|{symbol}|{order_id}|{fill_timestamp}",
                engine.engine_id
            );
            let plan = engine
                .entry_protections
                .entry(plan_key)
                .or_insert_with(|| json!({ "stop_price": Value::Null, "take_price": Value::Null }));
            if stop_price > 0.0 {
                plan["stop_price"] = json!(stop_price);
            }
            if let Some(take_price) = take_price {
                plan["take_price"] = json!(take_price);
            }

            for (order_type, trigger, kind) in [
                ("stop", stop_price, "stop"),
                ("take_profit", take_price.unwrap_or(0.0), "take"),
            ] {
                if trigger <= 0.0 {
                    continue;
                }
                let mut params = CreateOrderParams::new()
                    .stop_price(trigger)
                    .reduce_only(true);
                params.time_in_force = TimeInForce::GTC;
                let order = engine
                    .exchange
                    .create_order(
                        &symbol,
                        order_type,
                        protect_side,
                        protect_amount,
                        None,
                        params,
                    )
                    .map_err(|error| {
                        format!("could not arm {kind} protection for {symbol}: {error}")
                    })?;
                if let Some(id) = order["id"].as_str() {
                    engine.active_protections.push(ActiveProtection {
                        order_id: id.to_string(),
                        symbol: symbol.clone(),
                        entry_side: entry_side.clone(),
                        kind,
                    });
                }
            }

            engine.pending_protections[index].remaining -= protect_amount;
            if engine.pending_protections[index].remaining <= f64::EPSILON {
                engine.pending_protections.remove(index);
            }
        } else if let Some(index) = engine.active_protections.iter().position(|active| {
            active.symbol == symbol
                && source_order_id.as_deref() == Some(active.order_id.as_str())
                && active.entry_side != side
        }) {
            let closed_side = engine.active_protections[index].entry_side.clone();
            engine.active_protections.remove(index);
            let position_open = engine.exchange.account_snapshot()["positions"]
                .as_array()
                .is_some_and(|positions| {
                    positions.iter().any(|position| {
                        position["symbol"] == symbol
                            && position["qty"].as_f64().unwrap_or(0.0) > f64::EPSILON
                    })
                });
            if !position_open {
                let siblings: Vec<String> = engine
                    .active_protections
                    .iter()
                    .filter(|active| active.symbol == symbol && active.entry_side == closed_side)
                    .map(|active| active.order_id.clone())
                    .collect();
                engine
                    .active_protections
                    .retain(|active| active.symbol != symbol || active.entry_side != closed_side);
                for sibling in siblings {
                    if let Err(error) = engine.exchange.cancel_order(&sibling, &symbol) {
                        if !error.to_string().contains("unknown order") {
                            return Err(format!("could not cancel sibling protection: {error}"));
                        }
                    }
                }
            }
        }
    }
    engine.fills_seen = fill_count;
    Ok(())
}

fn snapshot(include_candles: bool, include_all_fills: bool) -> Result<Value, String> {
    ENGINE.with(|engine| {
        let mut engine = engine.borrow_mut();
        let engine = engine
            .as_mut()
            .ok_or_else(|| "engine is not initialized".to_string())?;
        let now_ms = engine.exchange.now_ms();
        let visible_count = engine
            .candles
            .partition_point(|row| {
                row.first()
                    .is_some_and(|timestamp| *timestamp <= now_ms as f64)
            });
        let full_reset = include_candles;
        let candle_start = if full_reset {
            0
        } else {
            engine.candles_reported.min(visible_count)
        };
        engine.candles_reported = visible_count;
        let candle_rows = if full_reset {
            Vec::new()
        } else {
            engine.candles[candle_start..visible_count].to_vec()
        };
        let fill_count = engine.exchange.fill_count();
        let all_fills = include_all_fills || fill_count < engine.fills_reported;
        let fills = if all_fills {
            engine.exchange.fetch_fills(None, None)
        } else {
            engine
                .exchange
                .fetch_fills(None, None)
                .into_iter()
                .take(fill_count.saturating_sub(engine.fills_reported))
                .collect()
        };
        engine.fills_reported = fill_count;
        let all_metadata = include_candles || include_all_fills;
        let entry_protections = engine
            .entry_protections
            .iter()
            .filter(|(id, _)| all_metadata || !engine.reported_entry_protections.contains(*id))
            .map(|(id, value)| (id.clone(), value.clone()))
            .collect::<HashMap<_, _>>();
        let exit_reasons = engine
            .exit_reasons
            .iter()
            .filter(|(id, _)| all_metadata || !engine.reported_exit_reasons.contains(*id))
            .map(|(id, reason)| (id.clone(), reason.clone()))
            .collect::<HashMap<_, _>>();
        engine
            .reported_entry_protections
            .extend(entry_protections.keys().cloned());
        engine
            .reported_exit_reasons
            .extend(exit_reasons.keys().cloned());
        Ok(json!({
            "type": "snapshot",
            "symbol": engine.symbol,
            "exchange": engine.exchange_name,
            "timeframe": engine.timeframe,
            "candles": Value::Null,
            "full_reset": full_reset,
            "candles_delta": candle_rows,
            "account": engine.exchange.account_snapshot(),
            "fills": fills,
            "entry_protections": entry_protections,
            "exit_reasons": exit_reasons,
            "oi": if include_candles { json!(engine.oi) } else { Value::Null },
            "oi_period": engine.oi_period,
            "range": {
                "reveal_reset_ms": engine.reveal_reset_ms,
                "end_ms": engine.range_end_ms
            },
            "replay": {
                "type": "replay_state",
                "paused": engine.exchange.is_paused(),
                "now_ms": now_ms,
                "step_ms": engine.step_ms,
                "fed": visible_count,
                "total": engine.total,
                "done": visible_count >= engine.total,
                "speed": engine.speed
            }
        }))
    })
}

fn process_message(bytes: &[u8]) -> Vec<u8> {
    let result = (|| -> Result<Value, String> {
        let message: Value =
            serde_json::from_slice(bytes).map_err(|error| format!("invalid message: {error}"))?;
        match message["type"].as_str().unwrap_or_default() {
            "init" => init(&message)?,
            "snapshot" => {}
            _ => command(&message)?,
        }
        let command_type = message["type"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let include_candles = matches!(
            command_type.as_str(),
            "init" | "reset" | "switch_series"
        );
        let include_all_fills = command_type == "init"
            || (command_type == "snapshot" && message["include_fills"].as_bool().unwrap_or(false));
        snapshot(include_candles, include_all_fills)
    })();
    let response = match result {
        Ok(snapshot) => json!({ "ok": true, "snapshot": snapshot }),
        Err(error) => json!({ "ok": false, "error": error }),
    };
    serde_json::to_vec(&response).unwrap_or_else(|error| {
        format!(r#"{{"ok":false,"error":"failed to encode response: {error}"}}"#).into_bytes()
    })
}

#[no_mangle]
pub extern "C" fn alloc(size: usize) -> *mut u8 {
    Box::into_raw(vec![0_u8; size].into_boxed_slice()) as *mut u8
}

#[no_mangle]
pub unsafe extern "C" fn dealloc(pointer: *mut u8, length: usize) {
    let buffer = unsafe { slice::from_raw_parts_mut(pointer, length) };
    drop(unsafe { Box::from_raw(buffer) });
}

#[no_mangle]
pub unsafe extern "C" fn handle_message(pointer: *const u8, length: usize) -> u64 {
    let request = unsafe { slice::from_raw_parts(pointer, length) };
    let response = process_message(request).into_boxed_slice();
    let response_length = response.len();
    let response_pointer = Box::into_raw(response) as *mut u8;
    ((response_pointer as u32 as u64) << 32) | response_length as u64
}

#[no_mangle]
pub unsafe extern "C" fn release_result(pointer: *mut u8, length: usize) {
    let response = unsafe { slice::from_raw_parts_mut(pointer, length) };
    drop(unsafe { Box::from_raw(response) });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_init_data_without_failing_the_process() {
        let response = process_message(br#"{"type":"init","symbol":"BTCUSDT"}"#);
        let value: Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"], "timeframe is required");
    }

    #[test]
    fn replays_loaded_candles_and_reports_visible_state() {
        let init = json!({
            "type": "init",
            "symbol": "BTC/USDT",
            "timeframe": "1m",
            "start_ms": 0,
            "step_ms": 60_000,
            "market_kind": "linear",
            "candles": [[60_000, 100.0, 110.0, 90.0, 105.0, 12.0]]
        });
        let init_response = process_message(&serde_json::to_vec(&init).unwrap());
        let init_value: Value = serde_json::from_slice(&init_response).unwrap();
        assert_eq!(init_value["ok"], true);
        assert!(init_value["snapshot"]["candles"].is_null());
        assert_eq!(init_value["snapshot"]["full_reset"], true);
        assert!(init_value["snapshot"]["candles_delta"]
            .as_array()
            .unwrap()
            .is_empty());

        let step_response = process_message(br#"{"type":"step","steps":1}"#);
        let step_value: Value = serde_json::from_slice(&step_response).unwrap();
        assert_eq!(step_value["ok"], true);
        assert_eq!(
            step_value["snapshot"]["candles_delta"].as_array().unwrap().len(),
            1
        );
        assert!(step_value["snapshot"]["candles"].is_null());
        assert_eq!(step_value["snapshot"]["full_reset"], false);
        assert_eq!(step_value["snapshot"]["replay"]["now_ms"], 60_000);
    }

    #[test]
    fn replay_ticks_send_only_the_current_candle() {
        let init = json!({
            "type": "init",
            "symbol": "BTC/USDT",
            "timeframe": "1m",
            "start_ms": 0,
            "step_ms": 60_000,
            "market_kind": "linear",
            "candles": [
                [60_000, 100.0, 110.0, 90.0, 105.0, 12.0],
                [120_000, 105.0, 106.0, 99.0, 101.0, 8.0]
            ]
        });
        process_message(&serde_json::to_vec(&init).unwrap());

        let tick_response = process_message(br#"{"type":"tick","steps":1}"#);
        let tick_value: Value = serde_json::from_slice(&tick_response).unwrap();
        assert_eq!(tick_value["ok"], true);
        assert!(tick_value["snapshot"]["candles"].is_null());
        assert_eq!(tick_value["snapshot"]["full_reset"], false);
        assert_eq!(tick_value["snapshot"]["candles_delta"].as_array().unwrap().len(), 1);
        assert_eq!(
            tick_value["snapshot"]["candles_delta"][0][0].as_f64(),
            Some(60_000.0)
        );
        assert_eq!(tick_value["snapshot"]["replay"]["fed"], 1);
    }

    #[test]
    fn replay_batches_preserve_every_intermediate_candle() {
        let init = json!({
            "type": "init",
            "symbol": "BTC/USDT",
            "timeframe": "1m",
            "start_ms": 0,
            "step_ms": 60_000,
            "market_kind": "linear",
            "candles": [
                [60_000, 100.0, 110.0, 90.0, 105.0, 12.0],
                [120_000, 105.0, 106.0, 99.0, 101.0, 8.0],
                [180_000, 101.0, 104.0, 98.0, 102.0, 9.0]
            ]
        });
        process_message(&serde_json::to_vec(&init).unwrap());

        let tick_response = process_message(br#"{"type":"tick","steps":3}"#);
        let tick_value: Value = serde_json::from_slice(&tick_response).unwrap();
        let candles = tick_value["snapshot"]["candles_delta"].as_array().unwrap();
        assert_eq!(candles.len(), 3);
        assert_eq!(candles[0][0].as_f64(), Some(60_000.0));
        assert_eq!(candles[2][0].as_f64(), Some(180_000.0));
        assert_eq!(tick_value["snapshot"]["replay"]["fed"], 3);
    }

    #[test]
    fn browser_engine_arms_stop_protection_and_records_its_exit_reason() {
        let init = json!({
            "type": "init",
            "symbol": "BTC/USDT",
            "timeframe": "1m",
            "start_ms": 0,
            "step_ms": 60_000,
            "market_kind": "linear",
            "engine_id": "test",
            "candles": [
                [60_000, 100.0, 110.0, 99.0, 105.0, 12.0],
                [120_000, 105.0, 106.0, 90.0, 95.0, 8.0]
            ]
        });
        let init_response = process_message(&serde_json::to_vec(&init).unwrap());
        let init_value: Value = serde_json::from_slice(&init_response).unwrap();
        assert_eq!(init_value["ok"], true);
        process_message(br#"{"type":"step","steps":1}"#);

        let order = json!({
            "type": "place_order",
            "symbol": "BTC/USDT",
            "side": "buy",
            "order_type": "market",
            "quantity": 1.0,
            "stop_loss_price": 95.0
        });
        let order_response = process_message(&serde_json::to_vec(&order).unwrap());
        let order_value: Value = serde_json::from_slice(&order_response).unwrap();
        assert_eq!(order_value["ok"], true);
        assert_eq!(
            order_value["snapshot"]["account"]["positions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let exit_response = process_message(br#"{"type":"step","steps":1}"#);
        let exit_value: Value = serde_json::from_slice(&exit_response).unwrap();
        let fills = exit_value["snapshot"]["fills"].as_array().unwrap();
        assert_eq!(fills.len(), 1);
        assert!(exit_value["snapshot"]["account"]["positions"]
            .as_array()
            .unwrap()
            .is_empty());
        let exit_order_id = fills[0]["orderId"].as_str().unwrap();
        let exit_time_ms = fills[0]["timestamp"].as_i64().unwrap();
        let exit_key = format!("test|BTC/USDT|{exit_order_id}|{exit_time_ms}");
        assert_eq!(exit_value["snapshot"]["exit_reasons"][exit_key], "stop");
    }

    #[test]
    fn risk_budget_sizes_at_the_actual_entry_not_the_stale_preview() {
        let init = json!({
            "type": "init",
            "symbol": "BTC/USDT",
            "timeframe": "1m",
            "start_ms": 0,
            "step_ms": 60_000,
            "market_kind": "linear",
            "candles": [
                [60_000, 100.0, 101.0, 99.0, 100.0, 10.0],
                [120_000, 105.0, 106.0, 104.0, 105.0, 10.0],
                [180_000, 105.0, 106.0, 90.0, 95.0, 10.0]
            ]
        });
        process_message(&serde_json::to_vec(&init).unwrap());
        // The panel previewed at 100 with a 10-point stop (quantity 1.0).
        // The replay advances to 105 before the command reaches the engine.
        process_message(br#"{"type":"step","steps":2}"#);

        let order = json!({
            "type": "place_order",
            "symbol": "BTC/USDT",
            "side": "buy",
            "order_type": "market",
            "quantity": 1.0,
            "stop_loss_price": 90.0,
            "risk_usdt": 10.0
        });
        let order_response = process_message(&serde_json::to_vec(&order).unwrap());
        let order_value: Value = serde_json::from_slice(&order_response).unwrap();
        assert_eq!(order_value["ok"], true);
        let positions = order_value["snapshot"]["account"]["positions"]
            .as_array()
            .unwrap();
        assert_eq!(positions.len(), 1);
        let qty = positions[0]["qty"].as_f64().unwrap();
        // 10 / 15 with no commission, not the stale preview quantity.
        let expected = 10.0 / 15.0;
        assert!((qty - expected).abs() < 1e-9, "qty={qty} expected={expected}");

        let exit_response = process_message(br#"{"type":"step","steps":1}"#);
        let exit_value: Value = serde_json::from_slice(&exit_response).unwrap();
        let account = &exit_value["snapshot"]["account"];
        let realized = account["pnl"]["realized"].as_f64().unwrap();
        let fees = account["pnl"]["fees"].as_f64().unwrap();
        assert!(
            (realized - fees + 10.0).abs() < 1e-6,
            "stop-out cost {} (realized={realized}, fees={fees})",
            realized - fees
        );
        let fill = &exit_value["snapshot"]["fills"][0];
        assert_eq!(fill["price"].as_f64(), Some(90.0));
    }

    #[test]
    fn switching_coins_stops_replaying_the_previous_coin() {
        let init = json!({
            "type": "init",
            "symbol": "BTC/USDT",
            "timeframe": "1m",
            "start_ms": 0,
            "step_ms": 60_000,
            "market_kind": "linear",
            "candles": [
                [60_000, 100.0, 100.0, 100.0, 100.0, 10.0],
                // A crash after the switch: if BTC kept replaying in the
                // background this bar would hit the 90 stop.
                [120_000, 100.0, 100.0, 50.0, 50.0, 10.0]
            ]
        });
        process_message(&serde_json::to_vec(&init).unwrap());
        process_message(br#"{"type":"step","steps":1}"#);
        let order = json!({
            "type": "place_order",
            "symbol": "BTC/USDT",
            "side": "buy",
            "order_type": "market",
            "quantity": 1.0,
            "stop_loss_price": 90.0
        });
        process_message(&serde_json::to_vec(&order).unwrap());

        let switch = json!({
            "type": "switch_series",
            "symbol": "ETH/USDT",
            "exchange": "binance",
            "timeframe": "1m",
            "step_ms": 60_000,
            "speed": 1.0,
            "market_kind": "linear",
            "reveal_reset_ms": 60_000,
            "range_end_ms": 600_000,
            "oi": [],
            "oi_period": "",
            "preserve_clock": true,
            "candles": [[120_000, 2_000.0, 2_000.0, 2_000.0, 2_000.0, 4.0]]
        });
        let switch_value: Value =
            serde_json::from_slice(&process_message(&serde_json::to_vec(&switch).unwrap())).unwrap();
        assert_eq!(switch_value["ok"], true);

        // Only the chart on screen may advance. The old BTC series must be
        // gone from the replay feed, so its stop does not fire off-screen.
        let tick_value: Value =
            serde_json::from_slice(&process_message(br#"{"type":"step","steps":1}"#)).unwrap();
        let snapshot = &tick_value["snapshot"];
        assert_eq!(snapshot["symbol"], "ETH/USDT");
        assert_eq!(snapshot["fills"].as_array().unwrap().len(), 0);
        let positions = snapshot["account"]["positions"].as_array().unwrap();
        assert_eq!(
            positions.len(),
            1,
            "the off-screen BTC position must survive: {positions:?}"
        );
        assert_eq!(positions[0]["symbol"], "BTC/USDT");
        assert_eq!(positions[0]["markPrice"].as_f64(), Some(100.0));
    }

    #[test]
    fn switching_coins_resets_to_the_new_reveal_boundary_and_pauses() {
        let init = json!({
            "type": "init",
            "symbol": "BTC/USDT",
            "timeframe": "1m",
            "start_ms": 60_000,
            "step_ms": 60_000,
            "market_kind": "linear",
            "candles": [
                [60_000, 100.0, 100.0, 100.0, 100.0, 10.0],
                [120_000, 100.0, 100.0, 100.0, 100.0, 10.0],
                [180_000, 100.0, 100.0, 100.0, 100.0, 10.0],
                [240_000, 100.0, 100.0, 100.0, 100.0, 10.0]
            ]
        });
        process_message(&serde_json::to_vec(&init).unwrap());
        // Replay BTC forward: the new coin must NOT inherit this clock.
        let stepped: Value =
            serde_json::from_slice(&process_message(br#"{"type":"step","steps":3}"#)).unwrap();
        assert_eq!(stepped["snapshot"]["replay"]["now_ms"], 240_000);

        let switch = json!({
            "type": "switch_series",
            "symbol": "ETH/USDT",
            "exchange": "binance",
            "timeframe": "1m",
            "step_ms": 60_000,
            "speed": 1.0,
            "market_kind": "linear",
            "reveal_reset_ms": 180_000,
            "range_end_ms": 600_000,
            "oi": [],
            "oi_period": "",
            "preserve_clock": false,
            "candles": [
                [120_000, 200.0, 200.0, 100.0, 200.0, 5.0],
                [180_000, 200.0, 200.0, 200.0, 200.0, 5.0],
                [240_000, 900.0, 900.0, 900.0, 900.0, 5.0]
            ]
        });
        let switch_value: Value =
            serde_json::from_slice(&process_message(&serde_json::to_vec(&switch).unwrap())).unwrap();
        assert_eq!(switch_value["ok"], true);
        let replay = &switch_value["snapshot"]["replay"];
        assert_eq!(
            replay["now_ms"], 180_000,
            "a new coin starts at its own reveal boundary, not the old clock"
        );
        assert_eq!(replay["paused"], true, "a new coin opens paused");

        // The price must come from the boundary bar (200), never from the bar
        // after it (900): inheriting the previous clock priced the new coin at
        // a candle the chart had not revealed yet.
        let order = json!({
            "type": "place_order",
            "symbol": "ETH/USDT",
            "side": "buy",
            "order_type": "market",
            "quantity": 1.0,
            "stop_loss_price": 150.0
        });
        let order_value: Value =
            serde_json::from_slice(&process_message(&serde_json::to_vec(&order).unwrap())).unwrap();
        assert_eq!(
            order_value["snapshot"]["fills"][0]["price"].as_f64(),
            Some(200.0)
        );

        // Stepping must feed ONLY the bar after the new boundary. If the reset
        // dropped the replay marks, the pre-revealed bar at 120k (low 100)
        // would be fed again and stop this position out at an old price.
        let tick_value: Value =
            serde_json::from_slice(&process_message(br#"{"type":"step","steps":1}"#)).unwrap();
        let snapshot = &tick_value["snapshot"];
        assert_eq!(snapshot["replay"]["now_ms"], 240_000);
        assert_eq!(
            snapshot["fills"].as_array().unwrap().len(),
            0,
            "pre-revealed history must never be re-fed after a coin switch"
        );
        assert_eq!(snapshot["account"]["positions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn switching_series_keeps_the_account_and_replay_clock() {
        let init = json!({
            "type": "init",
            "symbol": "BTC/USDT",
            "timeframe": "1m",
            "start_ms": 0,
            "step_ms": 60_000,
            "market_kind": "linear",
            "candles": [[60_000, 100.0, 110.0, 99.0, 105.0, 12.0]]
        });
        process_message(&serde_json::to_vec(&init).unwrap());
        process_message(br#"{"type":"step","steps":1}"#);
        process_message(
            br#"{"type":"place_order","symbol":"BTC/USDT","side":"buy",
                "order_type":"market","quantity":1.0}"#,
        );

        let switch = json!({
            "type": "switch_series",
            "symbol": "ETH/USDT",
            "exchange": "binance",
            "timeframe": "5m",
            "step_ms": 300_000,
            "speed": 1.0,
            "market_kind": "linear",
            "reveal_reset_ms": 0,
            "range_end_ms": 600_000,
            "oi": [],
            "oi_period": "",
            "preserve_clock": true,
            "candles": [[300_000, 50.0, 55.0, 49.0, 52.0, 4.0]]
        });
        let switch_response = process_message(&serde_json::to_vec(&switch).unwrap());
        let switch_value: Value = serde_json::from_slice(&switch_response).unwrap();
        assert_eq!(switch_value["ok"], true);
        assert_eq!(switch_value["snapshot"]["symbol"], "ETH/USDT");
        assert_eq!(switch_value["snapshot"]["replay"]["now_ms"], 60_000);
        assert_eq!(
            switch_value["snapshot"]["account"]["positions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    /// A stop/limit entry is executed by a NEW market order, so the fill's
    /// `orderId` is not the id the user clicked. The chart pairs trades by the
    /// fill's id, so the plan must be keyed that way too — otherwise the stop
    /// and target are looked up under a key that never exists and the box picks
    /// up whatever stale plan shares the number.
    #[test]
    fn entry_protections_are_keyed_by_the_fill_that_opened_the_trade() {
        let init = json!({
            "type": "init",
            "symbol": "BTC/USDT",
            "timeframe": "1m",
            "start_ms": 0,
            "step_ms": 60_000,
            "market_kind": "linear",
            "engine_id": "test",
            "candles": [
                [60_000, 100.0, 101.0, 99.0, 100.0, 10.0],
                // Crosses the 108 trigger.
                [120_000, 100.0, 112.0, 99.0, 111.0, 10.0],
                [180_000, 111.0, 125.0, 110.0, 120.0, 10.0]
            ]
        });
        process_message(&serde_json::to_vec(&init).unwrap());
        process_message(br#"{"type":"step","steps":1}"#);

        let order = json!({
            "type": "place_order",
            "symbol": "BTC/USDT",
            "side": "buy",
            "order_type": "stop",
            "quantity": 1.0,
            "stop_price": 108.0,
            "stop_loss_price": 95.0,
            "tp_price": 130.0
        });
        let placed: Value =
            serde_json::from_slice(&process_message(&serde_json::to_vec(&order).unwrap())).unwrap();
        assert_eq!(placed["ok"], true, "{placed}");
        // The trigger order that was armed is not the order that fills.
        let trigger_id = placed["snapshot"]["account"]["open"]
            .as_array()
            .and_then(|open| open.first())
            .and_then(|order| order["id"].as_str())
            .expect("the stop entry should rest as a conditional order")
            .to_string();

        let step: Value =
            serde_json::from_slice(&process_message(br#"{"type":"step","steps":1}"#)).unwrap();
        assert_eq!(step["ok"], true, "{step}");
        let fills = step["snapshot"]["fills"].as_array().unwrap();
        let entry = fills
            .iter()
            .find(|fill| fill["side"] == "buy")
            .expect("the triggered entry should fill");
        let order_id = entry["orderId"].as_str().unwrap();
        assert_ne!(order_id, trigger_id, "triggered entries fill on a new order");

        let key = format!(
            "test|{}|{}|{}",
            entry["symbol"].as_str().unwrap(),
            order_id,
            entry["timestamp"].as_i64().unwrap()
        );
        let plans = step["snapshot"]["entry_protections"].as_object().unwrap();
        let plan = plans
            .get(&key)
            .unwrap_or_else(|| panic!("no plan under {key}, got {:?}", plans.keys()));
        assert_eq!(plan["stop_price"].as_f64(), Some(95.0));
        assert_eq!(plan["take_price"].as_f64(), Some(130.0));
    }

    /// A manual close is not one of the armed protective orders, so nothing
    /// would ever retire them: the stop and target would stay in the book,
    /// keep drawing lines and could close the NEXT position.
    #[test]
    fn manual_close_cancels_the_armed_protections() {
        let init = json!({
            "type": "init",
            "symbol": "BTC/USDT",
            "timeframe": "1m",
            "start_ms": 0,
            "step_ms": 60_000,
            "market_kind": "linear",
            "candles": [
                [60_000, 100.0, 101.0, 99.0, 100.0, 10.0],
                [120_000, 100.0, 102.0, 99.0, 101.0, 10.0]
            ]
        });
        process_message(&serde_json::to_vec(&init).unwrap());
        process_message(br#"{"type":"step","steps":1}"#);

        let order = json!({
            "type": "place_order",
            "symbol": "BTC/USDT",
            "side": "buy",
            "order_type": "market",
            "quantity": 1.0,
            "stop_loss_price": 95.0,
            "tp_price": 130.0
        });
        let placed: Value =
            serde_json::from_slice(&process_message(&serde_json::to_vec(&order).unwrap())).unwrap();
        assert_eq!(placed["ok"], true, "{placed}");
        let armed = placed["snapshot"]["account"]["open"].as_array().unwrap();
        assert_eq!(armed.len(), 2, "stop and target should be armed: {armed:?}");

        let closed: Value =
            serde_json::from_slice(&process_message(br#"{"type":"close_position"}"#)).unwrap();
        assert_eq!(closed["ok"], true, "{closed}");
        assert!(closed["snapshot"]["account"]["positions"]
            .as_array()
            .unwrap()
            .is_empty());
        let left = closed["snapshot"]["account"]["open"].as_array().unwrap();
        assert!(
            left.iter().all(|order| order["triggerPrice"].is_null()),
            "protective orders survived the close: {left:?}"
        );
    }

    /// The take-profit must be armed by the FILL, not by the entry order.
    ///
    /// Attaching `tp_price` to a resting stop entry made the broker arm a
    /// take-profit immediately — before the entry ever filled — while
    /// `pending_protections` armed a second one from the fill. The untracked
    /// copy could fire while flat, survive a manual close, or be left behind
    /// when the entry was cancelled.
    #[test]
    fn trigger_entry_arms_protections_only_after_the_fill() {
        let init = json!({
            "type": "init",
            "symbol": "BTC/USDT",
            "timeframe": "1m",
            "start_ms": 0,
            "step_ms": 60_000,
            "market_kind": "linear",
            "candles": [
                [60_000, 100.0, 101.0, 99.0, 100.0, 10.0],
                // Crosses the 108 trigger; the low stays above the 95 stop.
                [120_000, 100.0, 112.0, 99.0, 111.0, 10.0]
            ]
        });
        process_message(&serde_json::to_vec(&init).unwrap());
        process_message(br#"{"type":"step","steps":1}"#);

        let order = json!({
            "type": "place_order",
            "symbol": "BTC/USDT",
            "side": "buy",
            "order_type": "stop",
            "quantity": 1.0,
            "stop_price": 108.0,
            "stop_loss_price": 95.0,
            "tp_price": 130.0
        });
        let placed: Value =
            serde_json::from_slice(&process_message(&serde_json::to_vec(&order).unwrap())).unwrap();
        assert_eq!(placed["ok"], true, "{placed}");
        let resting = placed["snapshot"]["account"]["open"].as_array().unwrap();
        assert_eq!(
            resting.len(),
            1,
            "before the fill only the entry trigger may rest: {resting:?}"
        );
        assert_eq!(resting[0]["type"], "stop");
        assert!(
            resting.iter().all(|o| o["type"] != "take_profit"),
            "a take-profit was armed before the entry filled: {resting:?}"
        );

        let step: Value =
            serde_json::from_slice(&process_message(br#"{"type":"step","steps":1}"#)).unwrap();
        assert_eq!(step["ok"], true, "{step}");
        let open = step["snapshot"]["account"]["open"].as_array().unwrap();
        let stops = open.iter().filter(|o| o["type"] == "stop").count();
        let takes = open.iter().filter(|o| o["type"] == "take_profit").count();
        assert_eq!(stops, 1, "expected exactly one protective stop: {open:?}");
        assert_eq!(takes, 1, "expected exactly one take-profit: {open:?}");
        assert_eq!(open.len(), 2, "unexpected leftover orders: {open:?}");
    }

    /// Closing a trigger-entry trade by hand must retire the stop and target
    /// that were armed from its fill, leaving nothing open to fire later.
    #[test]
    fn trigger_entry_manual_close_leaves_no_orders() {
        let init = json!({
            "type": "init",
            "symbol": "BTC/USDT",
            "timeframe": "1m",
            "start_ms": 0,
            "step_ms": 60_000,
            "market_kind": "linear",
            "candles": [
                [60_000, 100.0, 101.0, 99.0, 100.0, 10.0],
                [120_000, 100.0, 112.0, 99.0, 111.0, 10.0]
            ]
        });
        process_message(&serde_json::to_vec(&init).unwrap());
        process_message(br#"{"type":"step","steps":1}"#);

        let order = json!({
            "type": "place_order",
            "symbol": "BTC/USDT",
            "side": "buy",
            "order_type": "stop",
            "quantity": 1.0,
            "stop_price": 108.0,
            "stop_loss_price": 95.0,
            "tp_price": 130.0
        });
        let placed: Value =
            serde_json::from_slice(&process_message(&serde_json::to_vec(&order).unwrap())).unwrap();
        assert_eq!(placed["ok"], true, "{placed}");
        let step: Value =
            serde_json::from_slice(&process_message(br#"{"type":"step","steps":1}"#)).unwrap();
        assert_eq!(step["ok"], true, "{step}");
        assert!(!step["snapshot"]["account"]["positions"]
            .as_array()
            .unwrap()
            .is_empty(), "the trigger entry should have opened a position");

        let closed: Value =
            serde_json::from_slice(&process_message(br#"{"type":"close_position"}"#)).unwrap();
        assert_eq!(closed["ok"], true, "{closed}");
        assert!(closed["snapshot"]["account"]["positions"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(
            closed["snapshot"]["account"]["open"]
                .as_array()
                .unwrap()
                .is_empty(),
            "orders survived the manual close: {:?}",
            closed["snapshot"]["account"]["open"]
        );
    }
}
