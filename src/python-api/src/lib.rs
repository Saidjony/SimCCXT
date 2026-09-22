//! PyO3 bindings for simccxt.
//!
//! Exposes the virtual exchange to Python with a CCXT-compatible surface.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyList, PyModule};
use pyo3::IntoPyObjectExt;
use serde_json::Value;

use simccxt_core::TimeInForce;
use simccxt_exchange::{CreateOrderParams, Exchange};

/// Recursively convert a `serde_json::Value` into a Python object.
fn value_to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Py<PyAny>> {
    match value {
        Value::Null => Ok(py.None()),
        Value::Bool(b) => b.into_py_any(py),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_py_any(py)
            } else if let Some(f) = n.as_f64() {
                f.into_py_any(py)
            } else {
                Ok(py.None())
            }
        }
        Value::String(s) => s.into_py_any(py),
        Value::Array(arr) => {
            let list = PyList::empty(py);
            for item in arr {
                list.append(value_to_py(py, item)?)?;
            }
            Ok(list.into_any().unbind())
        }
        Value::Object(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, value_to_py(py, v)?)?;
            }
            Ok(dict.into_any().unbind())
        }
    }
}

fn to_py_json(py: Python<'_>, value: Value) -> Py<PyAny> {
    value_to_py(py, &value).unwrap_or_else(|_| py.None())
}

/// A virtual cryptocurrency exchange, compatible with the CCXT interface.
#[pyclass(name = "Exchange", unsendable)]
pub struct PyExchange {
    inner: simccxt_exchange::Exchange,
}

#[pymethods]
impl PyExchange {
    /// Create a new exchange.
    #[new]
    fn new() -> Self {
        Self { inner: Exchange::default() }
    }

    /// Register a market (spot or linear futures).
    fn add_market(&mut self, symbol: &str, kind: &str) -> PyResult<()> {
        let symbol = simccxt_core::Symbol::parse(symbol)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let kind = match kind {
            "spot" => simccxt_broker::MarketKind::Spot,
            "linear" | "swap" | "futures" => simccxt_broker::MarketKind::Linear,
            _ => return Err(PyValueError::new_err(format!("unknown market kind: {kind}"))),
        };
        self.inner.add_market(&symbol, kind);
        Ok(())
    }

    /// Deposit funds into the wallet.
    fn deposit(&mut self, asset: &str, amount: f64) {
        self.inner.deposit(asset, amount);
    }

    /// Load historical OHLCV candles (each row is [ts_ms, o, h, l, c, v]).
    #[pyo3(signature = (symbol, timeframe, candles))]
    fn load_candles(&mut self, symbol: &str, timeframe: &str, candles: Vec<[f64; 6]>) -> PyResult<()> {
        let mut ohlcv = Vec::with_capacity(candles.len());
        for row in candles {
            let ts = chrono::DateTime::from_timestamp_millis(row[0] as i64)
                .ok_or_else(|| PyValueError::new_err("invalid timestamp"))?;
            ohlcv.push(simccxt_core::OHLCV::new(ts, row[1], row[2], row[3], row[4], row[5]));
        }
        self.inner.load_candles(symbol, timeframe, ohlcv);
        Ok(())
    }

    /// Load candles from a CSV file on disk (simulator data source).
    ///
    /// CSV columns: `timestamp,open,high,low,close,volume`.
    /// `timestamp` may be epoch seconds/milliseconds or an ISO-8601 datetime
    /// (e.g. `2026-03-01 00:00:00`, `2026-03-01T00:00:00+00:00`).
    fn load_candles_from_csv(&mut self, symbol: &str, timeframe: &str, path: &str) -> PyResult<()> {
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(path)
            .map_err(|e| PyValueError::new_err(format!("csv open {path}: {e}")))?;
        let mut candles = Vec::new();
        for rec in rdr.records() {
            let rec = rec.map_err(|e| PyValueError::new_err(format!("csv record: {e}")))?;
            if rec.len() < 6 {
                return Err(PyValueError::new_err("row must have 6 columns"));
            }
            let parse = |i: usize| -> PyResult<f64> {
                rec.get(i)
                    .ok_or_else(|| PyValueError::new_err("missing column"))?
                    .parse()
                    .map_err(|e| PyValueError::new_err(format!("bad column {}: {e}", i)))
            };
            let ms = parse_ts_ms(rec.get(0).unwrap_or(""))?;
            let ts = chrono::DateTime::from_timestamp_millis(ms)
                .ok_or_else(|| PyValueError::new_err(format!("bad timestamp {}", rec.get(0).unwrap_or(""))))?;
            candles.push(simccxt_core::OHLCV::new(ts, parse(1)?, parse(2)?, parse(3)?, parse(4)?, parse(5)?));
        }
        self.inner.load_candles(symbol, timeframe, candles);
        Ok(())
    }

