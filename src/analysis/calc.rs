//! Per-bar derived series: the [`Indicator`] trait, built-ins, custom functions,
//! and the [`Calc`] pipeline.
//!
//! Built-in specs are `sma:N`, `ema:N`, `rsi:N`, `ret:log`, `ret:simple`, `vol:N`.
//! API users can also register arbitrary closures with [`Calc::with_fn`].

use anyhow::{Result, bail};

use crate::analysis::config::ReturnMethod;
use crate::analysis::model::Bar;

/// A per-bar derived column. `compute` returns one value per input bar
/// (`None` during the indicator's warm-up).
pub trait Indicator {
    fn name(&self) -> String;
    fn compute(&self, bars: &[Bar]) -> Vec<Option<f64>>;
}

fn closes(bars: &[Bar]) -> Vec<f64> {
    bars.iter().map(|b| b.close).collect()
}

fn one_return(method: ReturnMethod, prev: f64, cur: f64) -> f64 {
    match method {
        ReturnMethod::Log => (cur / prev).ln(),
        ReturnMethod::Simple => (cur - prev) / prev,
    }
}

/// Simple moving average of close over `period` bars.
pub struct Sma {
    pub period: usize,
}

impl Indicator for Sma {
    fn name(&self) -> String {
        format!("sma_{}", self.period)
    }

    fn compute(&self, bars: &[Bar]) -> Vec<Option<f64>> {
        let closes = closes(bars);
        let n = self.period;
        let mut out = vec![None; bars.len()];
        if n == 0 {
            return out;
        }
        let mut sum = 0.0;
        for i in 0..closes.len() {
            sum += closes[i];
            if i >= n {
                sum -= closes[i - n];
            }
            if i + 1 >= n {
                out[i] = Some(sum / n as f64);
            }
        }
        out
    }
}

/// Exponential moving average of close, seeded with the `period`-SMA.
pub struct Ema {
    pub period: usize,
}

impl Indicator for Ema {
    fn name(&self) -> String {
        format!("ema_{}", self.period)
    }

    fn compute(&self, bars: &[Bar]) -> Vec<Option<f64>> {
        let closes = closes(bars);
        let n = self.period;
        let mut out = vec![None; bars.len()];
        if n == 0 || closes.len() < n {
            return out;
        }
        let alpha = 2.0 / (n as f64 + 1.0);
        // Seed with the SMA of the first `n` closes at index n-1.
        let mut ema = closes[..n].iter().sum::<f64>() / n as f64;
        out[n - 1] = Some(ema);
        for i in n..closes.len() {
            ema = alpha * closes[i] + (1.0 - alpha) * ema;
            out[i] = Some(ema);
        }
        out
    }
}

/// Wilder's Relative Strength Index over `period` bars.
pub struct Rsi {
    pub period: usize,
}

impl Indicator for Rsi {
    fn name(&self) -> String {
        format!("rsi_{}", self.period)
    }

    fn compute(&self, bars: &[Bar]) -> Vec<Option<f64>> {
        let closes = closes(bars);
        let n = self.period;
        let mut out = vec![None; bars.len()];
        if n == 0 || closes.len() <= n {
            return out;
        }
        // Seed average gain/loss over the first `n` deltas (indices 1..=n).
        let mut avg_gain = 0.0;
        let mut avg_loss = 0.0;
        for i in 1..=n {
            let change = closes[i] - closes[i - 1];
            if change >= 0.0 {
                avg_gain += change;
            } else {
                avg_loss -= change;
            }
        }
        avg_gain /= n as f64;
        avg_loss /= n as f64;
        out[n] = Some(rsi_value(avg_gain, avg_loss));
        for i in (n + 1)..closes.len() {
            let change = closes[i] - closes[i - 1];
            let (gain, loss) = if change >= 0.0 {
                (change, 0.0)
            } else {
                (0.0, -change)
            };
            avg_gain = (avg_gain * (n as f64 - 1.0) + gain) / n as f64;
            avg_loss = (avg_loss * (n as f64 - 1.0) + loss) / n as f64;
            out[i] = Some(rsi_value(avg_gain, avg_loss));
        }
        out
    }
}

fn rsi_value(avg_gain: f64, avg_loss: f64) -> f64 {
    if avg_loss == 0.0 {
        100.0
    } else {
        let rs = avg_gain / avg_loss;
        100.0 - 100.0 / (1.0 + rs)
    }
}

/// Bar-to-bar returns (log or simple).
pub struct Returns {
    pub method: ReturnMethod,
}

impl Indicator for Returns {
    fn name(&self) -> String {
        match self.method {
            ReturnMethod::Log => "ret_log".into(),
            ReturnMethod::Simple => "ret_simple".into(),
        }
    }

