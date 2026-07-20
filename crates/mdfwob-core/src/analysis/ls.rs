//! `ls`: a quick, multi-file listing of tick/bar files — one row per file with tick/bar-oriented,
//! timezone-aware columns. The market-data analog of `fwob ls`. Cheap: header + boundary keys plus
//! a small bounded leading sample per file (for bar granularity and the hours flag), never a full
//! scan. Rendered as a table, Markdown, CSV, or JSON Lines.

use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};
use fwob::Reader;
use fwob_core::Key;
use jiff::tz::TimeZone;

use crate::analysis::inspect::{classify_hours, detect_bar_granularity};
use crate::analysis::output::{Table, comma_u64, format_epoch_tz};
use crate::analysis::read::{InputKind, decode_tick, detect_kind};
use crate::analysis::schema::decode_bar;
use crate::analysis::session::Session;
use crate::analysis::stat::format_label;

/// Output format for `ls` (mirrors `fwob ls`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LsFormat {
    #[default]
    Table,
    Markdown,
    Csv,
    JsonLines,
}

impl LsFormat {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "table" => Some(Self::Table),
            "md" => Some(Self::Markdown),
            "csv" => Some(Self::Csv),
            "jsonl" => Some(Self::JsonLines),
            _ => None,
        }
    }
}

/// One listing row for a tick or bar file.
#[derive(Debug, Clone)]
pub struct LsRow {
    /// Display path (set by the caller; typically relative to the current directory).
    pub file: String,
    pub symbol: String,
    pub kind: &'static str,
    pub format: String,
    pub frame_count: u64,
    /// First/last key epochs (rendered in the chosen timezone at output time).
    pub first: Option<u32>,
    pub last: Option<u32>,
    /// Detected bar interval (`1m`, `1d`, …); `None` for tick files.
    pub granularity: Option<String>,
    pub hours: &'static str,
    pub bytes: u64,
}

fn key_epoch(key: Key) -> Option<u32> {
    match key {
        Key::U32(value) => Some(value),
        Key::I64(value) => u32::try_from(value).ok(),
        _ => None,
    }
}

/// Reads one file's listing row: metadata (symbol/kind/format/frame_count/bytes), boundary time
/// range, and — from up to `sample` leading frames — bar granularity and the trading-hours flag.
/// `file` is the display path to record. Fails if the file is not a canonical Tick/Bar file.
pub fn ls_file(file: String, path: &Path, rth: &Session, sample: u64) -> Result<LsRow> {
    let mut reader =
        Reader::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let kind = detect_kind(&reader)?;
    let symbol = {
        let title = reader.title();
        if title.is_empty() {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_owned()
        } else {
            title.to_owned()
        }
    };
    let format = format_label(path)?;
    let frame_count = reader.frame_count();
    let bytes = std::fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();
    let first = reader.first_key()?.and_then(key_epoch);
    let last = reader.last_key()?.and_then(key_epoch);

    let sample_n = frame_count.min(sample);
    let mut times: Vec<u32> = Vec::new();
    for frame in reader.frames(0..sample_n)? {
        let frame = frame?;
        let time = match kind {
            InputKind::Tick => decode_tick(frame.bytes()).time,
            InputKind::Bar => decode_bar(frame.bytes())?.time,
        };
        times.push(time);
    }
    let granularity = (kind == InputKind::Bar)
        .then(|| detect_bar_granularity(&times))
        .flatten();
    let hours = if times.is_empty() {
        "n/a"
    } else {
        classify_hours(&times, rth)
    };

    Ok(LsRow {
        file,
        symbol,
        kind: match kind {
            InputKind::Tick => "tick",
            InputKind::Bar => "bar",
        },
        format,
        frame_count,
        first,
        last,
        granularity,
        hours,
        bytes,
    })
}

const HEADERS: [&str; 10] = [
    "file",
    "symbol",
    "kind",
    "format",
    "frames",
    "first",
    "last",
    "granularity",
    "hours",
    "bytes",
];
// Right-align the numeric columns (frames, bytes); everything else left.
const ALIGNS: [bool; 10] = [
    false, false, false, false, true, false, false, false, false, true,
];