    /// The current simulation time in milliseconds.
    fn now_ms(&self) -> i64 {
        self.inner.now_ms()
    }

    /// Set the simulator clock to a timestamp (milliseconds). The clock starts
    /// paused; call `resume()` before stepping.
    fn reset(&mut self, time_ms: i64) -> PyResult<()> {
        let ts = chrono::DateTime::from_timestamp_millis(time_ms)
            .ok_or_else(|| PyValueError::new_err("invalid timestamp"))?;
        self.inner.reset(ts);
        Ok(())
    }

    /// Resume the simulator clock (start advancing on `step`).
    fn resume(&mut self) {
        self.inner.resume();
    }

    /// Pause the simulator clock (steps no longer advance time).
    fn pause(&mut self) {
        self.inner.pause();
    }

    /// Whether the simulator clock is currently paused.
    fn is_paused(&self) -> bool {
        self.inner.is_paused()
    }

    /// Set the simulator clock step size in milliseconds.
    fn set_step_ms(&mut self, ms: u64) {
        self.inner.set_step_ms(ms);
    }

    /// Advance the simulator clock by `n` steps (each `step_ms` long).
    /// Returns the new simulation time in milliseconds.
    #[pyo3(signature = (n=1))]
    fn step(&mut self, n: u64) -> i64 {
        self.inner.step_n(n);
        self.inner.now_ms()
    }

    /// Advance until the last candle is consumed; returns final time (ms).
    fn run(&mut self) -> i64 {
        self.inner.run();
        self.inner.now_ms()
    }

    /// `fetch_ohlcv_live(symbol, timeframe)` — OHLCV rows with
    /// `timestamp <= current simulation time` (as a step-wise feed).
    fn fetch_ohlcv_live(&self, py: Python<'_>, symbol: &str, timeframe: &str) -> Py<PyAny> {
        let rows = self.inner.fetch_ohlcv_live(symbol, timeframe);
        let list = PyList::empty(py);
        for row in rows {
            let r = PyList::empty(py);
            for v in row {
                let _ = r.append(v);
            }
            let _ = list.append(r);
        }
        list.into_any().unbind()
    }

    /// Whether there are no more candles to replay for a symbol/timeframe.
    fn replay_done(&self, symbol: &str, timeframe: &str) -> bool {
        self.inner.replay_done(symbol, timeframe)
    }

    /// `fetch_ohlcv(symbol, timeframe, since=None, limit=None)`.
    #[pyo3(signature = (symbol, timeframe, since=None, limit=None))]
    fn fetch_ohlcv(
        &self,
        py: Python<'_>,
        symbol: &str,
        timeframe: &str,
        since: Option<i64>,
        limit: Option<usize>,
    ) -> Py<PyAny> {
        let rows = self.inner.fetch_ohlcv(symbol, timeframe, since, limit);
        let list = PyList::empty(py);
        for row in rows {
            let r = PyList::empty(py);
            for v in row {
                let _ = r.append(v);
            }
            let _ = list.append(r);
        }
        list.into_any().unbind()
    }

    /// `fetch_ticker(symbol)`.
    fn fetch_ticker(&self, py: Python<'_>, symbol: &str) -> Py<PyAny> {
        to_py_json(py, self.inner.fetch_ticker(symbol))
    }

