//! Trader-level diary scenarios.
//!
//! `diary.rs` unit-tests the FIFO reconstruction from hand-written fills, which
//! says nothing about the payload the trader actually opens at
//! `?trader=<ник>`. These tests drive the engine through the very same client
//! messages the browser sends (subscribe -> login -> challenge -> orders ->
//! replay), then read `/api/trader/<ник>` and `/api/traders` payloads through
//! the very handlers the HTTP layer calls.
//!
//! The invariants asserted here are the trader's: what I did is what the diary
//! shows. Every run is offline and deterministic (synthetic bars, no cache),
//! and no test hardcodes a price — the synthetic series is walked instead.


use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot};

use super::*;

const SYM: &str = "BTC/USDT";
const NICK: &str = "Бык-Бык-7";
const RISK: f64 = 10.0;
const CAPITAL: f64 = 1000.0;
const DAY_MS: i64 = 86_400_000;

/// No cache here: every series is deterministic synthetic data, so these tests

fn engine() -> (Engine, broadcast::Receiver<String>) {
    let (tx, rx) = broadcast::channel::<String>(1024);
    (Engine::new(tx), rx)
}

/// Deliver one client message, parsed from its JSON form — the same bytes the
/// browser puts on the wire.
fn send(eng: &mut Engine, client_id: u64, msg: Value) {
    let cmd: ClientCmd = serde_json::from_value(msg).expect("client message parses");
    eng.handle_command(client_id, cmd);
}

/// A trader opening their terminal: pick a series, step a couple of bars so
/// there is a price, take a nick and join the challenge with a risk.
fn open_trader(eng: &mut Engine, client_id: u64, timeframe: &str, nick: &str, risk: f64) {
    send(
        eng,
        client_id,
        json!({"type": "subscribe", "symbol": SYM, "timeframe": timeframe, "exchange": "binance"}),
    );
    // Market data is the browser's job now, so the session starts without a
    // series: seed one (as the browser would) and step a couple of bars so the
    // trader has a live price to trade.
    let step: i64 = if timeframe == "1d" { 86_400_000 } else { 300_000 };
    // 400 bars of history, with the reveal boundary here and plenty of bars
    // still AHEAD of the clock: these scenarios walk the replay forward.
    eng.seed_series(client_id, SYM, timeframe, 4_000_000, 400, 4_000_000, 4_000_000, 4_000_000 + 400 * step);
    send(eng, client_id, json!({"type": "replay", "action": "step", "steps": 2}));
    send(eng, client_id, json!({"type": "login", "name": nick}));
    send(
        eng,
        client_id,
        json!({
            "type": "challenge", "join": true,
            "risk_usdt": risk, "capital_usdt": CAPITAL
        }),
    );
}

/// A market order, optionally protected by a stop loss (the OrderPanel's
/// "закрыть по стопу" checkbox).
fn market(eng: &mut Engine, client_id: u64, side: &str, qty: f64, stop_loss: Option<f64>) {
    send(
        eng,
        client_id,
        json!({
            "type": "place_order", "symbol": SYM, "side": side,
            "order_type": "market", "quantity": qty, "price": null, "stop_price": null,
            "stop_loss_price": stop_loss, "time_in_force": "GTC",
            "post_only": false, "reduce_only": false
        }),
    );
}

/// Advance the replay by `steps` bars of `step_ms` each: a coarse jump still
/// feeds every bar in between, which is how the trader's bar-scrubber moves.
fn jump(eng: &mut Engine, client_id: u64, step_ms: i64, steps: u64) {
    send(
        eng,
        client_id,
        json!({"type": "replay", "action": "step", "steps": steps, "step_ms": step_ms}),
    );
}

/// The loaded series as plain rows: [timestamp, open, high, low, close, volume].
fn series(eng: &Engine, client_id: u64, timeframe: &str) -> Vec<Vec<f64>> {
    eng.sessions[&client_id].ex.fetch_ohlcv(SYM, timeframe, None, None)
}

/// Index of the bar the replay clock stands on.
fn bar_index(eng: &Engine, client_id: u64, timeframe: &str) -> i64 {
    let rows = series(eng, client_id, timeframe);
    let now = eng.sessions[&client_id].ex.now_ms();
    ((now - rows[0][0] as i64) / 300_000).max(0)
}

/// Walk exactly `bars` bars of 5m forward (bar-accurate, unlike a coarse jump).
fn jump_bars(eng: &mut Engine, client_id: u64, bars: i64) {
    jump(eng, client_id, 300_000, bars.max(1) as u64);
}

/// Fees actually paid on `side` fills — the diary charges them against the PnL,
/// so the scenarios reconcile against the real fills instead of a hardcoded rate.
fn paid_fees(eng: &Engine, client_id: u64, side: &str) -> f64 {
    eng.sessions[&client_id]
        .ex
        .fetch_fills(None, None)
        .iter()
        .filter(|fill| fill["side"].as_str() == Some(side))
        .map(|fill| num(fill, "fee"))
        .sum()
}

