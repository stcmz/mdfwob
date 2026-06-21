//! Rendering of analysis results to table / markdown / CSV / JSON(L) / FWOB.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fwob::Writer;
use fwob_v2::WriterOptions;
use jiff::{Timestamp, tz::TimeZone};

use crate::analysis::calc::{CalcColumn, CalcSummary};
use crate::analysis::config::ReturnMethod;
use crate::analysis::model::Bar;
use crate::analysis::schema::{bar_schema, calc_schema, encode_bar, encode_calc_row};
use crate::analysis::stat::StatRow;

/// Output format selected by a positional token (`table` is the default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Table,
    Csv,
    Markdown,
    Json,
    JsonLines,
    Fwob,
}

impl OutputFormat {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "table" => Some(Self::Table),
            "csv" => Some(Self::Csv),
            "md" => Some(Self::Markdown),
            "json" => Some(Self::Json),
            "jsonl" => Some(Self::JsonLines),
            "fwob" => Some(Self::Fwob),
            _ => None,
        }
    }
}

/// A derived bar series for one symbol.
pub struct BarSeries {
    pub symbol: String,
    pub bars: Vec<Bar>,
}

/// A computed calc series for one symbol.
pub struct CalcSeries {
    pub symbol: String,
    pub bars: Vec<Bar>,
    pub columns: Vec<CalcColumn>,
    pub summary: Option<CalcSummary>,
}

// ---- shared formatting helpers ------------------------------------------------

fn fmt_dt(epoch: u32) -> String {
    match Timestamp::from_second(i64::from(epoch)) {
        Ok(ts) => ts
            .to_zoned(TimeZone::UTC)
            .strftime("%Y-%m-%d %H:%M:%SZ")
            .to_string(),
        Err(_) => epoch.to_string(),
    }
}

fn fmt_time(epoch: u32, human: bool) -> String {
    if human {
        fmt_dt(epoch)
    } else {
        epoch.to_string()
    }
}

fn comma_group(digits: &str) -> String {
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn comma_i64(value: i64) -> String {
    if value < 0 {
        format!("-{}", comma_group(&value.unsigned_abs().to_string()))
    } else {
        comma_group(&value.to_string())
    }
}

fn comma_u64(value: u64) -> String {
    comma_group(&value.to_string())
}

fn fmt_price(value: f64) -> String {
    if !value.is_finite() {
        return "-".into();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let rounded = format!("{:.4}", value.abs());
    let (int_part, frac_part) = rounded
        .split_once('.')
        .unwrap_or((rounded.as_str(), "0000"));
    format!("{sign}{}.{frac_part}", comma_group(int_part))
}

fn fmt_opt(value: Option<f64>) -> String {
    match value {
        Some(v) if v.is_finite() => {
            let s = format!("{v:.6}");
            let trimmed = s.trim_end_matches('0').trim_end_matches('.');
            trimmed.to_owned()
        }
        _ => "-".into(),
    }
}

fn opt_time(value: Option<u32>, human: bool) -> String {
    value
        .map(|t| fmt_time(t, human))
        .unwrap_or_else(|| "-".into())
}

// ---- generic table renderer ---------------------------------------------------

struct Table {
    headers: Vec<String>,
    aligns: Vec<bool>,
    rows: Vec<Vec<String>>,
}

impl Table {
    fn widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        widths
    }

    fn render_table(&self) -> String {
        let widths = self.widths();
        let mut out = String::new();
        push_padded(&mut out, &self.headers, &widths, &self.aligns);
        for row in &self.rows {
            push_padded(&mut out, row, &widths, &self.aligns);
        }
        out
    }

    fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("| {} |\n", self.headers.join(" | ")));
        let seps: Vec<&str> = self
            .aligns
            .iter()
            .map(|right| if *right { "---:" } else { "---" })
            .collect();
        out.push_str(&format!("| {} |\n", seps.join(" | ")));
        for row in &self.rows {
            let escaped: Vec<String> = row.iter().map(|c| c.replace('|', "\\|")).collect();
            out.push_str(&format!("| {} |\n", escaped.join(" | ")));
        }
        out
    }

    fn render_csv(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.headers.join(","));
        out.push('\n');
        for row in &self.rows {
            let cells: Vec<String> = row.iter().map(|c| csv_field(c)).collect();
            out.push_str(&cells.join(","));
            out.push('\n');
        }
        out
    }
}