    /// `fetch_order_book(symbol, limit=None)`.
    #[pyo3(signature = (symbol, limit=None))]
    fn fetch_order_book(&self, py: Python<'_>, symbol: &str, limit: Option<usize>) -> Py<PyAny> {
        to_py_json(py, self.inner.fetch_order_book(symbol, limit))
    }

    /// `fetch_trades(symbol, limit=None)`.
    #[pyo3(signature = (symbol, limit=None))]
    fn fetch_trades(&self, py: Python<'_>, symbol: &str, limit: Option<usize>) -> Py<PyAny> {
        let trades = self.inner.fetch_trades(symbol, limit);
        let list = PyList::empty(py);
        for t in trades {
            let _ = list.append(value_to_py(py, &t).unwrap());
        }
        list.into_any().unbind()
    }

    /// `fetch_fills(symbol, limit=None)` — the account's own executed trades
    /// (fills produced by Rust execution), newest first.
    #[pyo3(signature = (symbol=None, limit=None))]
    fn fetch_fills(&self, py: Python<'_>, symbol: Option<&str>, limit: Option<usize>) -> Py<PyAny> {
        let fills = self.inner.fetch_fills(symbol, limit);
        let list = PyList::empty(py);
        for f in fills {
            let _ = list.append(value_to_py(py, &f).unwrap());
        }
        list.into_any().unbind()
    }

    /// `fetch_balance()`.
    fn fetch_balance(&self, py: Python<'_>) -> Py<PyAny> {
        to_py_json(py, self.inner.fetch_balance())
    }

    /// `fetch_positions(symbols=None)`.
    #[pyo3(signature = (symbols=None))]
    fn fetch_positions(&self, py: Python<'_>, symbols: Option<Vec<String>>) -> Py<PyAny> {
        let positions = self.inner.fetch_positions(symbols.as_deref());
        let list = PyList::empty(py);
        for p in positions {
            let _ = list.append(value_to_py(py, &p).unwrap());
        }
        list.into_any().unbind()
    }

    /// `fetch_open_orders(symbol=None)`.
    #[pyo3(signature = (symbol=None))]
    fn fetch_open_orders(&self, py: Python<'_>, symbol: Option<&str>) -> Py<PyAny> {
        let orders = self.inner.fetch_open_orders(symbol);
        let list = PyList::empty(py);
        for o in orders {
            let _ = list.append(value_to_py(py, &o).unwrap());
        }
        list.into_any().unbind()
    }

    /// `fetch_funding_rate(symbol)`.
    fn fetch_funding_rate(&self, py: Python<'_>, symbol: &str) -> Py<PyAny> {
        to_py_json(py, self.inner.fetch_funding_rate(symbol))
    }

    /// `fetch_open_interest(symbol)`.
    fn fetch_open_interest(&self, py: Python<'_>, symbol: &str) -> Py<PyAny> {
        to_py_json(py, self.inner.fetch_open_interest(symbol))
    }

    /// `create_order(symbol, type, side, amount, price=None, params=None)`.
    #[pyo3(signature = (symbol, order_type, side, amount, price=None, params=None))]
    #[allow(clippy::too_many_arguments)]
    fn create_order(
        &mut self,
        py: Python<'_>,
        symbol: &str,
        order_type: &str,
        side: &str,
        amount: f64,
        price: Option<f64>,
        params: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        let mut p = CreateOrderParams::new();
        if let Some(params) = params {
            if let Some(v) = params.get_item("stopPrice")? {
                p.stop_price = Some(v.extract()?);
            }
            if let Some(v) = params.get_item("stop_price")? {
                p.stop_price = Some(v.extract()?);
            }
            if let Some(v) = params.get_item("reduceOnly")? {
                p.reduce_only = v.extract()?;
            }
            if let Some(v) = params.get_item("postOnly")? {
                p.post_only = v.extract()?;
            }
            if let Some(v) = params.get_item("timeInForce")? {
                let tif: String = v.extract()?;
                p.time_in_force = parse_tif(&tif)?;
            }
        }
        match self.inner.create_order(symbol, order_type, side, amount, price, p) {
            Ok(order) => Ok(to_py_json(py, order)),
            Err(e) => Err(PyValueError::new_err(e.to_string())),
        }
    }