/// `GET /api/trader/<nick>` — exactly what the diary page fetches.
fn diary(eng: &Engine, nick: &str) -> Value {
    let (tx, rx) = oneshot::channel();
    eng.handle_diary(nick, tx);
    rx.blocking_recv().expect("diary handler replies")
}

/// `GET /api/traders` — the participants board.
fn traders(eng: &Engine) -> Value {
    let (tx, rx) = oneshot::channel();
    eng.handle_traders(tx);
    rx.blocking_recv().expect("traders handler replies")
}

/// The trader hangs up (browser tab closed).
fn disconnect(eng: &mut Engine, client_id: u64) {
    eng.handle_job(EngineJob::Disconnect { client_id });
}

fn array<'a>(value: &'a Value, key: &str) -> &'a Vec<Value> {
    value[key]
        .as_array()
        .unwrap_or_else(|| panic!("`{key}` must be an array, got {}", value[key]))
}

fn num(value: &Value, key: &str) -> f64 {
    value[key].as_f64().unwrap_or_else(|| panic!("`{key}` must be a number, got {}", value[key]))
}

fn int(value: &Value, key: &str) -> i64 {
    value[key]
        .as_i64()
        .unwrap_or_else(|| panic!("`{key}` must be an integer, got {}", value[key]))
}

fn sum_by(rows: &[Value], key: &str) -> f64 {
    rows.iter().map(|row| num(row, key)).sum()
}

fn last_price(eng: &Engine, client_id: u64) -> f64 {
    eng.sessions[&client_id]
        .ex
        .last_price_for(SYM)
        .unwrap_or_else(|| panic!("session #{client_id} has no price for {SYM}"))
}

/// Reconciliation a trader can do by hand: every unit they bought is either
/// still open or already closed by a trade in the diary — nothing invented,
/// nothing lost. Long-only sessions (the flip case has its own test).
fn fills_balance_the_diary(eng: &Engine, client_id: u64, d: &Value, long_only: bool) {
    let fills = eng.sessions[&client_id].ex.fetch_fills(None, None);
    let mut bought = 0.0;
    let mut sold = 0.0;
    for fill in &fills {
        match fill["side"].as_str() {
            Some("buy") => bought += num(fill, "amount"),
            Some("sell") => sold += num(fill, "amount"),
            other => panic!("unexpected fill side {other:?}"),
        }
    }
    let closed = sum_by(array(d, "trades"), "qty");
    let open = sum_by(array(d, "positions"), "qty");
    if long_only {
        assert!(
            (bought - sold - open).abs() < 1e-9,
            "net filled {bought} - {sold} must equal the open {open}: {d}"
        );
        assert!(
            (sold - closed).abs() < 1e-9,
            "every sold unit must be a closed trade: sold {sold}, closed {closed}: {d}"
        );
    } else {
        // Signed: a flip leaves the open position on the other side.
        let net = bought - sold;
        let signed_open: f64 = array(d, "positions")
            .iter()
            .map(|p| match p["side"].as_str() {
                Some("short") => -num(p, "qty"),
                _ => num(p, "qty"),
            })
            .sum();
        assert!(
            (net - signed_open).abs() < 1e-9,
            "net filled {net} must equal the signed open {signed_open}: {d}"
        );
    }
}

// ---------------------------------------------------------------------------
// The trader's trades
// ---------------------------------------------------------------------------

#[test]
fn trader_round_trip_appears_in_diary() {
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", NICK, RISK);

    market(&mut eng, 1, "buy", 0.5, None);
    jump(&mut eng, 1, 3_600_000, 2);
    market(&mut eng, 1, "sell", 0.5, None);

    let d = diary(&eng, NICK);
    assert_eq!(d["nick"], NICK);
    let trades = array(&d, "trades");
    assert_eq!(trades.len(), 1, "one round trip is one trade: {d}");

    let t = &trades[0];
    assert_eq!(t["symbol"], SYM);
    assert_eq!(t["side"], "long");
    assert_eq!(t["direction"], "buy→sell");
    assert!((num(t, "qty") - 0.5).abs() < 1e-9, "half a BTC in, half out");

    let (entry, exit, fee, pnl) =
        (num(t, "entry_price"), num(t, "exit_price"), num(t, "fee"), num(t, "pnl"));
    let expected = (exit - entry) * 0.5 - fee;
    assert!(
        (pnl - expected).abs() < 1e-9,
        "pnl must be (exit-entry)*qty minus both fees: expected {expected}, got {pnl}"
    );
    assert!(
        (num(t, "entry_fee") + num(t, "exit_fee") - fee).abs() < 1e-12,
        "the total fee is the sum of both legs"
    );
    assert!(
        (num(t, "r") - pnl / RISK).abs() < 1e-9,
        "R is the PnL over the risk the trader joined with"
    );

    assert!(
        int(t, "entry_time_ms") <= int(t, "exit_time_ms"),
        "a trade cannot exit before it entered"
    );
    assert!(!t["entry_order_id"].as_str().unwrap_or_default().is_empty());
    assert!(!t["exit_order_id"].as_str().unwrap_or_default().is_empty());

    assert!(array(&d, "positions").is_empty(), "flat now, so no open position: {d}");
    assert_eq!(num(&d["stats"], "total_trades"), 1.0);
    fills_balance_the_diary(&eng, 1, &d, true);
}

