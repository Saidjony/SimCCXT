//! Technical indicators for simccxt.
//!
//! Pure, deterministic indicator calculations over price series. All functions
//! return an aligned series (same length as input) with `NaN` for warm-up
//! positions where the indicator is undefined.

use simccxt_core::OHLCV;

/// Compute the Simple Moving Average.
///
/// Returns a series of the same length as `values`; the first `period - 1`
/// entries are `NaN`.
pub fn sma(values: &[f64], period: usize) -> Vec<f64> {
    assert!(period > 0, "period must be positive");
    let mut out = vec![f64::NAN; values.len()];
    if period == 0 || values.len() < period {
        return out;
    }
    let mut sum: f64 = values[..period].iter().sum();
    out[period - 1] = sum / period as f64;
    for i in period..values.len() {
        sum += values[i] - values[i - period];
        out[i] = sum / period as f64;
    }
    out
}

/// Compute the Exponential Moving Average.
///
/// Uses a smoothing factor `alpha = 2 / (period + 1)`. The first value is
/// seeded with the first input; entries before `period - 1` are `NaN`.
pub fn ema(values: &[f64], period: usize) -> Vec<f64> {
    assert!(period > 0, "period must be positive");
    let mut out = vec![f64::NAN; values.len()];
    if values.is_empty() {
        return out;
    }
    let alpha = 2.0 / (period as f64 + 1.0);
    out[0] = values[0];
    for i in 1..values.len() {
        out[i] = alpha * values[i] + (1.0 - alpha) * out[i - 1];
    }
    // Warm-up: mark entries before period-1 as NaN.
    for v in out.iter_mut().take(period.saturating_sub(1).min(values.len())) {
        *v = f64::NAN;
    }
    out
}

/// Compute the Relative Strength Index (Wilder's smoothing).
///
/// Returns a series where the first `period` entries are `NaN`.
pub fn rsi(closes: &[f64], period: usize) -> Vec<f64> {
    assert!(period > 0, "period must be positive");
    let mut out = vec![f64::NAN; closes.len()];
    if closes.len() <= period {
        return out;
    }

    let mut gains = 0.0;
    let mut losses = 0.0;
    for i in 1..=period {
        let change = closes[i] - closes[i - 1];
        if change >= 0.0 {
            gains += change;
        } else {
            losses -= change;
        }
    }
    let mut avg_gain = gains / period as f64;
    let mut avg_loss = losses / period as f64;

    out[period] = rsi_value(avg_gain, avg_loss);

    for i in (period + 1)..closes.len() {
        let change = closes[i] - closes[i - 1];
        let gain = if change >= 0.0 { change } else { 0.0 };
        let loss = if change < 0.0 { -change } else { 0.0 };
        avg_gain = (avg_gain * (period as f64 - 1.0) + gain) / period as f64;
        avg_loss = (avg_loss * (period as f64 - 1.0) + loss) / period as f64;
        out[i] = rsi_value(avg_gain, avg_loss);
    }
    out
}

fn rsi_value(avg_gain: f64, avg_loss: f64) -> f64 {
    if avg_loss == 0.0 {
        return 100.0;
    }
    let rs = avg_gain / avg_loss;
    100.0 - (100.0 / (1.0 + rs))
}

/// The result of a MACD calculation.
#[derive(Debug, Clone, PartialEq)]
pub struct Macd {
    /// MACD line.
    pub macd: Vec<f64>,
    /// Signal line.
    pub signal: Vec<f64>,
    /// Histogram (macd - signal).
    pub histogram: Vec<f64>,
}

/// Compute the MACD (Moving Average Convergence Divergence).
///
/// `macd = ema(closes, fast) - ema(closes, slow)`, `signal = ema(macd, signal)`.
pub fn macd(closes: &[f64], fast: usize, slow: usize, signal_period: usize) -> Macd {
    let ema_fast = ema(closes, fast);
    let ema_slow = ema(closes, slow);
    let macd_line: Vec<f64> = ema_fast
        .iter()
        .zip(ema_slow.iter())
        .map(|(f, s)| f - s)
        .collect();
    // Signal EMA over the valid (non-NaN) MACD line.
    let signal = ema_valid(&macd_line, signal_period);
    let histogram = macd_line
        .iter()
        .zip(signal.iter())
        .map(|(m, s)| m - s)
        .collect();
    Macd { macd: macd_line, signal, histogram }
}

/// EMA that tolerates leading `NaN` values (skips them during warm-up).
fn ema_valid(values: &[f64], period: usize) -> Vec<f64> {
    let mut out = vec![f64::NAN; values.len()];
    let valid: Vec<f64> = values.iter().copied().filter(|v| !v.is_nan()).collect();
    if valid.is_empty() {
        return out;
    }
    let alpha = 2.0 / (period as f64 + 1.0);
    let mut prev = valid[0];
    let mut valid_idx = 0;
    for i in 0..values.len() {
        if values[i].is_nan() {
            continue;
        }
        if valid_idx == 0 {
            out[i] = prev;
        } else {
            prev = alpha * values[i] + (1.0 - alpha) * prev;
            out[i] = prev;
        }
        valid_idx += 1;
    }
    out
}

