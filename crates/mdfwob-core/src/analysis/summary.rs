//! The `calc --summary` footer: per-column statistics plus a derived "character" read, rendered as
//! colored TOML.
//!
//! Driven by the indicator columns the user asked for (not a separate return computation):
//! - `[<base>.price]` — first/last/high/low of the close, drawdown from the running peak, and CAGR
//!   (calendar-time based, so it is interval-agnostic).
//! - `[<base>.<col>]` per indicator: `n / mean / min / max / last`.
//! - `[<base>.ret_*]` for a return column (`ret_log`/`ret_simple`): the return series as a fitted
//!   normal — mean, stdev, skewness, excess kurtosis, quartiles/median, Jarque–Bera — plus
//!   annualized return/vol and Sharpe.
//! - `[<base>.ret_*.character]` — heuristic word-labels (trend/volatility/skew/tails/distribution,
//!   and a vol-regime read from a `vol:N` column) derived from fixed, documented thresholds.
//!
//! Annualization uses a `periods_per_year` that defaults to the data's own bar frequency (returns ÷
//! calendar years), so it is correct for daily, weekly, or intraday bars without a fixed 252
//! assumption; an explicit override may be passed. Stats accumulate as `calc` streams rows, so
//! nothing but a return column's own values is buffered. Rendering reuses `fwob::toml`.

use std::io::{self, Write};

use fwob::toml::TomlWriter;

/// Seconds in an average year (365.25 days), for calendar-time annualization.
const YEAR_SECONDS: f64 = 365.25 * 86_400.0;

/// Identity of one indicator column, needed to summarize it.
pub struct SummaryColumn {
    pub name: String,
    pub decimals: u8,
}

/// A column is a return column iff it carries a bar-to-bar return series.
fn is_return_column(name: &str) -> bool {
    name == "ret_log" || name == "ret_simple"
}

/// Accumulates per-column statistics (and the underlying price path) as `calc` streams rows.
pub struct SummaryCollector {
    bars: usize,
    price: PriceAcc,
    columns: Vec<ColumnAcc>,
}

/// The close-price path, for the `[price]` block.
struct PriceAcc {
    seen: bool,
    first_time: u32,
    first_close: f64,
    last_time: u32,
    last_close: f64,
    high: f64,
    low: f64,
}

impl PriceAcc {
    fn new() -> Self {
        Self {
            seen: false,
            first_time: 0,
            first_close: 0.0,
            last_time: 0,
            last_close: 0.0,
            high: f64::NEG_INFINITY,
            low: f64::INFINITY,
        }
    }

    fn push(&mut self, time: u32, close: f64) {
        if !close.is_finite() {
            return;
        }
        if !self.seen {
            self.first_time = time;
            self.first_close = close;
            self.seen = true;
        }
        self.last_time = time;
        self.last_close = close;
        self.high = self.high.max(close);
        self.low = self.low.min(close);
    }

    /// Calendar years spanned by the observed closes.
    fn years(&self) -> f64 {
        (self.last_time.saturating_sub(self.first_time)) as f64 / YEAR_SECONDS
    }
}

enum ColumnAcc {
    Basic(BasicAcc),
    Return(ReturnAcc),
}

struct BasicAcc {
    name: String,
    decimals: u8,
    n: usize,
    sum: f64,
    min: f64,
    max: f64,
    last: Option<f64>,
}

struct ReturnAcc {
    name: String,
    values: Vec<f64>,
}

impl SummaryCollector {
    /// Builds a collector for `columns` (aligned to the rows later fed to [`push_row`]).
    pub fn new(columns: &[SummaryColumn]) -> Self {
        let columns = columns
            .iter()
            .map(|column| {
                if is_return_column(&column.name) {
                    ColumnAcc::Return(ReturnAcc {
                        name: column.name.clone(),
                        values: Vec::new(),
                    })
                } else {
                    ColumnAcc::Basic(BasicAcc {
                        name: column.name.clone(),
                        decimals: column.decimals,
                        n: 0,
                        sum: 0.0,
                        min: f64::INFINITY,
                        max: f64::NEG_INFINITY,
                        last: None,
                    })
                }
            })
            .collect();
        Self {
            bars: 0,
            price: PriceAcc::new(),
            columns,
        }
    }