#[test]
fn diary_numbers_reconcile_with_the_account() {
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", NICK, RISK);
    market(&mut eng, 1, "buy", 0.25, None);
    jump(&mut eng, 1, 3_600_000, 3);
    market(&mut eng, 1, "sell", 0.25, None);

    let d = diary(&eng, NICK);
    let stats = &d["stats"];
    let trades = array(&d, "trades");

    assert!(
        (num(stats, "total_pnl") - sum_by(trades, "pnl")).abs() < 1e-9,
        "total_pnl must be the sum of the trades: {stats}"
    );
    assert!(
        (num(stats, "fees") - sum_by(trades, "fee")).abs() < 1e-12,
        "the fee total must be the sum of the trades: {stats}"
    );

    // The account books realized PnL gross; the diary charges the fees against
    // it, so the two must differ by exactly the fees paid.
    let account = eng.sessions[&1].ex.account_snapshot();
    let realized = num(&account["pnl"], "realized");
    assert!(
        (num(stats, "realized_pnl") - realized).abs() < 1e-9,
        "the diary's realized must be the account's realized: {stats} vs {account}"
    );
    assert!(
        (num(stats, "realized_pnl") - num(stats, "fees") - num(stats, "total_pnl")).abs() < 1e-9,
        "flat account: realized minus fees must equal the trades' PnL: {stats}"
    );

    assert!(
        (num(&d, "equity") - num(&account, "equity")).abs() < 1e-9,
        "equity must come from the account snapshot"
    );
    assert!(num(&d, "equity") > 0.0);
}

#[test]
fn open_position_is_not_a_trade() {
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", NICK, RISK);
    market(&mut eng, 1, "buy", 0.4, None);
    jump(&mut eng, 1, 3_600_000, 2);

    let d = diary(&eng, NICK);
    assert!(array(&d, "trades").is_empty(), "nothing closed yet: {d}");
    assert_eq!(num(&d["stats"], "total_trades"), 0.0);
    assert_eq!(num(&d["stats"], "total_pnl"), 0.0);
    assert_eq!(num(&d["stats"], "win_rate"), 0.0);
    assert_eq!(num(&d["stats"], "avg_r"), 0.0);

    let positions = array(&d, "positions");
    assert_eq!(positions.len(), 1, "the open long is a position, not a trade: {d}");
    let p = &positions[0];
    assert_eq!(p["symbol"], SYM);
    assert_eq!(p["side"], "long");
    assert!((num(p, "qty") - 0.4).abs() < 1e-9);

    let (entry, mark) = (num(p, "entry_price"), num(p, "mark_price"));
    assert!(mark > 0.0, "the diary marks the open position: {p}");
    // The diary charges the entry fee against the open PnL, exactly like it
    // does for a closed trade.
    let entry_fee = paid_fees(&eng, 1, "buy");
    assert!(
        (num(p, "unrealized") - ((mark - entry) * num(p, "qty") - entry_fee)).abs() < 1e-6,
        "unrealized of a long is (mark - entry) * qty minus the entry fee: {p}"
    );
    assert!(int(p, "entry_time_ms") > 0, "the chart needs the entry time: {p}");
    fills_balance_the_diary(&eng, 1, &d, true);
}

#[test]
fn partial_close_splits_trade_and_position() {
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", NICK, RISK);
    market(&mut eng, 1, "buy", 1.0, None);
    jump(&mut eng, 1, 3_600_000, 2);
    market(&mut eng, 1, "sell", 0.4, None);

    let d = diary(&eng, NICK);
    let trades = array(&d, "trades");
    let positions = array(&d, "positions");
    assert_eq!(trades.len(), 1, "the 0.4 closed: {d}");
    assert_eq!(positions.len(), 1, "the 0.6 stays open: {d}");
    assert!((num(&trades[0], "qty") - 0.4).abs() < 1e-9);
    assert!((num(&positions[0], "qty") - 0.6).abs() < 1e-9);

    // The leftover lot keeps the original entry: same price, same time.
    assert_eq!(positions[0]["entry_price"], trades[0]["entry_price"]);
    assert_eq!(positions[0]["entry_time_ms"], trades[0]["entry_time_ms"]);
    fills_balance_the_diary(&eng, 1, &d, true);
}