/// The result of a Bollinger Bands calculation.
#[derive(Debug, Clone, PartialEq)]
pub struct BollingerBands {
    /// Upper band.
    pub upper: Vec<f64>,
    /// Middle band (SMA).
    pub middle: Vec<f64>,
    /// Lower band.
    pub lower: Vec<f64>,
}

/// Compute Bollinger Bands.
pub fn bollinger_bands(closes: &[f64], period: usize, num_std: f64) -> BollingerBands {
    let middle = sma(closes, period);
    let mut upper = vec![f64::NAN; closes.len()];
    let mut lower = vec![f64::NAN; closes.len()];

    for i in (period - 1)..closes.len() {
        let window = &closes[i + 1 - period..=i];
        let mean = middle[i];
        let variance: f64 = window.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / period as f64;
        let std_dev = variance.sqrt();
        upper[i] = mean + num_std * std_dev;
        lower[i] = mean - num_std * std_dev;
    }
    BollingerBands { upper, middle, lower }
}

/// Compute the Average True Range.
pub fn atr(candles: &[OHLCV], period: usize) -> Vec<f64> {
    assert!(period > 0, "period must be positive");
    let mut out = vec![f64::NAN; candles.len()];
    if candles.len() <= period {
        return out;
    }
    let mut true_ranges = Vec::with_capacity(candles.len());
    for (i, c) in candles.iter().enumerate() {
        let tr = if i == 0 {
            c.high - c.low
        } else {
            let prev_close = candles[i - 1].close;
            (c.high - c.low)
                .max((c.high - prev_close).abs())
                .max((c.low - prev_close).abs())
        };
        true_ranges.push(tr);
    }
    // Wilder's smoothing of the true range.
    let mut avg = true_ranges[..period].iter().sum::<f64>() / period as f64;
    out[period - 1] = avg;
    for i in period..true_ranges.len() {
        avg = (avg * (period as f64 - 1.0) + true_ranges[i]) / period as f64;
        out[i] = avg;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sma_basic() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let out = sma(&values, 3);
        assert!(out[0].is_nan());
        assert!(out[1].is_nan());
        assert!((out[2] - 2.0).abs() < 1e-9);
        assert!((out[3] - 3.0).abs() < 1e-9);
        assert!((out[4] - 4.0).abs() < 1e-9);
    }

    #[test]
    fn test_sma_period_larger_than_input() {
        let values = vec![1.0, 2.0];
        let out = sma(&values, 5);
        assert!(out.iter().all(|v| v.is_nan()));
    }

    #[test]
    fn test_ema_basic() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let out = ema(&values, 3);
        // alpha = 0.5
        // ema[0] = 1
        // ema[1] = 0.5*2 + 0.5*1 = 1.5
        // ema[2] = 0.5*3 + 0.5*1.5 = 2.25
        assert!((out[2] - 2.25).abs() < 1e-9);
    }

    #[test]
    fn test_ema_warmup_is_nan() {
        let values = vec![1.0, 2.0, 3.0, 4.0];
        let out = ema(&values, 3);
        assert!(out[0].is_nan());
        assert!(out[1].is_nan());
        assert!(!out[2].is_nan());
    }

    #[test]
    fn test_rsi_all_gains() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let out = rsi(&values, 3);
        // No losses => RSI = 100.
        assert!((out[3] - 100.0).abs() < 1e-9);
    }

    #[test]
    fn test_rsi_bounds() {
        // A price series that rises and falls should keep RSI within [0, 100].
        let values: Vec<f64> = (0..50).map(|i| (i as f64).sin() * 10.0 + 100.0).collect();
        let out = rsi(&values, 14);
        for v in out.iter().skip(14) {
            assert!(*v >= 0.0 && *v <= 100.0, "rsi out of range: {v}");
        }
    }

    #[test]
    fn test_macd_shape() {
        let values: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let m = macd(&values, 12, 26, 9);
        assert_eq!(m.macd.len(), values.len());
        assert_eq!(m.signal.len(), values.len());
        assert_eq!(m.histogram.len(), values.len());
    }

    #[test]
    fn test_bollinger_bands() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let bb = bollinger_bands(&values, 3, 2.0);
        assert!((bb.middle[2] - 2.0).abs() < 1e-9);
        // std of [1,2,3] = sqrt(2/3) ≈ 0.8165
        let std = (2.0f64 / 3.0).sqrt();
        assert!((bb.upper[2] - (2.0 + 2.0 * std)).abs() < 1e-9);
        assert!((bb.lower[2] - (2.0 - 2.0 * std)).abs() < 1e-9);
    }

    #[test]
    fn test_atr() {
        let candles = vec![
            OHLCV::new(chrono::Utc::now(), 1.0, 2.0, 0.5, 1.5, 10.0),
            OHLCV::new(chrono::Utc::now(), 1.5, 3.0, 1.0, 2.0, 10.0),
            OHLCV::new(chrono::Utc::now(), 2.0, 4.0, 1.5, 3.0, 10.0),
            OHLCV::new(chrono::Utc::now(), 3.0, 5.0, 2.0, 4.0, 10.0),
        ];
        let out = atr(&candles, 3);
        assert!(out[0].is_nan());
        assert!(out[1].is_nan());
        assert!(!out[2].is_nan());
        assert!(out[3] > 0.0);
    }
}