fn push_padded(out: &mut String, row: &[String], widths: &[usize], aligns: &[bool]) {
    let mut line = String::new();
    for (i, cell) in row.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        let pad = widths[i].saturating_sub(cell.chars().count());
        if aligns[i] {
            line.push_str(&" ".repeat(pad));
            line.push_str(cell);
        } else {
            line.push_str(cell);
            line.push_str(&" ".repeat(pad));
        }
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

// ---- stat ---------------------------------------------------------------------

const STAT_HEADERS: [&str; 11] = [
    "symbol", "format", "ticks", "first", "last", "min", "max", "mean", "vwap", "volume", "gaps",
];
const STAT_ALIGNS: [bool; 11] = [
    false, false, true, false, false, true, true, true, true, true, true,
];

fn stat_row_cells(row: &StatRow, human: bool) -> Vec<String> {
    vec![
        row.symbol.clone(),
        row.format.clone(),
        comma_u64(row.ticks),
        opt_time(row.first, human),
        opt_time(row.last, human),
        fmt_price(row.min),
        fmt_price(row.max),
        fmt_price(row.mean),
        fmt_price(row.vwap),
        comma_i64(row.volume),
        comma_u64(row.gaps),
    ]
}

pub fn write_stat(rows: &[StatRow], format: OutputFormat, out: &mut impl Write) -> Result<()> {
    match format {
        OutputFormat::Fwob => bail!("stat does not support fwob output"),
        OutputFormat::Json | OutputFormat::JsonLines => {
            for row in rows {
                let value = serde_json::json!({
                    "symbol": row.symbol,
                    "format": row.format,
                    "ticks": row.ticks,
                    "first": row.first,
                    "last": row.last,
                    "min": finite(row.min),
                    "max": finite(row.max),
                    "mean": finite(row.mean),
                    "vwap": finite(row.vwap),
                    "volume": row.volume,
                    "gaps": row.gaps,
                });
                serde_json::to_writer(&mut *out, &value)?;
                writeln!(out)?;
            }
            Ok(())
        }
        other => {
            let human = matches!(other, OutputFormat::Table | OutputFormat::Markdown);
            let table = Table {
                headers: STAT_HEADERS.iter().map(|s| s.to_string()).collect(),
                aligns: STAT_ALIGNS.to_vec(),
                rows: rows.iter().map(|r| stat_row_cells(r, human)).collect(),
            };
            write!(out, "{}", render(&table, other))?;
            Ok(())
        }
    }
}

// ---- bars ---------------------------------------------------------------------

fn bar_headers(include_symbol: bool) -> (Vec<String>, Vec<bool>) {
    let mut headers = Vec::new();
    let mut aligns = Vec::new();
    if include_symbol {
        headers.push("symbol".to_string());
        aligns.push(false);
    }
    for (name, right) in [
        ("time", false),
        ("open", true),
        ("high", true),
        ("low", true),
        ("close", true),
        ("volume", true),
        ("vwap", true),
        ("trades", true),
    ] {
        headers.push(name.to_string());
        aligns.push(right);
    }
    (headers, aligns)
}

fn bar_cells(symbol: Option<&str>, bar: &Bar, human: bool) -> Vec<String> {
    let mut cells = Vec::new();
    if let Some(sym) = symbol {
        cells.push(sym.to_owned());
    }
    cells.extend([
        fmt_time(bar.time, human),
        fmt_price(bar.open),
        fmt_price(bar.high),
        fmt_price(bar.low),
        fmt_price(bar.close),
        comma_i64(bar.volume),
        fmt_price(bar.vwap),
        comma_u64(bar.trades),
    ]);
    cells
}

pub fn write_bars(
    series: &[BarSeries],
    format: OutputFormat,
    out_dir: Option<&Path>,
    out: &mut impl Write,
) -> Result<()> {
    if format == OutputFormat::Fwob {
        let dir = out_dir.context("bars --format fwob requires --output DIR")?;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        for s in series {
            write_bars_fwob(&s.symbol, &s.bars, dir)?;
        }
        return Ok(());
    }

    let include_symbol = series.len() > 1;
    if matches!(format, OutputFormat::Json | OutputFormat::JsonLines) {
        for s in series {
            for bar in &s.bars {
                let value = serde_json::json!({
                    "symbol": s.symbol,
                    "time": bar.time,
                    "open": bar.open,
                    "high": bar.high,
                    "low": bar.low,
                    "close": bar.close,
                    "volume": bar.volume,
                    "vwap": finite(bar.vwap),
                    "trades": bar.trades,
                });
                serde_json::to_writer(&mut *out, &value)?;
                writeln!(out)?;
            }
        }
        return Ok(());
    }

    let human = matches!(format, OutputFormat::Table | OutputFormat::Markdown);
    let (headers, aligns) = bar_headers(include_symbol);
    let mut rows = Vec::new();
    for s in series {
        let sym = include_symbol.then_some(s.symbol.as_str());
        for bar in &s.bars {
            rows.push(bar_cells(sym, bar, human));
        }
    }
    let table = Table {
        headers,
        aligns,
        rows,
    };
    write!(out, "{}", render(&table, format))?;
    Ok(())
}

fn write_bars_fwob(symbol: &str, bars: &[Bar], dir: &Path) -> Result<()> {
    let path = dir.join(format!("{symbol}.fwob"));
    let mut writer = Writer::create_v2(&path, bar_schema(), WriterOptions::new(symbol))
        .with_context(|| format!("failed to create {}", path.display()))?;
    let mut buf = Vec::with_capacity(bars.len() * 40);
    for bar in bars {
        encode_bar(bar, &mut buf);
    }
    writer.append_presorted_frames(&buf)?;
    writer.finish()?;
    Ok(())
}

// ---- calc ---------------------------------------------------------------------

pub fn write_calc(
    series: &[CalcSeries],
    format: OutputFormat,
    out_dir: Option<&Path>,
    out: &mut impl Write,
) -> Result<()> {
    if format == OutputFormat::Fwob {
        let dir = out_dir.context("calc --format fwob requires --output DIR")?;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        for s in series {
            write_calc_fwob(s, dir)?;
        }
        return Ok(());
    }

    let include_symbol = series.len() > 1;
    if matches!(format, OutputFormat::Json | OutputFormat::JsonLines) {
        for s in series {
            for (i, bar) in s.bars.iter().enumerate() {
                let mut map = serde_json::Map::new();
                map.insert("symbol".into(), s.symbol.clone().into());
                map.insert("time".into(), bar.time.into());
                map.insert("close".into(), bar.close.into());
                for col in &s.columns {
                    map.insert(col.name.clone(), json_opt(col.values[i]));
                }
                serde_json::to_writer(&mut *out, &serde_json::Value::Object(map))?;
                writeln!(out)?;
            }
        }
        return Ok(());
    }

    let human = matches!(format, OutputFormat::Table | OutputFormat::Markdown);
    // Headers
    let mut headers = Vec::new();
    let mut aligns = Vec::new();
    if include_symbol {
        headers.push("symbol".to_string());
        aligns.push(false);
    }
    headers.push("time".to_string());
    aligns.push(false);
    headers.push("close".to_string());
    aligns.push(true);
    let column_names: Vec<String> = series
        .first()
        .map(|s| s.columns.iter().map(|c| c.name.clone()).collect())
        .unwrap_or_default();
    for name in &column_names {
        headers.push(name.clone());
        aligns.push(true);
    }

    let mut rows = Vec::new();
    for s in series {
        for (i, bar) in s.bars.iter().enumerate() {
            let mut cells = Vec::new();
            if include_symbol {
                cells.push(s.symbol.clone());
            }
            cells.push(fmt_time(bar.time, human));
            cells.push(fmt_price(bar.close));
            for col in &s.columns {
                cells.push(fmt_opt(col.values[i]));
            }
            rows.push(cells);
        }
    }
    let table = Table {
        headers,
        aligns,
        rows,
    };
    write!(out, "{}", render(&table, format))?;

    // Summary footer (table/markdown only).
    if matches!(format, OutputFormat::Table | OutputFormat::Markdown) {
        for s in series {
            if let Some(summary) = &s.summary {
                writeln!(out, "{}", summary_line(&s.symbol, summary, include_symbol))?;
            }
        }
    }
    Ok(())
}

fn summary_line(symbol: &str, summary: &CalcSummary, include_symbol: bool) -> String {
    let method = match summary.method {
        ReturnMethod::Log => "log",
        ReturnMethod::Simple => "simple",
    };
    let prefix = if include_symbol {
        format!("# {symbol} summary:")
    } else {
        "# summary:".to_string()
    };
    let annualized = summary
        .annualized
        .map(|v| format!("  annualized={v:.6}"))
        .unwrap_or_default();
    format!(
        "{prefix} method={method}  n={}  mean={:.6}  realized_vol={:.6}{annualized}  min={:.6}  max={:.6}",
        summary.count, summary.mean, summary.realized_vol, summary.min, summary.max
    )
}

fn write_calc_fwob(series: &CalcSeries, dir: &Path) -> Result<()> {
    let path = dir.join(format!("{}.fwob", series.symbol));
    let names: Vec<String> = series.columns.iter().map(|c| c.name.clone()).collect();
    let schema = calc_schema(&names)?;
    let mut writer = Writer::create_v2(&path, schema, WriterOptions::new(&series.symbol))
        .with_context(|| format!("failed to create {}", path.display()))?;
    let mut buf = Vec::new();
    for (i, bar) in series.bars.iter().enumerate() {
        let values: Vec<Option<f64>> = series.columns.iter().map(|c| c.values[i]).collect();
        encode_calc_row(bar.time, bar.close, &values, &mut buf);
    }
    writer.append_presorted_frames(&buf)?;
    writer.finish()?;
    Ok(())
}

// ---- helpers ------------------------------------------------------------------

fn render(table: &Table, format: OutputFormat) -> String {
    match format {
        OutputFormat::Markdown => table.render_markdown(),
        OutputFormat::Csv => table.render_csv(),
        _ => table.render_table(),
    }
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn json_opt(value: Option<f64>) -> serde_json::Value {
    match value {
        Some(v) if v.is_finite() => serde_json::json!(v),
        _ => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comma_grouping() {
        assert_eq!(comma_u64(0), "0");
        assert_eq!(comma_u64(12), "12");
        assert_eq!(comma_u64(1234), "1,234");
        assert_eq!(comma_u64(1234567), "1,234,567");
        assert_eq!(comma_i64(-12345), "-12,345");
    }

    #[test]
    fn price_formatting() {
        assert_eq!(fmt_price(185.3), "185.3000");
        assert_eq!(fmt_price(1234.5678), "1,234.5678");
        assert_eq!(fmt_price(f64::NAN), "-");
    }

    #[test]
    fn no_trailing_whitespace_in_table() {
        let table = Table {
            headers: vec!["a".into(), "bbbb".into()],
            aligns: vec![false, true],
            rows: vec![vec!["x".into(), "1".into()]],
        };
        for line in table.render_table().lines() {
            assert_eq!(line, line.trim_end());
        }
    }
}