#[test]
fn flip_closes_the_long_and_opens_a_short() {
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", NICK, RISK);
    market(&mut eng, 1, "buy", 1.0, None);
    jump(&mut eng, 1, 3_600_000, 2);
    market(&mut eng, 1, "sell", 1.5, None);

    let d = diary(&eng, NICK);
    let trades = array(&d, "trades");
    let positions = array(&d, "positions");
    assert_eq!(trades.len(), 1, "the long round trip closed: {d}");
    assert_eq!(trades[0]["side"], "long");
    assert!((num(&trades[0], "qty") - 1.0).abs() < 1e-9, "exactly 1.0 was closed");

    assert_eq!(positions.len(), 1, "the oversold 0.5 is a new short: {d}");
    assert_eq!(positions[0]["side"], "short");
    assert!((num(&positions[0], "qty") - 0.5).abs() < 1e-9);
    let (entry, mark) = (num(&positions[0], "entry_price"), num(&positions[0], "mark_price"));
    // The 1.5 sell paid the fee on all of it; 0.5 of that fill stayed open.
    let (sold, left_open) = (1.5, 0.5);
    let open_lot_fee = paid_fees(&eng, 1, "sell") * (left_open / sold);
    assert!(
        (num(&positions[0], "unrealized") - ((entry - mark) * left_open - open_lot_fee)).abs() < 1e-6,
        "unrealized of a short is (entry - mark) * qty minus its share of the entry fee: {d}"
    );
    fills_balance_the_diary(&eng, 1, &d, false);
}

#[test]
fn stats_math_is_self_consistent() {
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", NICK, RISK);

    // Trade 1 exits at the highest close still ahead in the window: the series
    // is scanned instead of hardcoding a price.
    let rows = series(&eng, 1, "5m");
    let start = bar_index(&eng, 1, "5m") as usize;
    let (peak_i, peak) = rows[start + 1..]
        .iter()
        .enumerate()
        .map(|(offset, row)| (start + 1 + offset, row[4]))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .expect("bars ahead of the clock");

    market(&mut eng, 1, "buy", 0.2, None);
    let entry = last_price(&eng, 1);
    assert!(
        peak > entry * 1.02,
        "the synthetic window must contain a move worth trading: peak {peak} vs entry {entry}"
    );
    let distance = peak_i as i64 - bar_index(&eng, 1, "5m");
    jump_bars(&mut eng, 1, distance);
    market(&mut eng, 1, "sell", 0.2, None); // winner

    // Trade 2 buys on a bar whose NEXT bar closes lower, so the exit is a
    // guaranteed loser now that no commission is charged: the direction of the
    // price is the whole PnL.
    let rows = series(&eng, 1, "5m");
    let now = bar_index(&eng, 1, "5m") as usize;
    let down = (now + 1..rows.len().saturating_sub(1))
        .find(|i| rows[i + 1][4] < rows[*i][4])
        .expect("a down bar ahead");
    let distance = down as i64 - bar_index(&eng, 1, "5m");
    jump_bars(&mut eng, 1, distance);
    market(&mut eng, 1, "buy", 0.2, None);
    jump_bars(&mut eng, 1, 1);
    market(&mut eng, 1, "sell", 0.2, None);

    let d = diary(&eng, NICK);
    let trades = array(&d, "trades");
    let stats = &d["stats"];
    assert_eq!(trades.len(), 2, "two round trips: {d}");

    let pnls: Vec<f64> = trades.iter().map(|t| num(t, "pnl")).collect();
    let rs: Vec<f64> = trades.iter().map(|t| num(t, "r")).collect();
    let wins: Vec<f64> = pnls.iter().copied().filter(|p| *p > 0.0).collect();
    let losses: Vec<f64> = pnls.iter().copied().filter(|p| *p < 0.0).collect();
    assert!(
        wins.len() == 1 && losses.len() == 1,
        "the scenario must produce one winner and one loser, got {pnls:?}"
    );

    assert_eq!(num(stats, "total_trades"), 2.0);
    assert_eq!(num(stats, "wins"), 1.0);
    assert_eq!(num(stats, "losses"), 1.0);
    assert!(
        (num(stats, "win_rate") - 0.5).abs() < 1e-12,
        "one win out of two: {stats}"
    );
    assert!((num(stats, "total_pnl") - pnls.iter().sum::<f64>()).abs() < 1e-9);
    assert!((num(stats, "avg_r") - rs.iter().sum::<f64>() / 2.0).abs() < 1e-12);
    assert!((num(stats, "avg_win") - wins[0]).abs() < 1e-9);
    assert!((num(stats, "avg_loss") - losses[0]).abs() < 1e-9);
    assert!((num(stats, "best") - wins[0]).abs() < 1e-9);
    assert!((num(stats, "worst") - losses[0]).abs() < 1e-9);
    assert_eq!(num(stats, "fees"), 0.0, "the simulator charges no commission");
    assert!((num(stats, "fees") - sum_by(trades, "fee")).abs() < 1e-12);
    assert!(
        (num(stats, "realized_pnl") - num(stats, "fees") - num(stats, "total_pnl")).abs() < 1e-9,
        "flat account: realized minus fees equals the trades' PnL: {stats}"
    );

    // Sorted by exit time — the table's default order.
    let exits: Vec<i64> = trades.iter().map(|t| int(t, "exit_time_ms")).collect();
    let mut sorted = exits.clone();
    sorted.sort_unstable();
    assert_eq!(exits, sorted, "trades come back sorted by exit time: {exits:?}");
    fills_balance_the_diary(&eng, 1, &d, true);
}