    /// Feeds one bar: its close (for the price block) and the computed column values (aligned to the
    /// columns given at construction). Warm-up cells (`None`) and non-finite values are ignored.
    pub fn push_row(&mut self, time: u32, close: f64, values: &[Option<f64>]) {
        self.bars += 1;
        self.price.push(time, close);
        for (acc, value) in self.columns.iter_mut().zip(values) {
            let Some(value) = *value else { continue };
            if !value.is_finite() {
                continue;
            }
            match acc {
                ColumnAcc::Basic(b) => {
                    b.n += 1;
                    b.sum += value;
                    b.min = b.min.min(value);
                    b.max = b.max.max(value);
                    b.last = Some(value);
                }
                ColumnAcc::Return(r) => r.values.push(value),
            }
        }
    }

    /// The annualization factor: `periods_per_year` if given, else derived from the data's own bar
    /// frequency (`n_returns` observations over the observed calendar span).
    fn periods_per_year(&self, override_ppy: Option<f64>, n_returns: usize) -> f64 {
        if let Some(ppy) = override_ppy {
            return ppy;
        }
        let years = self.price.years();
        if years > 0.0 {
            n_returns as f64 / years
        } else {
            f64::NAN
        }
    }

    /// The volatility regime from the first `vol:N` column, comparing its latest reading to its own
    /// mean: `elevated` (> 1.25x), `calm` (< 0.8x), or `normal`.
    fn vol_regime(&self) -> Option<&'static str> {
        for acc in &self.columns {
            if let ColumnAcc::Basic(b) = acc
                && b.name.starts_with("vol_")
                && b.n > 0
            {
                let mean = b.sum / b.n as f64;
                if let (Some(last), true) = (b.last, mean > 0.0) {
                    let ratio = last / mean;
                    return Some(if ratio > 1.25 {
                        "elevated"
                    } else if ratio < 0.8 {
                        "calm"
                    } else {
                        "normal"
                    });
                }
            }
        }
        None
    }

    /// Renders the `[<base>]` summary block as TOML (colored when `color`). `base` is `"summary"`
    /// for a single symbol, or `"<SYMBOL>.summary"` when several symbols share the output.
    /// `override_ppy` overrides the data-derived annualization factor.
    pub fn render(
        &self,
        out: &mut impl Write,
        color: bool,
        base: &str,
        override_ppy: Option<f64>,
    ) -> io::Result<()> {
        let mut w = TomlWriter::new(out, color);
        w.section(base)?;
        w.kv_num("bars", self.bars)?;

        if self.price.seen {
            w.blank()?;
            w.section(&format!("{base}.price"))?;
            w.kv_float("first", self.price.first_close, 4)?;
            w.kv_float("last", self.price.last_close, 4)?;
            w.kv_float("high", self.price.high, 4)?;
            w.kv_float("low", self.price.low, 4)?;
            if self.price.high > 0.0 {
                w.kv_float(
                    "drawdown_from_peak",
                    self.price.last_close / self.price.high - 1.0,
                    4,
                )?;
            }
            let years = self.price.years();
            if years > 0.0 && self.price.first_close > 0.0 {
                let cagr = (self.price.last_close / self.price.first_close).powf(1.0 / years) - 1.0;
                w.kv_float("cagr", cagr, 4)?;
            }
        }

        for acc in &self.columns {
            w.blank()?;
            match acc {
                ColumnAcc::Basic(b) => {
                    w.section(&format!("{base}.{}", b.name))?;
                    w.kv_num("n", b.n)?;
                    if b.n > 0 {
                        let decimals = b.decimals as usize;
                        w.kv_float("mean", b.sum / b.n as f64, decimals.max(2))?;
                        w.kv_float("min", b.min, decimals)?;
                        w.kv_float("max", b.max, decimals)?;
                        if let Some(last) = b.last {
                            w.kv_float("last", last, decimals)?;
                        }
                    }
                }
                ColumnAcc::Return(r) => {
                    let col_base = format!("{base}.{}", r.name);
                    w.section(&col_base)?;
                    w.kv_num("n", r.values.len())?;
                    if let Some(stats) = ReturnStats::compute(&r.values) {
                        stats.render(&mut w)?;
                        if stats.n > 1 {
                            let ppy = self.periods_per_year(override_ppy, stats.n);
                            let annual = stats.render_annualized(&mut w, ppy)?;
                            self.render_character(&mut w, &col_base, &stats, annual)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The `[<col_base>.character]` sub-block of heuristic labels.
    fn render_character<W: Write>(
        &self,
        w: &mut TomlWriter<W>,
        col_base: &str,
        stats: &ReturnStats,
        annual: Annualized,
    ) -> io::Result<()> {
        w.blank()?;
        w.section(&format!("{col_base}.character"))?;
        w.kv_str("trend", trend_label(annual.return_))?;
        w.kv_str("volatility", volatility_label(annual.vol))?;
        w.kv_str("skew", skew_label(stats.skew))?;
        w.kv_str("tails", tails_label(stats.excess_kurtosis))?;
        w.kv_str("distribution", distribution_label(stats.jarque_bera))?;
        if let Some(regime) = self.vol_regime() {
            w.kv_str("regime", regime)?;
        }
        Ok(())
    }
}

/// Annualized return/vol pair, carried from the numeric render into the character labels.
#[derive(Clone, Copy)]
struct Annualized {
    return_: f64,
    vol: f64,
}

/// The fitted-normal statistics of a return series.
struct ReturnStats {
    n: usize,
    mean: f64,
    stdev: f64,
    skew: f64,
    excess_kurtosis: f64,
    jarque_bera: f64,
    p25: f64,
    median: f64,
    p75: f64,
    min: f64,
    max: f64,
}

impl ReturnStats {
    fn compute(values: &[f64]) -> Option<Self> {
        let n = values.len();
        if n == 0 {
            return None;
        }
        let count = n as f64;
        let mean = values.iter().sum::<f64>() / count;
        let sum_sq = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>();
        let m2 = sum_sq / count;
        let m3 = values.iter().map(|v| (v - mean).powi(3)).sum::<f64>() / count;
        let m4 = values.iter().map(|v| (v - mean).powi(4)).sum::<f64>() / count;
        // Sample standard deviation (n-1), matching the realized-vol convention.
        let stdev = if n > 1 {
            (sum_sq / (count - 1.0)).sqrt()
        } else {
            f64::NAN
        };
        let skew = if m2 > 0.0 { m3 / m2.powf(1.5) } else { 0.0 };
        let excess_kurtosis = if m2 > 0.0 { m4 / (m2 * m2) - 3.0 } else { 0.0 };
        let jarque_bera = count / 6.0 * (skew * skew + excess_kurtosis * excess_kurtosis / 4.0);
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        Some(Self {
            n,
            mean,
            stdev,
            skew,
            excess_kurtosis,
            jarque_bera,
            p25: quantile(&sorted, 0.25),
            median: quantile(&sorted, 0.50),
            p75: quantile(&sorted, 0.75),
            min: sorted[0],
            max: sorted[n - 1],
        })
    }

    fn render<W: Write>(&self, w: &mut TomlWriter<W>) -> io::Result<()> {
        w.kv_float("mean", self.mean, 6)?;
        w.kv_float("stdev", self.stdev, 6)?;
        w.kv_float("skew", self.skew, 4)?;
        w.kv_float("excess_kurtosis", self.excess_kurtosis, 4)?;
        w.kv_float("p25", self.p25, 6)?;
        w.kv_float("median", self.median, 6)?;
        w.kv_float("p75", self.p75, 6)?;
        w.kv_float("jarque_bera", self.jarque_bera, 2)?;
        w.kv_float("min", self.min, 6)?;
        w.kv_float("max", self.max, 6)?;
        Ok(())
    }

    /// Emits `annualized_return`, `annualized_vol`, and `sharpe`, returning the annualized pair for
    /// the character labels. `ppy` is the annualization factor (periods per year).
    fn render_annualized<W: Write>(
        &self,
        w: &mut TomlWriter<W>,
        ppy: f64,
    ) -> io::Result<Annualized> {
        let annual = Annualized {
            return_: self.mean * ppy,
            vol: self.stdev * ppy.sqrt(),
        };
        w.kv_float("annualized_return", annual.return_, 6)?;
        w.kv_float("annualized_vol", annual.vol, 6)?;
        if self.stdev > 0.0 {
            w.kv_float("sharpe", annual.return_ / annual.vol, 4)?;
        }
        Ok(annual)
    }
}

// ---- heuristic label thresholds (documented, fixed) ---------------------------

/// Annualized-drift sign with a ±2% deadband.
fn trend_label(annualized_return: f64) -> &'static str {
    if annualized_return > 0.02 {
        "up"
    } else if annualized_return < -0.02 {
        "down"
    } else {
        "flat"
    }
}

/// Annualized-volatility band: `low` < 15% ≤ `moderate` ≤ 25% < `high`.
fn volatility_label(annualized_vol: f64) -> &'static str {
    if !annualized_vol.is_finite() {
        "n/a"
    } else if annualized_vol < 0.15 {
        "low"
    } else if annualized_vol <= 0.25 {
        "moderate"
    } else {
        "high"
    }
}

/// `symmetric` for |skew| < 0.5, else `right`/`left`.
fn skew_label(skew: f64) -> &'static str {
    if skew.abs() < 0.5 {
        "symmetric"
    } else if skew > 0.0 {
        "right"
    } else {
        "left"
    }
}

/// `fat` for excess kurtosis > 1, `thin` for < -1, else `normal`.
fn tails_label(excess_kurtosis: f64) -> &'static str {
    if excess_kurtosis > 1.0 {
        "fat"
    } else if excess_kurtosis < -1.0 {
        "thin"
    } else {
        "normal"
    }
}

/// Jarque–Bera vs the χ²(2) 5% critical value (5.99): `non-normal` if it rejects normality.
fn distribution_label(jarque_bera: f64) -> &'static str {
    if jarque_bera > 5.99 {
        "non-normal"
    } else {
        "near-normal"
    }
}