    /// `cancel_order(id, symbol)`.
    fn cancel_order(&mut self, py: Python<'_>, id: &str, symbol: &str) -> PyResult<Py<PyAny>> {
        match self.inner.cancel_order(id, symbol) {
            Ok(order) => Ok(to_py_json(py, order)),
            Err(e) => Err(PyValueError::new_err(e.to_string())),
        }
    }

    /// `account_snapshot()` — one compact call with balances, equity, PnL,
    /// positions and open orders. Lightweight for UI timer snapshots.
    fn account_snapshot(&self, py: Python<'_>) -> Py<PyAny> {
        to_py_json(py, self.inner.account_snapshot())
    }

    /// `market_snapshot(symbol)` — last price / ticker for one symbol.
    fn market_snapshot(&self, py: Python<'_>, symbol: &str) -> Py<PyAny> {
        to_py_json(py, self.inner.market_snapshot(symbol))
    }

    /// `close_position(symbol, qty=None)` — market-counter a position to flat.
    /// For spot, sells `qty` of the base asset at the last price (defaults to
    /// the full held quantity). For linear, flattens the portfolio position.
    fn close_position(&mut self, py: Python<'_>, symbol: &str, qty: Option<f64>) -> PyResult<Py<PyAny>> {
        let (held, _) = self.inner.spot_basis(symbol);
        let q = qty.unwrap_or(held);
        if q <= 0.0 {
            return Err(PyValueError::new_err("nothing to close"));
        }
        match self.inner.create_order(symbol, "market", "sell", q, None, CreateOrderParams::new()) {
            Ok(order) => Ok(to_py_json(py, order)),
            Err(e) => Err(PyValueError::new_err(e.to_string())),
        }
    }
}

/// Create a new exchange (factory, mirrors `simccxt.binance()`).
#[pyfunction]
fn binance() -> PyExchange {
    PyExchange::new()
}

/// Create a new exchange (factory, mirrors `simccxt.bybit()`).
#[pyfunction]
fn bybit() -> PyExchange {
    PyExchange::new()
}

/// Create a new exchange (factory, mirrors `simccxt.okx()`).
#[pyfunction]
fn okx() -> PyExchange {
    PyExchange::new()
}

/// Parse a time-in-force string ("GTC", "IOC", "FOK", "GTX") into the core enum.
fn parse_tif(s: &str) -> PyResult<TimeInForce> {
    match s.trim().to_uppercase().as_str() {
        "GTC" => Ok(TimeInForce::GTC),
        "IOC" => Ok(TimeInForce::IOC),
        "FOK" => Ok(TimeInForce::FOK),
        "GTX" => Ok(TimeInForce::GTX),
        "DAY" => Ok(TimeInForce::DAY),
        other => Err(PyValueError::new_err(format!("unknown timeInForce: {other}"))),
    }
}

/// Parse a timestamp column value (epoch seconds/ms or ISO-8601) to epoch ms.
fn parse_ts_ms(raw: &str) -> PyResult<i64> {
    if let Ok(v) = raw.trim().parse::<f64>() {
        let ms = if v.abs() < 1e12 { (v * 1000.0) as i64 } else { v as i64 };
        return Ok(ms);
    }
    // ISO-8601 variants
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw.trim()) {
        return Ok(dt.timestamp_millis());
    }
    let s = raw.trim().trim_end_matches('Z').trim_end_matches("+00:00");
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(ndt.and_utc().timestamp_millis());
        }
    }
    Err(PyValueError::new_err(format!("cannot parse timestamp: {raw:?}")))
}

/// The `simccxt` Python module.
#[pymodule]
fn simccxt(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyExchange>()?;
    m.add_function(wrap_pyfunction!(binance, m)?)?;
    m.add_function(wrap_pyfunction!(bybit, m)?)?;
    m.add_function(wrap_pyfunction!(okx, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