#[test]
fn r_scales_with_the_joined_risk() {
    // Same script, two traders, different risk: identical fills, R divided by 5.
    let (mut eng, _rx) = engine();
    for (id, nick, risk) in [(1, "Узкий-Риск", 10.0), (2, "Широкий-Риск", 50.0)] {
        open_trader(&mut eng, id, "5m", nick, risk);
        market(&mut eng, id, "buy", 0.3, None);
        jump(&mut eng, id, 3_600_000, 2);
        market(&mut eng, id, "sell", 0.3, None);
    }

    let a = diary(&eng, "Узкий-Риск");
    let b = diary(&eng, "Широкий-Риск");
    assert!((num(&a, "risk_usdt") - 10.0).abs() < 1e-9);
    assert!((num(&b, "risk_usdt") - 50.0).abs() < 1e-9);

    let ta = &array(&a, "trades")[0];
    let tb = &array(&b, "trades")[0];
    assert!(
        (num(ta, "pnl") - num(tb, "pnl")).abs() < 1e-9,
        "the same fills must produce the same PnL: {ta} vs {tb}"
    );
    assert!(
        (num(ta, "r") - num(tb, "r") * 5.0).abs() < 1e-9,
        "five times the risk must give a fifth of the R: {} vs {}",
        num(ta, "r"),
        num(tb, "r")
    );
}

// ---------------------------------------------------------------------------
// What the trader sees on the page
// ---------------------------------------------------------------------------

#[test]
fn diary_is_empty_for_a_fresh_trader() {
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", NICK, RISK);

    let d = diary(&eng, NICK);
    assert!(array(&d, "trades").is_empty());
    assert!(array(&d, "positions").is_empty());
    let stats = &d["stats"];
    for (key, expected) in [
        ("total_trades", 0.0),
        ("win_rate", 0.0),
        ("wins", 0.0),
        ("losses", 0.0),
        ("realized_pnl", 0.0),
        ("total_pnl", 0.0),
        ("avg_r", 0.0),
        ("avg_win", 0.0),
        ("avg_loss", 0.0),
        ("best", 0.0),
        ("worst", 0.0),
        ("fees", 0.0),
    ] {
        assert!(
            num(stats, key).abs() < 1e-12,
            "{key} must be {expected} for a fresh trader, got {}: {stats}",
            num(stats, key)
        );
    }
    assert!((num(&d, "risk_usdt") - RISK).abs() < 1e-9);
    assert!((num(&d, "capital_usdt") - CAPITAL).abs() < 1e-9);
    assert_eq!(d["in_challenge"], json!(true));
    assert!((num(&d, "equity") - 1_000_000.0).abs() < 1e-6, "untouched paper account");
}

#[test]
fn diary_disappears_when_the_trader_goes_offline() {
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", NICK, RISK);
    market(&mut eng, 1, "buy", 0.1, None);
    assert_eq!(array(&diary(&eng, NICK), "trades").len(), 0);

    disconnect(&mut eng, 1);

    let d = diary(&eng, NICK);
    assert_eq!(d["type"], "diary");
    assert_eq!(d["error"], "trader not found", "diary is online-only: {d}");
    assert!(d.get("trades").is_none(), "no half-diary after disconnect: {d}");
    assert!(array(&traders(&eng), "traders").is_empty());
}

#[test]
fn diary_is_isolated_between_traders() {
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", "Первый", RISK);
    open_trader(&mut eng, 2, "5m", "Второй", RISK);

    market(&mut eng, 1, "buy", 1.0, None);
    jump(&mut eng, 1, 3_600_000, 2);
    market(&mut eng, 1, "sell", 1.0, None);
    market(&mut eng, 2, "buy", 0.05, None);

    let a = diary(&eng, "Первый");
    let b = diary(&eng, "Второй");

    assert_eq!(a["nick"], "Первый");
    assert_eq!(b["nick"], "Второй");
    assert_eq!(array(&a, "trades").len(), 1, "Первый closed his trade: {a}");
    assert!(array(&a, "positions").is_empty());
    assert!(array(&b, "trades").is_empty(), "Второй has nothing closed: {b}");
    assert_eq!(array(&b, "positions").len(), 1);
    assert!((num(&array(&b, "positions")[0], "qty") - 0.05).abs() < 1e-9);
    assert_ne!(a["equity"], b["equity"], "each has their own account");
}

