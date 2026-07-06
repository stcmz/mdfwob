//! Per-bar derived series: the [`Indicator`] trait, built-ins, custom functions,
//! and the [`Calc`] pipeline.
//!
//! Built-in specs are `sma:N`, `ema:N`, `dema:N`, `vsma:N`, `vema:N`, `vdema:N`,
//! `rsi:N`, `ret:log`, `ret:simple`, `vol:N`. API users can also register arbitrary
//! closures with [`Calc::with_fn`].

use anyhow::{Result, bail};

use crate::analysis::config::ReturnMethod;
use crate::analysis::model::Bar;

/// A per-bar derived column. `compute` returns one value per input bar
/// (`None` during the indicator's warm-up).
pub trait Indicator {
    fn name(&self) -> String;
    fn compute(&self, bars: &[Bar]) -> Vec<Option<f64>>;
    /// Fixed-point decimal precision used when storing/rendering this column. Price-level
    /// indicators keep 4 (fits i32 up to ±214,748); small-magnitude ones use 8.
    fn decimals(&self) -> u8 {
        4
    }
}

fn closes(bars: &[Bar]) -> Vec<f64> {
    bars.iter().map(|b| b.close).collect()
}

fn volumes(bars: &[Bar]) -> Vec<f64> {
    bars.iter().map(|b| b.volume as f64).collect()
}

/// Simple moving average over `period` bars of the values produced by `select` (close or volume).
fn simple_ma(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; values.len()];
    if period == 0 {
        return out;
    }
    let mut sum = 0.0;
    for i in 0..values.len() {
        sum += values[i];
        if i >= period {
            sum -= values[i - period];
        }
        if i + 1 >= period {
            out[i] = Some(sum / period as f64);
        }
    }
    out
}

/// Exponential moving average of `values`, seeded with the `period`-bar SMA at index `period-1`.
fn exp_ma(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; values.len()];
    if period == 0 || values.len() < period {
        return out;
    }
    let alpha = 2.0 / (period as f64 + 1.0);
    let mut ema = values[..period].iter().sum::<f64>() / period as f64;
    out[period - 1] = Some(ema);
    for i in period..values.len() {
        ema = alpha * values[i] + (1.0 - alpha) * ema;
        out[i] = Some(ema);
    }
    out
}

/// Double exponential moving average (Mulloy): `DEMA = 2*EMA - EMA(EMA)`, which cancels much of a
/// plain EMA's lag. Warms up over `2*(period-1)` bars (the inner EMA feeds the outer one).
fn double_ema(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let n = values.len();
    let mut out = vec![None; n];
    if period == 0 {
        return out;
    }
    let e1 = exp_ma(values, period);
    // The inner EMA is `Some` from `start` onward with no gaps; feed that tail to the outer EMA.
    let Some(start) = e1.iter().position(Option::is_some) else {
        return out;
    };
    let e1_tail: Vec<f64> = e1[start..].iter().map(|v| v.unwrap()).collect();
    let e2 = exp_ma(&e1_tail, period);
    for i in start..n {
        if let (Some(a), Some(b)) = (e1[i], e2[i - start]) {
            out[i] = Some(2.0 * a - b);
        }
    }
    out
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
        simple_ma(&closes(bars), self.period)
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
        exp_ma(&closes(bars), self.period)
    }
}

/// Double exponential moving average of close (lower lag than `ema`).
pub struct Dema {
    pub period: usize,
}

impl Indicator for Dema {
    fn name(&self) -> String {
        format!("dema_{}", self.period)
    }

    fn compute(&self, bars: &[Bar]) -> Vec<Option<f64>> {
        double_ema(&closes(bars), self.period)
    }
}

/// Simple moving average of volume over `period` bars.
pub struct VolumeSma {
    pub period: usize,
}

impl Indicator for VolumeSma {
    fn name(&self) -> String {
        format!("vsma_{}", self.period)
    }

    fn compute(&self, bars: &[Bar]) -> Vec<Option<f64>> {
        simple_ma(&volumes(bars), self.period)
    }

    fn decimals(&self) -> u8 {
        0
    }
}

/// Exponential moving average of volume, seeded with the `period`-SMA.
pub struct VolumeEma {
    pub period: usize,
}

impl Indicator for VolumeEma {
    fn name(&self) -> String {
        format!("vema_{}", self.period)
    }

    fn compute(&self, bars: &[Bar]) -> Vec<Option<f64>> {
        exp_ma(&volumes(bars), self.period)
    }

