//! The `calc --summary` footer: per-column statistics rendered as colored TOML.
//!
//! Driven by the indicator columns the user asked for (not a separate return computation): a
//! return column (`ret_log`/`ret_simple`) is summarized as a fitted normal — mean, stdev, skewness,
//! excess kurtosis, quartiles/median, and a Jarque–Bera statistic — from its buffered values;
//! every other column reports `n / mean / min / max / last`. Stats accumulate as `calc` streams
//! rows, so nothing but a return column's own values is buffered. Rendering reuses `fwob::toml` so
//! the footer matches `fwob inspect`'s colored-TOML style.

use std::io::{self, Write};

use fwob::toml::TomlWriter;

/// Identity of one indicator column, needed to summarize it.
pub struct SummaryColumn {
    pub name: String,
    pub decimals: u8,
}

/// A column is a return column iff it carries a bar-to-bar return series.
fn is_return_column(name: &str) -> bool {
    name == "ret_log" || name == "ret_simple"
}

/// Accumulates per-column statistics as `calc` streams rows.
pub struct SummaryCollector {
    bars: usize,
    columns: Vec<ColumnAcc>,
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
        Self { bars: 0, columns }
    }

    /// Feeds one bar's computed column values (aligned to the columns given at construction).
    /// Warm-up cells (`None`) and non-finite values are ignored.
    pub fn push_row(&mut self, values: &[Option<f64>]) {
        self.bars += 1;
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

    /// Renders the `[<base>]` summary block as TOML (colored when `color`). `base` is `"summary"`
    /// for a single symbol, or `"<SYMBOL>.summary"` when several symbols share the output.
    pub fn render(
        &self,
        out: &mut impl Write,
        color: bool,
        base: &str,
        annualize: bool,
        periods_per_year: f64,
    ) -> io::Result<()> {
        let mut w = TomlWriter::new(out, color);
        w.section(base)?;
        w.kv_num("bars", self.bars)?;
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
                    w.section(&format!("{base}.{}", r.name))?;
                    render_return(&mut w, &r.values, annualize, periods_per_year)?;
                }
            }
        }
        Ok(())
    }
}

/// The fitted-normal readout for a return series: N(μ, σ) parameters plus shape (skewness, excess
/// kurtosis, Jarque–Bera) and robust location/spread (quartiles).
fn render_return<W: Write>(
    w: &mut TomlWriter<W>,
    values: &[f64],
    annualize: bool,
    periods_per_year: f64,
) -> io::Result<()> {
    let n = values.len();
    w.kv_num("n", n)?;
    if n == 0 {
        return Ok(());
    }
    let count = n as f64;
    let mean = values.iter().sum::<f64>() / count;
    let m2 = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / count;
    let m3 = values.iter().map(|v| (v - mean).powi(3)).sum::<f64>() / count;
    let m4 = values.iter().map(|v| (v - mean).powi(4)).sum::<f64>() / count;
    // Sample standard deviation (n-1), matching the realized-vol convention.
    let stdev = if n > 1 {
        (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (count - 1.0)).sqrt()
    } else {
        f64::NAN
    };
    let skew = if m2 > 0.0 { m3 / m2.powf(1.5) } else { 0.0 };
    let excess_kurtosis = if m2 > 0.0 { m4 / (m2 * m2) - 3.0 } else { 0.0 };
    let jarque_bera = count / 6.0 * (skew * skew + excess_kurtosis * excess_kurtosis / 4.0);

    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);

    w.kv_float("mean", mean, 6)?;
    w.kv_float("stdev", stdev, 6)?;
    w.kv_float("skew", skew, 4)?;
    w.kv_float("excess_kurtosis", excess_kurtosis, 4)?;
    w.kv_float("p25", quantile(&sorted, 0.25), 6)?;
    w.kv_float("median", quantile(&sorted, 0.50), 6)?;
    w.kv_float("p75", quantile(&sorted, 0.75), 6)?;
    w.kv_float("jarque_bera", jarque_bera, 2)?;
    w.kv_float("min", sorted[0], 6)?;
    w.kv_float("max", sorted[n - 1], 6)?;
    if annualize && periods_per_year > 0.0 {
        w.kv_float("annualized_vol", stdev * periods_per_year.sqrt(), 6)?;
    }
    Ok(())
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

    fn render_to_string(collector: &SummaryCollector, annualize: bool) -> String {
        let mut buf = Vec::new();
        collector
            .render(&mut buf, false, "summary", annualize, 252.0)
            .unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn basic_column_reports_n_mean_min_max_last() {
        let cols = [SummaryColumn {
            name: "sma_3".into(),
            decimals: 4,
        }];
        let mut c = SummaryCollector::new(&cols);
        c.push_row(&[None]);
        c.push_row(&[Some(10.0)]);
        c.push_row(&[Some(20.0)]);
        c.push_row(&[Some(30.0)]);
        let out = render_to_string(&c, false);
        assert!(out.contains("[summary]\nbars = 4\n"), "{out}");
        assert!(out.contains("[summary.sma_3]"), "{out}");
        assert!(out.contains("n = 3"), "{out}");
        assert!(out.contains("mean = 20.0000"), "{out}");
        assert!(out.contains("min = 10.0000"), "{out}");
        assert!(out.contains("max = 30.0000"), "{out}");
        assert!(out.contains("last = 30.0000"), "{out}");
    }

    #[test]
    fn return_column_reports_distribution() {
        let cols = [SummaryColumn {
            name: "ret_log".into(),
            decimals: 8,
        }];
        let mut c = SummaryCollector::new(&cols);
        for v in [0.01, -0.02, 0.015, -0.005, 0.03] {
            c.push_row(&[Some(v)]);
        }
        let out = render_to_string(&c, true);
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
            "min",
            "max",
            "annualized_vol",
        ] {
            assert!(out.contains(&format!("{key} = ")), "missing {key} in {out}");
        }
    }

    #[test]
    fn quartiles_interpolate() {
        let sorted = [1.0, 2.0, 3.0, 4.0];
        assert!((quantile(&sorted, 0.5) - 2.5).abs() < 1e-12);
        assert!((quantile(&sorted, 0.25) - 1.75).abs() < 1e-12);
    }
}