#[test]
fn diary_finds_a_trader_with_a_cyrillic_nick() {
    let (mut eng, _rx) = engine();
    let nick = "Дневник-Тест";
    open_trader(&mut eng, 1, "5m", nick, RISK);
    market(&mut eng, 1, "buy", 0.2, None);

    let d = diary(&eng, nick);
    assert_eq!(d["nick"], nick);
    assert!(d.get("error").is_none(), "a Cyrillic nick must resolve: {d}");
    assert_eq!(array(&d, "positions").len(), 1);

    // Same nickname as the URL carries it, percent-encoded.
    let mut target = String::from("/api/trader/");
    for byte in nick.as_bytes() {
        target.push_str(&format!("%{byte:02X}"));
    }
    assert!(matches!(route_request("GET", &target), HttpRoute::Trader(ref n) if n == nick));
}

#[test]
fn diary_carries_the_context_the_chart_needs() {
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "1h", NICK, RISK);

    // What TradingTerminal pushes every few seconds: the trader's drawings and
    // enabled criteria, so the review page can show their reasoning.
    send(
        &mut eng,
        1,
        json!({
            "type": "diary_meta",
            "drawings": [{"kind": "hline", "price": 81234.5}],
            "criteria": {"trend": ["ma200", "ema50"], "oi": []}
        }),
    );

    let d = diary(&eng, NICK);
    assert_eq!(d["symbol"], SYM);
    assert_eq!(d["timeframe"], "1h");
    assert_eq!(d["exchange"], "binance");
    let range = &d["range"];
    let session = &eng.sessions[&1];
    assert_eq!(int(range, "start_ms"), session.range_start_ms);
    assert_eq!(int(range, "reveal_start_ms"), session.reveal_start_ms);
    assert_eq!(int(range, "end_ms"), session.range_end_ms);
    assert!(int(range, "start_ms") <= int(range, "reveal_start_ms"));
    assert!(int(range, "reveal_start_ms") <= int(range, "end_ms"));

    assert_eq!(d["drawings"][0]["kind"], "hline");
    assert_eq!(d["drawings"][0]["price"], json!(81234.5));
    assert_eq!(d["criteria"]["trend"], json!(["ma200", "ema50"]));
    assert_eq!(d["criteria"]["oi"], json!([]));
}

// ---------------------------------------------------------------------------
// Regressions: the diary must survive the terminal's rough edges
// ---------------------------------------------------------------------------

#[test]
fn diary_survives_a_timeframe_switch() {
    // Switching timeframe used to rewind the clock before the trader's own
    // entry and replay months of history through the broker, which mangled
    // positions and fired stops at ancient prices.
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", NICK, RISK);
    market(&mut eng, 1, "buy", 0.05, None);
    jump(&mut eng, 1, 3_600_000, 3);

    let before = diary(&eng, NICK);
    let fills_before = eng.sessions[&1].ex.fetch_fills(None, None).len();
    let clock_before = eng.sessions[&1].ex.now_ms();
    assert_eq!(array(&before, "positions").len(), 1);

    // A bare timeframe switch: same window, no new dates.
    send(
        &mut eng,
        1,
        json!({"type": "subscribe", "symbol": SYM, "timeframe": "1h", "exchange": "binance"}),
    );

    // The clock snaps onto the new (hourly) bar grid, so it may move back by up
    // to one bar of the NEW timeframe — but never to the reveal point.
    let one_bar_ms = 3_600_000;
    assert!(
        eng.sessions[&1].ex.now_ms() + one_bar_ms >= clock_before,
        "the clock must not rewind past one hourly bar: {clock_before} -> {}",
        eng.sessions[&1].ex.now_ms()
    );
    assert_eq!(
        eng.sessions[&1].ex.fetch_fills(None, None).len(),
        fills_before,
        "switching timeframe must not invent fills"
    );

    let after = diary(&eng, NICK);
    assert_eq!(after["timeframe"], "1h", "the diary follows the selection");
    assert_eq!(
        array(&after, "positions").len(),
        1,
        "the position must survive the switch: {after}"
    );
    let (p_before, p_after) = (&array(&before, "positions")[0], &array(&after, "positions")[0]);
    for key in ["symbol", "side", "qty", "entry_price", "entry_time_ms"] {
        assert_eq!(p_after[key], p_before[key], "the entry must not move ({key}): {p_after}");
    }
    assert_eq!(array(&after, "trades").len(), 0, "no phantom trades: {after}");
}