fn row_cells(row: &LsRow, tz: &TimeZone, human: bool) -> Vec<String> {
    let num = |value: u64| {
        if human {
            comma_u64(value)
        } else {
            value.to_string()
        }
    };
    let time = |epoch: Option<u32>| {
        epoch
            .map(|e| format_epoch_tz(e, tz))
            .unwrap_or_else(|| "-".to_owned())
    };
    vec![
        row.file.clone(),
        row.symbol.clone(),
        row.kind.to_owned(),
        row.format.clone(),
        num(row.frame_count),
        time(row.first),
        time(row.last),
        row.granularity.clone().unwrap_or_else(|| "-".to_owned()),
        row.hours.to_owned(),
        num(row.bytes),
    ]
}

/// Renders `rows` in the chosen `format`, with times in `tz`.
pub fn write_ls(
    rows: &[LsRow],
    format: LsFormat,
    tz: &TimeZone,
    out: &mut impl Write,
) -> io::Result<()> {
    match format {
        LsFormat::JsonLines => {
            for row in rows {
                let value = serde_json::json!({
                    "file": row.file,
                    "symbol": row.symbol,
                    "kind": row.kind,
                    "format": row.format,
                    "frame_count": row.frame_count,
                    "first": row.first.map(|e| format_epoch_tz(e, tz)),
                    "last": row.last.map(|e| format_epoch_tz(e, tz)),
                    "granularity": row.granularity,
                    "hours": row.hours,
                    "bytes": row.bytes,
                });
                serde_json::to_writer(&mut *out, &value)?;
                writeln!(out)?;
            }
            Ok(())
        }
        _ => {
            let human = matches!(format, LsFormat::Table | LsFormat::Markdown);
            let table = Table {
                headers: HEADERS.iter().map(|s| (*s).to_owned()).collect(),
                aligns: ALIGNS.to_vec(),
                rows: rows.iter().map(|r| row_cells(r, tz, human)).collect(),
            };
            let rendered = match format {
                LsFormat::Markdown => table.render_markdown(),
                LsFormat::Csv => table.render_csv(),
                _ => table.render_table(),
            };
            write!(out, "{rendered}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> LsRow {
        LsRow {
            file: "data/AAPL.fwob".into(),
            symbol: "AAPL".into(),
            kind: "bar",
            format: "fwob-v2".into(),
            frame_count: 2575,
            first: Some(1_704_205_800),
            last: Some(1_704_205_800 + 29 * 86_400),
            granularity: Some("1d".into()),
            hours: "n/a",
            bytes: 103_000,
        }
    }

    #[test]
    fn table_has_headers_and_tz_times() {
        let tz = TimeZone::get("America/New_York").unwrap();
        let mut buf = Vec::new();
        write_ls(std::slice::from_ref(&row()), LsFormat::Table, &tz, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("file"), "{s}");
        assert!(s.contains("granularity"), "{s}");
        assert!(s.contains("AAPL"), "{s}");
        assert!(s.contains("1d"), "{s}");
        assert!(s.contains("2,575"), "{s}"); // comma-grouped in table form
        assert!(s.contains("-05:00"), "{s}"); // tz-aware time
    }

    #[test]
    fn csv_is_ungrouped_and_jsonl_parses() {
        let tz = TimeZone::get("UTC").unwrap();
        let mut csv = Vec::new();
        write_ls(std::slice::from_ref(&row()), LsFormat::Csv, &tz, &mut csv).unwrap();
        let s = String::from_utf8(csv).unwrap();
        assert!(s.contains("2575"), "{s}"); // no commas in csv
        assert!(s.starts_with("file,symbol,kind"), "{s}");

        let mut jsonl = Vec::new();
        write_ls(
            std::slice::from_ref(&row()),
            LsFormat::JsonLines,
            &tz,
            &mut jsonl,
        )
        .unwrap();
        let line = String::from_utf8(jsonl).unwrap();
        let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(value["symbol"], "AAPL");
        assert_eq!(value["kind"], "bar");
        assert_eq!(value["frame_count"], 2575);
    }

    #[test]
    fn format_parse() {
        assert_eq!(LsFormat::parse("table"), Some(LsFormat::Table));
        assert_eq!(LsFormat::parse("jsonl"), Some(LsFormat::JsonLines));
        assert_eq!(LsFormat::parse("nope"), None);
    }
}