/// Linear-interpolated quantile (numpy's default "type 7") over an ascending slice.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let pos = q * (n as f64 - 1.0);
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u32 = 86_400;

    fn render_to_string(collector: &SummaryCollector, ppy: Option<f64>) -> String {
        let mut buf = Vec::new();
        collector.render(&mut buf, false, "summary", ppy).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn basic_column_reports_n_mean_min_max_last() {
        let cols = [SummaryColumn {
            name: "sma_3".into(),
            decimals: 4,
        }];
        let mut c = SummaryCollector::new(&cols);
        c.push_row(0, 100.0, &[None]);
        c.push_row(DAY, 110.0, &[Some(10.0)]);
        c.push_row(2 * DAY, 120.0, &[Some(20.0)]);
        c.push_row(3 * DAY, 130.0, &[Some(30.0)]);
        let out = render_to_string(&c, None);
        assert!(out.contains("[summary]\nbars = 4\n"), "{out}");
        assert!(out.contains("[summary.price]"), "{out}");
        assert!(out.contains("first = 100.0000"), "{out}");
        assert!(out.contains("high = 130.0000"), "{out}");
        assert!(out.contains("[summary.sma_3]"), "{out}");
        assert!(out.contains("mean = 20.0000"), "{out}");
        assert!(out.contains("last = 30.0000"), "{out}");
    }

    #[test]
    fn return_column_reports_distribution_annualized_and_character() {
        let cols = [
            SummaryColumn {
                name: "ret_log".into(),
                decimals: 8,
            },
            SummaryColumn {
                name: "vol_2".into(),
                decimals: 8,
            },
        ];
        let mut c = SummaryCollector::new(&cols);
        let rets = [0.01, -0.02, 0.015, -0.005, 0.03, 0.008, -0.012, 0.02];
        for (i, r) in rets.iter().enumerate() {
            let t = i as u32 * DAY;
            c.push_row(t, 100.0 * (1.0 + *r), &[Some(*r), Some(0.02)]);
        }
        // Explicit ppy so annualized figures are deterministic.
        let out = render_to_string(&c, Some(252.0));
        assert!(out.contains("[summary.ret_log]"), "{out}");
        for key in [
            "mean",
            "stdev",
            "skew",
            "excess_kurtosis",
            "p25",
            "median",
            "p75",
            "jarque_bera",
            "annualized_return",
            "annualized_vol",
            "sharpe",
        ] {
            assert!(out.contains(&format!("{key} = ")), "missing {key} in {out}");
        }
        assert!(out.contains("[summary.ret_log.character]"), "{out}");
        assert!(out.contains("trend = "), "{out}");
        assert!(out.contains("tails = "), "{out}");
        assert!(out.contains("regime = "), "missing vol regime in {out}");
    }

    #[test]
    fn quartiles_interpolate() {
        let sorted = [1.0, 2.0, 3.0, 4.0];
        assert!((quantile(&sorted, 0.5) - 2.5).abs() < 1e-12);
        assert!((quantile(&sorted, 0.25) - 1.75).abs() < 1e-12);
    }

    #[test]
    fn labels_match_thresholds() {
        assert_eq!(trend_label(0.19), "up");
        assert_eq!(trend_label(0.0), "flat");
        assert_eq!(volatility_label(0.264), "high");
        assert_eq!(volatility_label(0.10), "low");
        assert_eq!(skew_label(0.07), "symmetric");
        assert_eq!(tails_label(5.43), "fat");
        assert_eq!(distribution_label(3158.0), "non-normal");
        assert_eq!(distribution_label(2.0), "near-normal");
    }
}
