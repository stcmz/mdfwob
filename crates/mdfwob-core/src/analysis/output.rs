//! Rendering of analysis results.
//!
//! `bars` and `calc` render through fwob's canonical [`FrameFormatter`], so their stdout is
//! identical to `fwob cat` of the corresponding `.fwob` (same time format, a-priori column
//! widths, fixed-point, and null handling). `stat` is a per-file summary (not frames) and keeps
//! a small bespoke renderer with tight, scan-based widths.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fwob::Writer;
use fwob::formatting::{FrameFormat, FrameFormatter};
use fwob_core::Schema;
use fwob_v2::WriterOptions;
use jiff::{Timestamp, tz::TimeZone};

use crate::analysis::model::Bar;
use crate::analysis::schema::{bar_schema, encode_bar, with_symbol_column};
use crate::analysis::stat::StatRow;

/// Maximum number of symbols that can share one interleaved stdout table (1-byte symbol index).
const MAX_SYMBOLS: usize = 256;

/// Output target selected by a positional token. Text formats reuse fwob's [`FrameFormat`]
/// (`table` default, plus `csv`, `md`, `jsonl`, `raw`, `hex`); `fwob` writes `.fwob` files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisFormat {
    Frame(FrameFormat),
    Fwob,
}

impl Default for AnalysisFormat {
    fn default() -> Self {
        Self::Frame(FrameFormat::Table)
    }
}

impl AnalysisFormat {
    pub fn parse(value: &str) -> Option<Self> {
        if value == "fwob" {
            return Some(Self::Fwob);
        }
        FrameFormat::parse(value).map(Self::Frame)
    }
}

/// A derived bar series for one symbol.
pub struct BarSeries {
    pub symbol: String,
    pub bars: Vec<Bar>,
}

// ---- bars ---------------------------------------------------------------------

pub fn write_bars(
    series: &[BarSeries],
    format: AnalysisFormat,
    out_dir: Option<&Path>,
    out: &mut impl Write,
) -> Result<()> {
    match format {
        AnalysisFormat::Fwob => {
            let dir = out_dir.context("bars --format fwob requires --output DIR")?;
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
            for s in series {
                write_bars_fwob(&s.symbol, &s.bars, dir)?;
            }
            Ok(())
        }
        AnalysisFormat::Frame(frame) => render_bars(series, frame, out),
    }
}

fn render_bars(series: &[BarSeries], format: FrameFormat, out: &mut impl Write) -> Result<()> {
    let include_symbol = series.len() > 1;
    guard_symbol_count(include_symbol, series.len())?;
    let base = bar_schema();
    let schema = if include_symbol {
        with_symbol_column(&base)
    } else {
        base
    };
    let symbols: Vec<String> = series.iter().map(|s| s.symbol.clone()).collect();
    let strings: &[String] = if include_symbol { &symbols } else { &[] };
    let mut stream = FrameStream::new(&schema, strings, format, false, out)?;
    for (index, s) in series.iter().enumerate() {
        for bar in &s.bars {
            stream.emit(index, |buf| encode_bar(bar, buf))?;
        }
    }
    Ok(())
}

/// Streaming row renderer over fwob's [`FrameFormatter`], shared by `bars` and `calc`. Writes the
/// header on construction, then one row per [`FrameStream::emit`] — the caller supplies a closure
/// that encodes that row's payload — so rows reach the terminal as they are produced rather than
/// after the whole series. Only the per-row encoding differs between callers, and it lives at the
/// call site. When `strings` is non-empty the schema must be [`with_symbol_column`]'s and each row
/// is prefixed with its symbol's index.
pub struct FrameStream<'a, W: Write> {
    formatter: FrameFormatter<'a>,
    include_symbol: bool,
    /// Flush after every row so an interactive terminal shows each row the moment it is produced.
    /// Left off when stdout is redirected, so a buffered sink keeps full throughput.
    autoflush: bool,
    frame: Vec<u8>,
    out: &'a mut W,
}