    fn compute(&self, bars: &[Bar]) -> Vec<Option<f64>> {
        let closes = closes(bars);
        let mut out = vec![None; bars.len()];
        for i in 1..closes.len() {
            out[i] = Some(one_return(self.method, closes[i - 1], closes[i]));
        }
        out
    }
}

/// Rolling realized volatility: sample stdev of log returns over `period` bars.
pub struct Volatility {
    pub period: usize,
}

impl Indicator for Volatility {
    fn name(&self) -> String {
        format!("vol_{}", self.period)
    }

    fn compute(&self, bars: &[Bar]) -> Vec<Option<f64>> {
        let closes = closes(bars);
        let n = self.period;
        let mut out = vec![None; bars.len()];
        if n < 2 || closes.len() <= n {
            return out;
        }
        // Log returns at index i correspond to close[i]/close[i-1].
        let returns: Vec<f64> = (1..closes.len())
            .map(|i| (closes[i] / closes[i - 1]).ln())
            .collect();
        // A window of `n` returns ends at bar index i (returns index i-1).
        for i in n..closes.len() {
            let window = &returns[i - n..i];
            out[i] = Some(sample_stdev(window));
        }
        out
    }
}

fn sample_stdev(values: &[f64]) -> f64 {
    let n = values.len();
    if n < 2 {
        return f64::NAN;
    }
    let mean = values.iter().sum::<f64>() / n as f64;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
    var.sqrt()
}

struct CustomIndicator<F> {
    name: String,
    func: F,
}

impl<F> Indicator for CustomIndicator<F>
where
    F: Fn(&[Bar]) -> Vec<Option<f64>>,
{
    fn name(&self) -> String {
        self.name.clone()
    }

    fn compute(&self, bars: &[Bar]) -> Vec<Option<f64>> {
        (self.func)(bars)
    }
}

/// Parses a spec token (`sma:20`, `ret:log`, `vol:20`, ...) into an indicator.
///
/// Returns `None` when the token is not spec-shaped (so a classifier can treat
/// it as a path/format token), `Some(Err)` when it is spec-shaped but invalid.
pub fn parse_spec(token: &str) -> Option<Result<Box<dyn Indicator>>> {
    let (kind, arg) = token.split_once(':')?;
    // Only claim tokens whose prefix is a known indicator, so paths that contain
    // a colon (e.g. a Windows drive letter `C:\...`) fall through to path tokens.
    if !matches!(kind, "sma" | "ema" | "rsi" | "vol" | "ret") {
        return None;
    }
    let result = (|| -> Result<Box<dyn Indicator>> {
        let period = || -> Result<usize> {
            let n: usize = arg
                .parse()
                .map_err(|_| anyhow::anyhow!("{kind} expects an integer period, got {arg:?}"))?;
            if n == 0 {
                bail!("{kind} period must be at least 1");
            }
            Ok(n)
        };
        match kind {
            "sma" => Ok(Box::new(Sma { period: period()? })),
            "ema" => Ok(Box::new(Ema { period: period()? })),
            "rsi" => Ok(Box::new(Rsi { period: period()? })),
            "vol" => Ok(Box::new(Volatility { period: period()? })),
            "ret" => {
                let method = ReturnMethod::from_token(arg)
                    .ok_or_else(|| anyhow::anyhow!("ret expects log|simple, got {arg:?}"))?;
                Ok(Box::new(Returns { method }))
            }
            _ => bail!("unknown indicator {kind:?}"),
        }
    })();
    Some(result)
}

/// A column of computed values aligned to the input bars.
#[derive(Debug, Clone)]
pub struct CalcColumn {
    pub name: String,
    pub values: Vec<Option<f64>>,
}

/// The result of running a [`Calc`] pipeline.
pub struct CalcOutput<'a> {
    pub bars: &'a [Bar],
    pub columns: Vec<CalcColumn>,
}

/// Whole-series return/volatility summary.
#[derive(Debug, Clone)]
pub struct CalcSummary {
    pub method: ReturnMethod,
    pub count: usize,
    pub mean: f64,
    pub stdev: f64,
    pub realized_vol: f64,
    pub annualized: Option<f64>,
    pub min: f64,
    pub max: f64,
}

/// Builder/runner for per-bar indicator columns over a bar series.
pub struct Calc<'a> {
    bars: &'a [Bar],
    indicators: Vec<Box<dyn Indicator + 'a>>,
}

impl<'a> Calc<'a> {
    pub fn new(bars: &'a [Bar]) -> Self {
        Self {
            bars,
            indicators: Vec::new(),
        }
    }