#[test]
fn stop_loss_exit_lands_in_the_diary() {
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", NICK, RISK);

    // Put the stop above the lowest low of the whole remaining series, so the
    // fill is guaranteed by construction instead of by luck.
    // Put the stop above the lowest low of the bars still ahead, so the fill is
    // guaranteed by construction instead of by luck.
    let rows = series(&eng, 1, "5m");
    let now = eng.sessions[&1].ex.now_ms();
    let ahead: Vec<f64> = rows
        .iter()
        .filter(|bar| bar[0] as i64 > now)
        .map(|bar| bar[3])
        .collect();
    assert!(!ahead.is_empty(), "the window must have bars left to walk");
    let floor = ahead.iter().copied().fold(f64::MAX, f64::min);
    let entry_ref = last_price(&eng, 1);
    let stop = floor * 1.0005;
    assert!(stop < entry_ref, "a long's stop must sit below the entry: {stop} vs {entry_ref}");

    market(&mut eng, 1, "buy", 0.05, Some(stop));
    assert_eq!(
        eng.sessions[&1].ex.fetch_open_orders(Some(SYM)).len(),
        1,
        "the stop is armed right after the entry fills"
    );

    // Walk the replay to the end of the window in one coarse jump: every bar in
    // between is fed, so the stop is certain to be touched.
    jump(&mut eng, 1, 60 * DAY_MS, 2);

    let d = diary(&eng, NICK);
    let trades = array(&d, "trades");
    assert_eq!(trades.len(), 1, "the stop closed the trade: {d}");
    assert_eq!(trades[0]["side"], "long");
    // The fill itself says which level ended the trade. Price proximity cannot:
    // a stop is a market order and may fill well past its trigger on a wide
    // bar, which would otherwise be reported as a manual exit.
    assert_eq!(
        trades[0]["exit_reason"], "stop",
        "the diary must say the stop closed it: {trades:?}"
    );
    assert!(
        num(&trades[0], "exit_price") < num(&trades[0], "entry_price"),
        "a stop loss exits below the entry: {trades:?}"
    );
    assert!(
        num(&trades[0], "exit_price") >= stop * 0.999,
        "the fill must be at the stop, not at some months-old price: {} vs stop {stop}",
        num(&trades[0], "exit_price")
    );
    assert!(array(&d, "positions").is_empty(), "nothing left open: {d}");
    assert!(
        eng.sessions[&1].ex.fetch_open_orders(Some(SYM)).is_empty(),
        "the protection must not outlive the position"
    );
    assert!(num(&d["stats"], "total_pnl") < 0.0, "the stop cost money: {d}");
    fills_balance_the_diary(&eng, 1, &d, true);
}

#[test]
fn diary_reports_the_protective_levels_the_trader_planned() {
    // The in-session review compares the outcome against the plan, and a plan
    // cannot be guessed from fills: the stop and TP the trader set must ride
    // along with the trade.
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", NICK, RISK);
    let entry = last_price(&eng, 1);
    // Far below anything in the window: this case is a MANUAL close with the
    // stop still armed, so the stop must never get a chance to fire.
    let stop = entry * 0.5;

    market(&mut eng, 1, "buy", 0.1, Some(stop));
    assert_eq!(
        eng.sessions[&1].ex.fetch_open_orders(Some(SYM)).len(),
        1,
        "the stop is armed, so the plan exists"
    );

    // Close by hand so the trade lands in the diary.
    jump(&mut eng, 1, 3_600_000, 2);
    assert_eq!(
        eng.sessions[&1].ex.fetch_open_orders(Some(SYM)).len(),
        1,
        "the stop must still be armed — this is about a manual close"
    );
    market(&mut eng, 1, "sell", 0.1, None);

    let d = diary(&eng, NICK);
    let trades = array(&d, "trades");
    assert_eq!(trades.len(), 1, "the round trip closed: {d}");
    let t = &trades[0];
    assert!(
        (num(t, "stop_price") - stop).abs() < 1e-9,
        "the stop the trader set must reach the diary: {t}"
    );
    assert!(
        t["take_price"].is_null(),
        "no TP was set, so it must be null rather than guessed: {t}"
    );
    assert!(
        t["exit_reason"].is_null(),
        "closed by hand while the stop was still armed — not a stop-out: {t}"
    );
}

#[test]
fn an_unprotected_entry_is_reported_without_levels() {
    // No stop at all: the review must see that (it IS the finding), not a
    // fabricated level that would make R look defined.
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", NICK, RISK);
    market(&mut eng, 1, "buy", 0.1, None);
    jump(&mut eng, 1, 3_600_000, 2);
    market(&mut eng, 1, "sell", 0.1, None);

    let d = diary(&eng, NICK);
    let t = &array(&d, "trades")[0];
    assert!(t["stop_price"].is_null(), "no stop was set: {t}");
    assert!(t["take_price"].is_null(), "no TP was set: {t}");
    // Closed by hand ⇒ no protective level, and the diary must not claim one.
    assert!(t["exit_reason"].is_null(), "a manual exit has no level: {t}");
}

// ---------------------------------------------------------------------------
// The participants board
// ---------------------------------------------------------------------------