impl<'a, W: Write> FrameStream<'a, W> {
    pub fn new(
        schema: &'a Schema,
        strings: &'a [String],
        format: FrameFormat,
        autoflush: bool,
        out: &'a mut W,
    ) -> Result<Self> {
        let include_symbol = !strings.is_empty();
        let mut formatter = FrameFormatter::new(schema, strings, format);
        formatter.write_header(out)?;
        if autoflush {
            out.flush()?;
        }
        Ok(Self {
            formatter,
            include_symbol,
            autoflush,
            frame: Vec::new(),
            out,
        })
    }

    /// Renders one row. `encode` appends the frame payload (the schema fields, excluding the leading
    /// symbol-index byte). `symbol_index` is ignored unless the stream carries a symbol column.
    pub fn emit(&mut self, symbol_index: usize, encode: impl FnOnce(&mut Vec<u8>)) -> Result<()> {
        self.frame.clear();
        if self.include_symbol {
            self.frame.push(symbol_index as u8);
        }
        encode(&mut self.frame);
        self.formatter.write_frame(&mut *self.out, &self.frame)?;
        if self.autoflush {
            self.out.flush()?;
        }
        Ok(())
    }
}

pub fn write_bars_fwob(symbol: &str, bars: &[Bar], dir: &Path) -> Result<()> {
    let path = dir.join(format!("{symbol}.fwob"));
    let mut writer = FrameWriter::create(&path, bar_schema(), symbol)?;
    for bar in bars {
        writer.push(|buf| encode_bar(bar, buf))?;
    }
    writer.finish()
}

/// Streaming writer for a single `.fwob` file, shared by `bars` and `calc`. Rows are encoded into a
/// bounded buffer — the caller supplies a closure that encodes each payload — and flushed to the
/// [`Writer`] in batches, so a whole-history fine-interval conversion (e.g. `1s`, hundreds of
/// millions of rows) never materializes the entire series in memory. Feed rows with
/// [`FrameWriter::push`], then call [`FrameWriter::finish`].
pub struct FrameWriter {
    writer: Writer,
    buf: Vec<u8>,
}

impl FrameWriter {
    /// Target flush size: batching the presorted append amortizes page formation while keeping
    /// peak memory to a few megabytes regardless of the total row count.
    const FLUSH_BYTES: usize = 4 * 1024 * 1024;

    pub fn create(path: &Path, schema: Schema, title: &str) -> Result<Self> {
        let writer = Writer::create_v2(path, schema, WriterOptions::new(title))
            .with_context(|| format!("failed to create {}", path.display()))?;
        Ok(Self {
            writer,
            buf: Vec::with_capacity(Self::FLUSH_BYTES + 4096),
        })
    }

