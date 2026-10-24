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
    step_ms: u64,
    speed: f64,
    reveal_reset_ms: i64,
    range_end_ms: i64,
    candles: Vec<Vec<f64>>,
    oi: Vec<[f64; 2]>,
    oi_period: String,
    loaded_series: Vec<(String, String)>,
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
            maker: 0.001,
            taker: 0.0025,
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
            step_ms,
            speed,
            reveal_reset_ms,
            range_end_ms,
            total: rows.len(),
            candles: rows,
            oi,
            oi_period,
            loaded_series: vec![(symbol.to_string(), timeframe.to_string())],
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
                for (symbol, timeframe) in &engine.loaded_series {
                    if let Ok(symbol) = Symbol::parse(symbol) {
                        engine
                            .exchange
                            .mark_history_played(&symbol, timeframe, timestamp);
                    }
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
                engine.exchange.set_step_ms(step_ms);
                let current_time = Utc
                    .timestamp_millis_opt(now_ms)
                    .single()
                    .ok_or_else(|| "current replay time is out of range".to_string())?;
                engine
                    .exchange
                    .mark_symbol_history_played(&symbol_value, current_time);
                let rows_before_now = rows
                    .iter()
                    .filter(|row| {
                        row.first()
                            .is_some_and(|timestamp| *timestamp <= now_ms as f64)
                    })
                    .collect::<Vec<_>>();
                if let Some(last) = rows_before_now.last() {
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
                let same_window = command["preserve_clock"].as_bool().unwrap_or(false);
                let reveal_reset_ms = command["reveal_reset_ms"].as_i64().unwrap_or(now_ms);
                if !same_window {
                    let reset_time = Utc
                        .timestamp_millis_opt(reveal_reset_ms)
                        .single()
                        .ok_or_else(|| "reveal reset time is out of range".to_string())?;
                    engine.exchange.reset(reset_time);
                    for (old_symbol, old_timeframe) in &engine.loaded_series {
                        if let Ok(old_symbol) = Symbol::parse(old_symbol) {
                            engine.exchange.mark_history_played(
                                &old_symbol,
                                old_timeframe,
                                reset_time,
                            );
                        }
                    }
                    engine
                        .exchange
                        .mark_history_played(&symbol_value, timeframe, reset_time);
                }
                let series_key = (symbol.to_string(), timeframe.to_string());
                if !engine.loaded_series.contains(&series_key) {
                    engine.loaded_series.push(series_key);
                }
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
                if let Some(value) = command["tp_price"].as_f64() {
                    params.tp_price = Some(value);
                }
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
                let symbol = command["symbol"].as_str().unwrap_or(&engine.symbol);
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
                    .create_order(symbol, "market", side, amount, None, params)
                    .map_err(|error| error.to_string())?;
                process_fresh_fills(engine)?;
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
            engine
                .exit_reasons
                .insert(order_id.clone(), kind.to_string());
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
            let entry_order_id = pending.entry_order_id.clone();
            let entry_side = pending.entry_side.clone();
            let protect_side = if entry_side == "buy" { "sell" } else { "buy" };
            let plan = engine
                .entry_protections
                .entry(entry_order_id)
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
        assert_eq!(
            exit_value["snapshot"]["exit_reasons"][exit_order_id],
            "stop"
        );
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
        // 10 / (15 + (105 + 90) * 0.0025), not the stale preview quantity.
        let expected = 10.0 / (15.0 + 195.0 * 0.0025);
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
}