#[test]
fn traders_board_ranks_participants_by_r() {
    let (mut eng, _rx) = engine();
    // Богач: same 0.2 round trip, but 5% of capital at risk.
    open_trader(&mut eng, 1, "5m", "Скромный", 10.0);
    open_trader(&mut eng, 2, "5m", "Смелый", 2.0);
    market(&mut eng, 1, "buy", 0.2, None);
    jump(&mut eng, 1, 3_600_000, 2);
    market(&mut eng, 1, "sell", 0.2, None);
    // Смелый stays flat on purpose: his R is 0, so he must sort last.

    let board = traders(&eng);
    let rows = array(&board, "traders");
    assert_eq!(rows.len(), 2, "both participants are listed: {board}");
    assert_eq!(rows[0]["nick"], "Скромный", "the one with a closed trade is first: {board}");
    assert_eq!(rows[1]["nick"], "Смелый");
    assert!(num(&rows[0], "r") > num(&rows[1], "r"), "sorted by R desc: {board}");
    // The board ranks by the account's own PnL (realized + unrealized) over the
    // joined risk, not by the diary's per-trade R.
    for row in rows {
        let pnl = num(row, "realized") + num(row, "unrealized");
        assert!((num(row, "pnl") - pnl).abs() < 1e-9, "{row}");
        assert!(
            (num(row, "r") - pnl / num(row, "risk_usdt")).abs() < 1e-9,
            "R is the account PnL over the risk: {row}"
        );
        assert_eq!(row["in_challenge"], json!(true));
        assert!(num(row, "risk_usdt") > 0.0, "{row}");
        assert!(num(row, "capital_usdt") > 0.0, "{row}");
        assert!(num(row, "equity") > 0.0, "{row}");
        assert_eq!(row["symbol"], SYM);
    }
    assert!(num(&rows[0], "trades") >= 1.0, "the trader who traded is counted: {board}");
    assert_eq!(num(&rows[1], "trades"), 0.0, "the flat one is not: {board}");
}

#[test]
fn traders_board_drops_a_departed_trader() {
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", "Первый", RISK);
    open_trader(&mut eng, 2, "5m", "Второй", RISK);

    let nicks = |eng: &Engine| -> Vec<String> {
        let mut out: Vec<String> = array(&traders(eng), "traders")
            .iter()
            .map(|row| row["nick"].as_str().unwrap_or_default().to_string())
            .collect();
        out.sort();
        out
    };
    assert_eq!(nicks(&eng), vec!["Второй".to_string(), "Первый".to_string()]);

    disconnect(&mut eng, 1);

    assert_eq!(
        nicks(&eng),
        vec!["Второй".to_string()],
        "a closed tab must leave the board"
    );
    assert_eq!(diary(&eng, "Первый")["error"], "trader not found");
    assert!(diary(&eng, "Второй").get("error").is_none(), "the other trader is untouched");
}

/// Mirror one browser engine's fills the way `local_sync` does.
fn local_sync(eng: &mut Engine, client_id: u64, engine_id: &str, fills: Value, realized: f64) {
    send(
        eng,
        client_id,
        json!({
            "type": "local_sync",
            "account": {
                "equity": CAPITAL + realized,
                "pnl": { "realized": realized, "unrealized": 0.0, "fees": 0.0 },
                "positions": []
            },
            "fills": fills,
            "replace_fills": false,
            "engine_id": engine_id,
            "entry_protections": {},
            "exit_reasons": {},
            "symbol": SYM,
            "timeframe": "5m",
            "exchange": "binance",
            "range": { "start_ms": 0, "reveal_start_ms": 0, "end_ms": 0 }
        }),
    );
}

/// A page reload builds a NEW browser engine, and order ids start over at 1.
/// The reloaded tab replays from the same bar, so its entry fill is identical
/// to the old page's entry (same id, time, price, amount) while its exit is a
/// different execution. FIFO over the merged bag paired the new entry with the
/// OLD exit and the old entry with the new exit, inventing a trade that never
/// happened (and reversing a side).
#[test]
fn a_reload_does_not_pair_fills_across_engine_instances() {
    let (mut eng, _rx) = engine();
    open_trader(&mut eng, 1, "5m", NICK, RISK);

    // Page 1: long 1.0 at 100, closed at 110.
    local_sync(
        &mut eng,
        1,
        "page-1",
        json!([
            {"id":"1_0","symbol":SYM,"timestamp":1_000,"price":100.0,"amount":1.0,"orderId":"1","side":"buy","fee":0.0},
            {"id":"2_0","symbol":SYM,"timestamp":2_000,"price":110.0,"amount":1.0,"orderId":"2","side":"sell","fee":0.0}
        ]),
        10.0,
    );
    // Page 2 (reload): the same entry, a different exit at 105.
    local_sync(
        &mut eng,
        1,
        "page-2",
        json!([
            {"id":"1_0","symbol":SYM,"timestamp":1_000,"price":100.0,"amount":1.0,"orderId":"1","side":"buy","fee":0.0},
            {"id":"2_0","symbol":SYM,"timestamp":3_000,"price":105.0,"amount":1.0,"orderId":"2","side":"sell","fee":0.0}
        ]),
        15.0,
    );

    let d = diary(&eng, NICK);
    let trades = array(&d, "trades");
    assert_eq!(trades.len(), 2, "two real round trips, no phantom: {d}");
    assert!(
        trades.iter().all(|t| t["side"] == "long"),
        "both page loads were longs: {d}"
    );
    let exits: Vec<f64> = trades.iter().map(|t| num(t, "exit_price")).collect();
    assert_eq!(exits, vec![110.0, 105.0], "each exit belongs to its own entry");
    assert_eq!(num(&trades[1], "pnl"), 5.0);
}