    pub fn push(&mut self, encode: impl FnOnce(&mut Vec<u8>)) -> Result<()> {
        encode(&mut self.buf);
        if self.buf.len() >= Self::FLUSH_BYTES {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if !self.buf.is_empty() {
            self.writer.append_presorted_frames(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.flush()?;
        self.writer.finish()?;
        Ok(())
    }
}

pub fn guard_symbol_count(include_symbol: bool, count: usize) -> Result<()> {
    if include_symbol && count > MAX_SYMBOLS {
        bail!(
            "cannot render {count} symbols in one table; the symbol column supports at most {MAX_SYMBOLS}"
        );
    }
    Ok(())
}

// ---- stat (bespoke summary renderer) ------------------------------------------

const STAT_HEADERS: [&str; 10] = [
    "symbol", "kind", "format", "trades", "first", "last", "min", "max", "vwap", "volume",
];
const STAT_ALIGNS: [bool; 10] = [
    false, false, false, true, false, false, true, true, true, true,
];

pub fn write_stat(rows: &[StatRow], format: FrameFormat, out: &mut impl Write) -> Result<()> {
    match format {
        FrameFormat::JsonLines => {
            for row in rows {
                let value = serde_json::json!({
                    "symbol": row.symbol,
                    "kind": row.kind,
                    "format": row.format,
                    "trades": row.trades,
                    "first": row.first,
                    "last": row.last,
                    "min": finite(row.min),
                    "max": finite(row.max),
                    "vwap": finite(row.vwap),
                    "volume": row.volume,
                });
                serde_json::to_writer(&mut *out, &value)?;
                writeln!(out)?;
            }
            Ok(())
        }
        FrameFormat::Raw | FrameFormat::Hex => {
            bail!("stat does not support {} output", format.as_str())
        }
        FrameFormat::Table | FrameFormat::Markdown | FrameFormat::Csv => {
            let human = matches!(format, FrameFormat::Table | FrameFormat::Markdown);
            let table = Table {
                headers: STAT_HEADERS.iter().map(|s| s.to_string()).collect(),
                aligns: STAT_ALIGNS.to_vec(),
                rows: rows.iter().map(|r| stat_row_cells(r, human)).collect(),
            };
            let rendered = match format {
                FrameFormat::Markdown => table.render_markdown(),
                FrameFormat::Csv => table.render_csv(),
                _ => table.render_table(),
            };
            write!(out, "{rendered}")?;
            Ok(())
        }
    }
}

fn stat_row_cells(row: &StatRow, human: bool) -> Vec<String> {
    vec![
        row.symbol.clone(),
        row.kind.to_owned(),
        row.format.clone(),
        comma_u64(row.trades),
        opt_time(row.first, human),
        opt_time(row.last, human),
        fmt_price(row.min),
        fmt_price(row.max),
        fmt_price(row.vwap),
        comma_i64(row.volume),
    ]
}

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

// ---- shared formatting helpers ------------------------------------------------

fn fmt_dt(epoch: u32) -> String {
    match Timestamp::from_second(i64::from(epoch)) {
        Ok(ts) => ts
            .to_zoned(TimeZone::UTC)
            .strftime("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
        Err(_) => epoch.to_string(),
    }
}

/// Formats a UTC epoch second as an RFC3339 timestamp in `tz`, carrying a numeric offset (e.g.
/// `2015-01-02T09:30:00-05:00`). Used by `inspect`/`verify` so timestamps honor the exchange
/// timezone instead of bare UTC.
pub fn format_epoch_tz(epoch: u32, tz: &TimeZone) -> String {
    match Timestamp::from_second(i64::from(epoch)) {
        Ok(ts) => ts
            .to_zoned(tz.clone())
            .strftime("%Y-%m-%dT%H:%M:%S%:z")
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

fn opt_time(value: Option<u32>, human: bool) -> String {
    value
        .map(|t| fmt_time(t, human))
        .unwrap_or_else(|| "-".into())
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

pub(crate) fn comma_i64(value: i64) -> String {
    if value < 0 {
        format!("-{}", comma_group(&value.unsigned_abs().to_string()))
    } else {
        comma_group(&value.to_string())
    }
}

pub(crate) fn comma_u64(value: u64) -> String {
    comma_group(&value.to_string())
}

pub(crate) fn fmt_price(value: f64) -> String {
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

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_writer_streams_across_flush_boundary() {
        use crate::analysis::read::read_bars;
        use crate::analysis::schema::BAR_FRAME_LEN;

        // More bars than one flush buffer holds (FLUSH_BYTES / BAR_FRAME_LEN) so at least one
        // mid-stream batch is appended before finish, exercising the incremental path.
        let count = (FrameWriter::FLUSH_BYTES / BAR_FRAME_LEN as usize) + 5_000;
        let dir = std::env::temp_dir().join(format!("mdfwob-framewriter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut writer = FrameWriter::create(&dir.join("TEST.fwob"), bar_schema(), "TEST").unwrap();
        for i in 0..count {
            let t = 1_704_067_200 + i as u32; // ascending 1s bars
            let price = 100.0 + (i as f64) * 0.0001;
            let bar = Bar {
                time: t,
                open: price,
                high: price,
                low: price,
                close: price,
                volume: 10,
                vwap: price,
                trades: 1,
            };
            writer.push(|buf| encode_bar(&bar, buf)).unwrap();
        }
        writer.finish().unwrap();

        let (symbol, bars) = read_bars(&dir.join("TEST.fwob")).unwrap();
        assert_eq!(symbol, "TEST");
        assert_eq!(bars.len(), count);
        assert_eq!(bars[0].time, 1_704_067_200);
        assert_eq!(bars[count - 1].time, 1_704_067_200 + (count as u32 - 1));
        let _ = std::fs::remove_dir_all(dir);
    }

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
    fn time_is_rfc3339() {
        assert_eq!(fmt_dt(1_522_742_400), "2018-04-03T08:00:00Z");
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