    /// Adds a built-in or user-defined [`Indicator`].
    pub fn with<I: Indicator + 'a>(mut self, indicator: I) -> Self {
        self.indicators.push(Box::new(indicator));
        self
    }

    /// Adds a boxed indicator (e.g. one produced by [`parse_spec`]).
    pub fn with_boxed(mut self, indicator: Box<dyn Indicator + 'a>) -> Self {
        self.indicators.push(indicator);
        self
    }

    /// Adds a custom column computed by a user-supplied closure.
    pub fn with_fn<F>(mut self, name: impl Into<String>, func: F) -> Self
    where
        F: Fn(&[Bar]) -> Vec<Option<f64>> + 'a,
    {
        self.indicators.push(Box::new(CustomIndicator {
            name: name.into(),
            func,
        }));
        self
    }

    /// Runs every indicator and returns one aligned column per indicator.
    pub fn run(&self) -> CalcOutput<'a> {
        let columns = self
            .indicators
            .iter()
            .map(|indicator| CalcColumn {
                name: indicator.name(),
                values: indicator.compute(self.bars),
            })
            .collect();
        CalcOutput {
            bars: self.bars,
            columns,
        }
    }
}

/// Computes the whole-series return/volatility summary for a bar series.
pub fn summarize(
    bars: &[Bar],
    method: ReturnMethod,
    annualize: bool,
    periods_per_year: f64,
) -> Option<CalcSummary> {
    if bars.len() < 2 {
        return None;
    }
    let returns: Vec<f64> = (1..bars.len())
        .map(|i| one_return(method, bars[i - 1].close, bars[i].close))
        .collect();
    let count = returns.len();
    let mean = returns.iter().sum::<f64>() / count as f64;
    let stdev = sample_stdev(&returns);
    let min = returns.iter().copied().fold(f64::INFINITY, f64::min);
    let max = returns.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let annualized = (annualize && periods_per_year > 0.0).then(|| stdev * periods_per_year.sqrt());
    Some(CalcSummary {
        method,
        count,
        mean,
        stdev,
        realized_vol: stdev,
        annualized,
        min,
        max,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bars_from_closes(closes: &[f64]) -> Vec<Bar> {
        closes
            .iter()
            .enumerate()
            .map(|(i, &c)| Bar {
                time: i as u32 * 60,
                open: c,
                high: c,
                low: c,
                close: c,
                volume: 0,
                vwap: c,
                trades: 0,
            })
            .collect()
    }

    #[test]
    fn sma_matches_manual() {
        let bars = bars_from_closes(&[1.0, 2.0, 3.0, 4.0]);
        let out = Sma { period: 2 }.compute(&bars);
        assert_eq!(out, vec![None, Some(1.5), Some(2.5), Some(3.5)]);
    }

    #[test]
    fn returns_first_is_none() {
        let bars = bars_from_closes(&[10.0, 11.0]);
        let out = Returns {
            method: ReturnMethod::Simple,
        }
        .compute(&bars);
        assert_eq!(out[0], None);
        assert!((out[1].unwrap() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn rsi_all_gains_is_100() {
        let bars = bars_from_closes(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        let out = Rsi { period: 2 }.compute(&bars);
        assert_eq!(out[0], None);
        assert_eq!(out[1], None);
        assert_eq!(out[2], Some(100.0));
    }

    #[test]
    fn custom_fn_runs() {
        let bars = bars_from_closes(&[1.0, 2.0, 3.0]);
        let out = Calc::new(&bars)
            .with_fn("double_close", |bars| {
                bars.iter().map(|b| Some(b.close * 2.0)).collect()
            })
            .run();
        assert_eq!(out.columns.len(), 1);
        assert_eq!(out.columns[0].name, "double_close");
        assert_eq!(out.columns[0].values[2], Some(6.0));
    }

    #[test]
    fn parse_spec_classifies() {
        assert!(parse_spec("sma:20").unwrap().is_ok());
        assert!(parse_spec("ret:log").unwrap().is_ok());
        assert!(parse_spec("vol:0").unwrap().is_err());
        assert!(parse_spec("csv").is_none());
        // A Windows-style path with a drive-letter colon must not be a spec.
        assert!(parse_spec("C:/data/AAPL.fwob").is_none());
        assert!(parse_spec(r"C:\data\AAPL.fwob").is_none());
    }

    #[test]
    fn summary_reports_returns() {
        let bars = bars_from_closes(&[100.0, 101.0, 100.0, 102.0]);
        let summary = summarize(&bars, ReturnMethod::Log, true, 252.0).unwrap();
        assert_eq!(summary.count, 3);
        assert!(summary.annualized.is_some());
    }
}