    fn decimals(&self) -> u8 {
        0
    }
}

/// Double exponential moving average of volume (lower lag than `vema`).
pub struct VolumeDema {
    pub period: usize,
}

impl Indicator for VolumeDema {
    fn name(&self) -> String {
        format!("vdema_{}", self.period)
    }

    fn compute(&self, bars: &[Bar]) -> Vec<Option<f64>> {
        double_ema(&volumes(bars), self.period)
    }

    fn decimals(&self) -> u8 {
        0
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

    fn decimals(&self) -> u8 {
        8
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

    fn decimals(&self) -> u8 {
        8
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
    decimals: u8,
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

    fn decimals(&self) -> u8 {
        self.decimals
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
    if !matches!(
        kind,
        "sma" | "ema" | "dema" | "vsma" | "vema" | "vdema" | "rsi" | "vol" | "ret"
    ) {
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
            "dema" => Ok(Box::new(Dema { period: period()? })),
            "vsma" => Ok(Box::new(VolumeSma { period: period()? })),
            "vema" => Ok(Box::new(VolumeEma { period: period()? })),
            "vdema" => Ok(Box::new(VolumeDema { period: period()? })),
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
    /// Fixed-point precision used to store/render this column.
    pub decimals: u8,
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

    /// Adds a custom column computed by a user-supplied closure (8-decimal precision).
    pub fn with_fn<F>(self, name: impl Into<String>, func: F) -> Self
    where
        F: Fn(&[Bar]) -> Vec<Option<f64>> + 'a,
    {
        self.with_fn_decimals(name, 8, func)
    }

    /// Adds a custom column with an explicit fixed-point precision.
    pub fn with_fn_decimals<F>(mut self, name: impl Into<String>, decimals: u8, func: F) -> Self
    where
        F: Fn(&[Bar]) -> Vec<Option<f64>> + 'a,
    {
        self.indicators.push(Box::new(CustomIndicator {
            name: name.into(),
            func,
            decimals,
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
                decimals: indicator.decimals(),
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
    fn volume_moving_averages_run_on_volume() {
        let bars: Vec<Bar> = [10i64, 20, 30, 40]
            .into_iter()
            .enumerate()
            .map(|(i, v)| Bar {
                time: i as u32 * 60,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: v,
                vwap: 1.0,
                trades: 0,
            })
            .collect();
        let sma = VolumeSma { period: 2 };
        assert_eq!(sma.name(), "vsma_2");
        assert_eq!(sma.decimals(), 0);
        assert_eq!(
            sma.compute(&bars),
            vec![None, Some(15.0), Some(25.0), Some(35.0)]
        );
        // EMA of volume warms up to the 2-bar SMA at index 1, then tracks.
        let ema = VolumeEma { period: 2 };
        assert_eq!(ema.name(), "vema_2");
        let out = ema.compute(&bars);
        assert_eq!(out[0], None);
        assert_eq!(out[1], Some(15.0));
        assert!(out[3].unwrap() > out[1].unwrap());
    }

    #[test]
    fn dema_reduces_lag_versus_ema() {
        // On a steady ramp, DEMA tracks closer to the latest value than EMA (less lag).
        let bars = bars_from_closes(&(0..30).map(|i| 100.0 + i as f64).collect::<Vec<_>>());
        assert_eq!(Dema { period: 5 }.name(), "dema_5");
        let ema = Ema { period: 5 }.compute(&bars);
        let dema = Dema { period: 5 }.compute(&bars);
        let last = bars.len() - 1;
        let price = bars[last].close;
        let ema_lag = price - ema[last].unwrap();
        let dema_lag = price - dema[last].unwrap();
        assert!(
            dema_lag.abs() < ema_lag.abs(),
            "expected DEMA to lag less: dema_lag={dema_lag}, ema_lag={ema_lag}"
        );
        // Warm-up: DEMA needs 2*(N-1) bars before its first value.
        assert!(dema[2 * (5 - 1) - 1].is_none());
        assert!(dema[2 * (5 - 1)].is_some());
    }

    #[test]
    fn parse_spec_classifies() {
        assert!(parse_spec("sma:20").unwrap().is_ok());
        assert!(parse_spec("dema:20").unwrap().is_ok());
        assert!(parse_spec("vsma:20").unwrap().is_ok());
        assert!(parse_spec("vema:20").unwrap().is_ok());
        assert!(parse_spec("vdema:20").unwrap().is_ok());
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
