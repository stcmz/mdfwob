//! The `inspect` overview: a bounded, no-full-scan read of one tick or bar file into an
//! [`Inspection`], plus the pure helpers it is built from (bar-granularity and trading-hours
//! detection over a frame sample, schema-field labels, and a timezone- and semantic-aware frame
//! preview), which stay public so they are unit-testable without a file.
//!
//! [`inspect_file`] does the assembly once so every front end renders the *same* overview — the
//! CLI as colored TOML, the MCP server as JSON — instead of each re-deriving it and drifting.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use fwob::{FormatVersion, Reader};
use fwob_core::{FieldSemantic, FieldType, Key, Schema, TimestampUnit};
use jiff::{Timestamp, civil::Date, tz::TimeZone};

use crate::analysis::model::{Bar, Tick};
use crate::analysis::output::{comma_i64, comma_u64, fmt_price, format_epoch_tz};
use crate::analysis::read::{InputKind, decode_tick, detect_kind, key_epoch};
use crate::analysis::schema::decode_bar;
use crate::analysis::session::Session;

const DAY: u32 = 86_400;

/// Everything the `inspect` overview reports about one tick or bar file.
///
/// Deliberately holds domain values (a [`Schema`], epoch seconds, an [`InputKind`]) rather than
/// display strings, so each front end formats them its own way; the one exception is `preview`,
/// which is a rendered table because both front ends want it verbatim.
#[derive(Debug, Clone)]
pub struct Inspection {
    pub kind: InputKind,
    pub format: FormatVersion,
    /// The header title verbatim — empty when the file carries none. Reported as-is by
    /// `mdfwob inspect`'s `title`; prefer [`symbol`](Self::symbol) when you need a name to show.
    pub title: String,
    /// The header title, or the file stem when the header carries none. Matches what `ls` reports.
    pub symbol: String,
    pub schema: Schema,
    pub frame_count: u64,
    pub physical_bytes: u64,
    /// Boundary keys as epoch seconds.
    pub first: Option<u32>,
    pub last: Option<u32>,
    /// Detected bar interval (`1m`, `1d`, …). `None` for tick files or too few bars to tell.
    pub granularity: Option<String>,
    /// `"rth"` / `"extended"`; `None` when the file has no frames to classify.
    pub hours: Option<&'static str>,
    /// Head/tail sample, already rendered in the requested timezone with field semantics honored.
    pub preview: String,
}

impl Inspection {
    /// The on-disk format as it is reported to users: `"fwob-v1"` or `"fwob-v2"`.
    pub fn format_label(&self) -> &'static str {
        match self.format {
            FormatVersion::V1 => "fwob-v1",
            FormatVersion::V2 => "fwob-v2",
        }
    }

    /// `"tick"` or `"bar"`.
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            InputKind::Tick => "tick",
            InputKind::Bar => "bar",
        }
    }
}

/// Reads one tick or bar file's overview: header metadata, boundary keys, the bar granularity and
/// a decoded preview (rendered in `tz`) from up to `sample` frames at each end, and the
/// trading-hours classification from a few days' opening frames (see [`classify_file_hours`]).
///
/// Bounded by construction: only the header, the two boundary keys, the sampled windows, and a
/// handful of seeks are read, never the whole file. `rth` supplies the regular-hours window used
/// to classify `hours`. Fails if the file is not a canonical Tick/Bar file.
pub fn inspect_file(path: &Path, rth: &Session, tz: &TimeZone, sample: u64) -> Result<Inspection> {
    let mut reader =
        Reader::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let kind =
        detect_kind(&reader).with_context(|| format!("failed to inspect {}", path.display()))?;
    let format = reader.format_version();
    let title = reader.title().to_owned();
    let symbol = if title.is_empty() {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_owned()
    } else {
        title.clone()
    };
    let schema = reader.schema().clone();
    let frame_count = reader.frame_count();
    let physical_bytes = std::fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();
    let first = reader.first_key()?.and_then(key_epoch);
    let last = reader.last_key()?.and_then(key_epoch);

    // The same windows `ls` samples (shared `sample_windows`), so the two agree on granularity and
    // hours. The leading window also feeds the preview's head, the trailing window its tail.
    let (lead, tail) = sample_windows(frame_count, sample);
    let mut times: Vec<u32> = Vec::new();
    let (mut lead_ticks, mut tail_ticks): (Vec<Tick>, Vec<Tick>) = (Vec::new(), Vec::new());
    let (mut lead_bars, mut tail_bars): (Vec<Bar>, Vec<Bar>) = (Vec::new(), Vec::new());
    for (range, is_tail) in std::iter::once((lead, false)).chain(tail.map(|t| (t, true))) {
        for frame in reader.frames(range)? {
            let frame = frame?;
            match kind {
                InputKind::Tick => {
                    let tick = decode_tick(frame.bytes());
                    times.push(tick.time);
                    if is_tail {
                        &mut tail_ticks
                    } else {
                        &mut lead_ticks
                    }
                    .push(tick);
                }
                InputKind::Bar => {
                    let bar = decode_bar(frame.bytes())?;
                    times.push(bar.time);
                    if is_tail {
                        &mut tail_bars
                    } else {
                        &mut lead_bars
                    }
                    .push(bar);
                }
            }
        }
    }

    let hours = classify_file_hours(&mut reader, kind, first, last, &times, rth)?;

    let preview = match kind {
        InputKind::Tick => preview_ticks(&preview_rows(frame_count, &lead_ticks, &tail_ticks), tz),
        InputKind::Bar => preview_bars(&preview_rows(frame_count, &lead_bars, &tail_bars), tz),
    };

    Ok(Inspection {
        kind,
        format,
        title,
        symbol,
        schema,
        frame_count,
        physical_bytes,
        first,
        last,
        granularity: (kind == InputKind::Bar)
            .then(|| detect_bar_granularity(&times))
            .flatten(),
        hours,
        preview,
    })
}

/// The leading and (optional) trailing frame-index windows to sample from a `frame_count`-frame
/// file, each up to `per_end` frames and never overlapping. `inspect` and `ls` both sample these
/// exact windows so their granularity and hours classification are identical: the trailing window
/// is `None` only when the leading window already reaches the end of the file.
pub fn sample_windows(frame_count: u64, per_end: u64) -> (Range<u64>, Option<Range<u64>>) {
    let lead_n = frame_count.min(per_end);
    let tail = (frame_count > lead_n).then(|| {
        let start = frame_count.saturating_sub(per_end).max(lead_n);
        start..frame_count
    });
    (0..lead_n, tail)
}

/// Detects a bar series' interval label (`1m`, `30m`, `1h`, `1d`, `1w`, `1mo`, `1y`, …) from the
/// minimum positive gap between consecutive bar times. Intraday gaps map exactly; day-and-larger
/// gaps are matched with tolerance (DST makes a "1 day" gap 23–25h, weekends leave the *minimum*
/// gap at ~1 day). Returns `None` for fewer than two bars or no positive gap.
pub fn detect_bar_granularity(times: &[u32]) -> Option<String> {
    min_positive_gap(times).map(granularity_label)
}

fn granularity_label(min_delta: u32) -> String {
    if min_delta < DAY {
        if min_delta.is_multiple_of(3_600) {
            format!("{}h", min_delta / 3_600)
        } else if min_delta.is_multiple_of(60) {
            format!("{}m", min_delta / 60)
        } else {
            format!("{min_delta}s")
        }
    } else if (82_800..=90_000).contains(&min_delta) {
        "1d".to_owned()
    } else if (7 * DAY - 7_200..=7 * DAY + 7_200).contains(&min_delta) {
        "1w".to_owned()
    } else if (28 * DAY..=31 * DAY).contains(&min_delta) {
        "1mo".to_owned()
    } else if (365 * DAY..=366 * DAY).contains(&min_delta) {
        "1y".to_owned()
    } else {
        // A clean multiple of days (e.g. a 2-day bar) or anything else: round to whole days.
        format!("{}d", (min_delta + DAY / 2) / DAY)
    }
}

fn minute_of_day(epoch: u32, tz: &TimeZone) -> Option<i32> {
    let zoned = Timestamp::from_second(i64::from(epoch))
        .ok()?
        .to_zoned(tz.clone());
    Some(i32::from(zoned.hour()) * 60 + i32::from(zoned.minute()))
}

/// Classifies whether a sample's timestamps fall entirely inside regular trading hours.
///
/// Sees only the frames it is handed, so on its own it judges a file by wherever those happen to
/// sit. [`classify_file_hours`] uses it only as the fallback for a file with no second trading day
/// to probe.
///
/// - `"rth"` — every sampled frame is within `rth`'s window and the sample spans more than one
///   time-of-day (so the window is actually observable).
/// - `"extended"` — at least one sampled frame is outside the RTH window (pre/after-market).
/// - `"n/a"` — every frame shares a single time-of-day (e.g. daily bars anchored to the session
///   open), which cannot reveal which hours the underlying trades covered.
pub fn classify_hours(times: &[u32], rth: &Session) -> &'static str {
    let tz = rth.time_zone();
    let mut first_minute: Option<i32> = None;
    let mut multiple_minutes = false;
    let mut any_outside = false;
    for &time in times {
        if let Some(minute) = minute_of_day(time, &tz) {
            match first_minute {
                Some(m) if m != minute => multiple_minutes = true,
                None => first_minute = Some(minute),
                _ => {}
            }
        }
        if !rth.contains(time) {
            any_outside = true;
        }
    }
    if !multiple_minutes {
        "n/a"
    } else if any_outside {
        "extended"
    } else {
        "rth"
    }
}

/// Distinct trading days whose opening frames [`classify_file_hours`] reads at each end of a file.
pub const HOURS_PROBE_DAYS: usize = 5;
/// Frames read at each probed day's open.
pub const HOURS_PROBE_FRAMES: u64 = 5;

/// Classifies which trading hours a whole file covers: `"rth"`, `"extended"`, or `"n/a"`.
///
/// A day's *opening* prints are what tell the two recordings apart: extended-hours data opens in
/// the pre-market (04:00 in New York), regular-hours data at the bell. Frames at fixed positions
/// cannot see that — for a tick file, the first and last thousand frames span a few minutes either
/// side of wherever the data happens to start and stop, which on an IPO day or a download that
/// ended mid-session is squarely inside RTH. So this seeks to exchange-local midnight on up to
/// [`HOURS_PROBE_DAYS`] trading days at each end and reads the first [`HOURS_PROBE_FRAMES`] frames
/// after each; any of those outside `rth` makes the file `"extended"`.
///
/// The file's first day is never probed, since the data may begin partway through it. The last
/// day is, because a download is truncated at its end, not its start. Each probe continues from
/// the day it actually landed on, so weekends, holidays, and gaps cost no extra seeks and every
/// probe is a day with data.
///
/// `sample_times` are frames from the contiguous head/tail windows. They decide `"n/a"` for bar
/// files at daily or coarser granularity, whose times are bucket anchors rather than trade times,
/// and are the fallback for a file too short to contain a second trading day. Returns `None` for
/// an empty file.
pub fn classify_file_hours(
    reader: &mut Reader,
    kind: InputKind,
    first: Option<u32>,
    last: Option<u32>,
    sample_times: &[u32],
    rth: &Session,
) -> Result<Option<&'static str>> {
    let (Some(first), Some(last)) = (first, last) else {
        return Ok(None);
    };
    if sample_times.is_empty() {
        return Ok(None);
    }
    if kind == InputKind::Bar && min_positive_gap(sample_times).is_none_or(|gap| gap >= DAY) {
        return Ok(Some("n/a"));
    }
    let outside = |time: u32| !rth.contains(time);
    // One open outside RTH settles `extended`, so the walk stops there: a typical extended-hours
    // file costs a single probe, and only files that really are `rth` pay for every day.
    let opens = day_open_times(
        reader,
        kind,
        first,
        last,
        &rth.time_zone(),
        HOURS_PROBE_DAYS,
        HOURS_PROBE_FRAMES,
        outside,
    )?;
    if opens.is_empty() {
        return Ok(Some(classify_hours(sample_times, rth)));
    }
    Ok(Some(if opens.iter().any(|&time| outside(time)) {
        "extended"
    } else {
        "rth"
    }))
}

/// The times of the first `per_day` frames after exchange-local midnight, on up to `days` trading
/// days at each end of a file whose keys run from `first` to `last`. The file's first day is
/// skipped, and a short file whose head and tail walks meet samples each day once. The walk ends
/// early, after the first day with a frame for which `stop` holds. See [`classify_file_hours`].
#[allow(clippy::too_many_arguments)]
pub fn day_open_times(
    reader: &mut Reader,
    kind: InputKind,
    first: u32,
    last: u32,
    tz: &TimeZone,
    days: usize,
    per_day: u64,
    stop: impl Fn(u32) -> bool,
) -> Result<Vec<u32>> {
    let count = reader.frame_count();
    let mut probed = BTreeSet::new();
    let mut opens = Vec::new();

    // Head: forward from the day after the first.
    let mut date = local_date(first, tz).and_then(|day| day.tomorrow().ok());
    while probed.len() < days
        && let Some(day) = date
    {
        let Some(midnight) = local_midnight(day, tz) else {
            break;
        };
        if midnight > last {
            break;
        }
        // `midnight <= last`, so a frame keyed at or after it exists.
        let index = reader.lower_bound(Key::U32(midnight))?;
        let times = frame_times(
            reader,
            kind,
            index..count.min(index.saturating_add(per_day)),
        )?;
        let Some(&open) = times.first() else {
            break;
        };
        probed.insert(index);
        let decided = times.iter().any(|&time| stop(time));
        opens.extend(times);
        if decided {
            return Ok(opens);
        }
        // Continue from the day actually landed on, which skips any weekend or gap in one step.
        date = local_date(open, tz).and_then(|day| day.tomorrow().ok());
    }

    // Tail: backward from the last day, stopping before the file's first day.
    let head_probes = probed.len();
    let mut date = local_date(last, tz);
    while probed.len() - head_probes < days
        && let Some(day) = date
    {
        let Some(midnight) = local_midnight(day, tz) else {
            break;
        };
        if midnight <= first {
            break;
        }
        // `first < midnight <= last`, so `1 <= index < count`.
        let index = reader.lower_bound(Key::U32(midnight))?;
        if !probed.insert(index) {
            // The head walk already sampled this day, and every day before it.
            break;
        }
        // One read covers this day's opening frames and, just before them, the last frame of the
        // previous day with data — which is where the walk goes next.
        let times = frame_times(
            reader,
            kind,
            index - 1..count.min(index.saturating_add(per_day)),
        )?;
        let Some((&previous, day_open)) = times.split_first() else {
            break;
        };
        opens.extend_from_slice(day_open);
        if day_open.iter().any(|&time| stop(time)) {
            return Ok(opens);
        }
        date = local_date(previous, tz);
    }
    Ok(opens)
}

/// The `time` of each frame in `range`.
fn frame_times(reader: &mut Reader, kind: InputKind, range: Range<u64>) -> Result<Vec<u32>> {
    let mut times = Vec::new();
    for frame in reader.frames(range)? {
        let frame = frame?;
        times.push(match kind {
            InputKind::Tick => decode_tick(frame.bytes()).time,
            InputKind::Bar => decode_bar(frame.bytes())?.time,
        });
    }
    Ok(times)
}

/// The calendar date `epoch` falls on in `tz`.
fn local_date(epoch: u32, tz: &TimeZone) -> Option<Date> {
    Some(
        Timestamp::from_second(i64::from(epoch))
            .ok()?
            .to_zoned(tz.clone())
            .date(),
    )
}

/// The instant `date` begins in `tz`. On the rare zone whose DST shift skips midnight, that is the
/// first valid instant of the day.
fn local_midnight(date: Date, tz: &TimeZone) -> Option<u32> {
    u32::try_from(date.to_zoned(tz.clone()).ok()?.timestamp().as_second()).ok()
}

/// The smallest positive gap between consecutive times, if any.
fn min_positive_gap(times: &[u32]) -> Option<u32> {
    times
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]))
        .filter(|&delta| delta > 0)
        .min()
}

/// The TOML label for a field's storage type (mirrors `fwob inspect`).
pub fn field_type_label(field_type: FieldType) -> &'static str {
    match field_type {
        FieldType::SignedInteger => "signed-integer",
        FieldType::UnsignedInteger => "unsigned-integer",
        FieldType::FloatingPoint => "floating-point",
        FieldType::Utf8String => "utf8-string",
        FieldType::StringTableIndex => "string-table-index",
    }
}

/// The TOML label for a field's semantic (mirrors `fwob inspect`).
pub fn field_semantic_label(semantic: FieldSemantic) -> String {
    match semantic {
        FieldSemantic::None => "none".to_owned(),
        FieldSemantic::UnixTimestamp(TimestampUnit::Seconds) => "unix-seconds".to_owned(),
        FieldSemantic::UnixTimestamp(TimestampUnit::Milliseconds) => "unix-milliseconds".to_owned(),
        FieldSemantic::UnixTimestamp(TimestampUnit::Microseconds) => "unix-microseconds".to_owned(),
        FieldSemantic::UnixTimestamp(TimestampUnit::Nanoseconds) => "unix-nanoseconds".to_owned(),
        FieldSemantic::FixedPoint(points) => format!("fixed-{points}"),
        FieldSemantic::Percentage(points) => format!("percent-{points}"),
    }
}

/// How many frames the preview shows from the head and (separately) from the tail — matching
/// `fwob inspect`'s constant. A file with more than `2 * FRAME_PREVIEW_COUNT` frames shows the
/// first and last `FRAME_PREVIEW_COUNT` with an ellipsis between; a smaller file shows every frame.
pub const FRAME_PREVIEW_COUNT: usize = 3;

const TICK_HEADERS: [&str; 3] = ["time", "price", "size"];
const TICK_ALIGNS: [bool; 3] = [false, true, true];
const BAR_HEADERS: [&str; 8] = [
    "time", "open", "high", "low", "close", "volume", "vwap", "trades",
];
const BAR_ALIGNS: [bool; 8] = [false, true, true, true, true, true, true, true];

/// Selects the preview rows (head + optional ellipsis + tail) from decoded leading (`head`) and
/// trailing (`tail`) windows of a `frame_count`-frame file, mirroring `fwob`'s `preview_indices`:
/// all frames when `frame_count <= 2 * FRAME_PREVIEW_COUNT`, otherwise the first and last
/// `FRAME_PREVIEW_COUNT` with a `None` (ellipsis) between. `tail` may be empty when the file is
/// small enough that `head` already reaches the end; then the tail is taken from `head`.
pub fn preview_rows<T: Copy>(frame_count: u64, head: &[T], tail: &[T]) -> Vec<Option<T>> {
    let per_side = FRAME_PREVIEW_COUNT;
    let count = frame_count as usize;
    if count <= per_side * 2 {
        return head.iter().take(count).copied().map(Some).collect();
    }
    let mut out = Vec::with_capacity(per_side * 2 + 1);
    out.extend(head.iter().take(per_side).copied().map(Some));
    out.push(None);
    let tail_src = if tail.is_empty() { head } else { tail };
    let start = tail_src.len().saturating_sub(per_side);
    out.extend(tail_src[start..].iter().copied().map(Some));
    out
}

fn tick_cells(tick: &Tick, tz: &TimeZone) -> Vec<String> {
    vec![
        format_epoch_tz(tick.time, tz),
        fmt_price(tick.price),
        comma_i64(i64::from(tick.size)),
    ]
}

fn bar_cells(bar: &Bar, tz: &TimeZone) -> Vec<String> {
    vec![
        format_epoch_tz(bar.time, tz),
        fmt_price(bar.open),
        fmt_price(bar.high),
        fmt_price(bar.low),
        fmt_price(bar.close),
        comma_i64(bar.volume),
        fmt_price(bar.vwap),
        comma_u64(bar.trades),
    ]
}

/// Renders preview `rows` (from [`preview_rows`]) as an aligned tick table; `None` is an ellipsis
/// row. Timestamps render in `tz`, prices at 4 decimals.
pub fn preview_ticks(rows: &[Option<Tick>], tz: &TimeZone) -> String {
    let cells: Vec<Option<Vec<String>>> = rows
        .iter()
        .map(|r| r.as_ref().map(|t| tick_cells(t, tz)))
        .collect();
    align_table(&TICK_HEADERS, &TICK_ALIGNS, &cells)
}

/// Renders preview `rows` (from [`preview_rows`]) as an aligned bar table; `None` is an ellipsis
/// row. Timestamps render in `tz`, prices at 4 decimals, volume/trades comma-grouped.
pub fn preview_bars(rows: &[Option<Bar>], tz: &TimeZone) -> String {
    let cells: Vec<Option<Vec<String>>> = rows
        .iter()
        .map(|r| r.as_ref().map(|b| bar_cells(b, tz)))
        .collect();
    align_table(&BAR_HEADERS, &BAR_ALIGNS, &cells)
}

/// Formats `headers` + `rows` as a whitespace-aligned table (two-space column gap). `aligns[i]`
/// right-justifies column `i`; a `None` row renders as an ellipsis (`...` in every column), like
/// `fwob inspect`.
fn align_table(headers: &[&str], aligns: &[bool], rows: &[Option<Vec<String>>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for cells in rows.iter().flatten() {
        for (i, cell) in cells.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let push_row = |out: &mut String, cells: &[String]| {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
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
    };
    let ellipsis: Vec<String> = vec!["...".to_owned(); headers.len()];
    let header_cells: Vec<String> = headers.iter().map(|h| (*h).to_owned()).collect();
    push_row(&mut out, &header_cells);
    for row in rows {
        match row {
            Some(cells) => push_row(&mut out, cells),
            None => push_row(&mut out, &ellipsis),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(n: u32) -> u32 {
        1_600_000_000 + n * DAY
    }

    #[test]
    fn granularity_intraday_and_calendar() {
        assert_eq!(detect_bar_granularity(&[0, 60, 120]).as_deref(), Some("1m"));
        assert_eq!(
            detect_bar_granularity(&[0, 1_800, 3_600]).as_deref(),
            Some("30m")
        );
        assert_eq!(
            detect_bar_granularity(&[0, 3_600, 7_200]).as_deref(),
            Some("1h")
        );
        // Daily with a weekend gap: the minimum positive gap is still ~1 day.
        assert_eq!(
            detect_bar_granularity(&[day(0), day(1), day(4), day(5)]).as_deref(),
            Some("1d")
        );
        assert_eq!(
            detect_bar_granularity(&[day(0), day(7), day(14)]).as_deref(),
            Some("1w")
        );
        assert_eq!(detect_bar_granularity(&[0]), None);
    }

    #[test]
    fn hours_classification() {
        let rth = Session::new("America/New_York", "09:30-16:00").unwrap();
        // 2024-01-02: 09:30 ET == 14:30Z; build a few RTH-interior minutes.
        let base = 1_704_205_800; // 09:30 ET
        let rth_times = [base, base + 3_600, base + 6 * 3_600]; // 09:30, 10:30, 15:30
        assert_eq!(classify_hours(&rth_times, &rth), "rth");
        // Add an 08:00 ET pre-market bar (90 min before open).
        let ext_times = [base - 90 * 60, base, base + 3_600];
        assert_eq!(classify_hours(&ext_times, &rth), "extended");
        // All the same time-of-day (daily bars anchored at the open) → indeterminate.
        let daily = [base, base + DAY, base + 2 * DAY];
        assert_eq!(classify_hours(&daily, &rth), "n/a");
    }

    #[test]
    fn semantic_labels() {
        assert_eq!(
            field_type_label(FieldType::UnsignedInteger),
            "unsigned-integer"
        );
        assert_eq!(
            field_semantic_label(FieldSemantic::UnixTimestamp(TimestampUnit::Seconds)),
            "unix-seconds"
        );
        assert_eq!(
            field_semantic_label(FieldSemantic::FixedPoint(4)),
            "fixed-4"
        );
    }

    #[test]
    fn tick_preview_is_aligned_and_tz_aware() {
        let tz = TimeZone::get("America/New_York").unwrap();
        let ticks = [
            Some(Tick {
                time: 1_704_205_800,
                price: 100.25,
                size: 500,
            }),
            Some(Tick {
                time: 1_704_205_860,
                price: 100.5,
                size: 1_200,
            }),
        ];
        let table = preview_ticks(&ticks, &tz);
        assert!(table.contains("time"), "{table}");
        assert!(table.contains("100.2500"), "{table}");
        // Winter ET offset.
        assert!(table.contains("-05:00"), "{table}");
    }

    #[test]
    fn sample_windows_are_non_overlapping_head_and_tail() {
        // Small file: leading window covers everything, no trailing window.
        assert_eq!(sample_windows(5, 1024), (0..5, None));
        // File larger than one window but smaller than two: tail abuts the lead (no overlap, no gap).
        assert_eq!(sample_windows(1500, 1024), (0..1024, Some(1024..1500)));
        // File larger than two windows: head [0,1024) and tail [count-1024, count).
        assert_eq!(sample_windows(5000, 1024), (0..1024, Some(3976..5000)));
        // Exactly one window: no tail.
        assert_eq!(sample_windows(1024, 1024), (0..1024, None));
    }

    #[test]
    fn preview_rows_head_tail_and_ellipsis() {
        // Small file (<= 2*N): every frame, no ellipsis.
        let all: Vec<u32> = (0..5).collect();
        let rows = preview_rows(5, &all, &[]);
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(Option::is_some));

        // Large file: head N + ellipsis + tail N, taken from leading/trailing windows.
        let head: Vec<u32> = (0..10).collect();
        let tail: Vec<u32> = (90..100).collect();
        let rows = preview_rows(100, &head, &tail);
        assert_eq!(rows.len(), FRAME_PREVIEW_COUNT * 2 + 1);
        assert_eq!(rows[0], Some(0));
        assert_eq!(rows[FRAME_PREVIEW_COUNT], None); // ellipsis
        assert_eq!(*rows.last().unwrap(), Some(99));

        // No distinct tail window (file fits in the leading sample): tail taken from head.
        let rows = preview_rows(10, &head, &[]);
        assert_eq!(rows[0], Some(0));
        assert_eq!(rows[FRAME_PREVIEW_COUNT], None);
        assert_eq!(*rows.last().unwrap(), Some(9));
    }

    #[test]
    fn ellipsis_row_renders() {
        let tz = TimeZone::get("UTC").unwrap();
        let rows = vec![
            Some(Tick {
                time: 1_704_205_800,
                price: 1.0,
                size: 1,
            }),
            None,
            Some(Tick {
                time: 1_704_205_860,
                price: 2.0,
                size: 2,
            }),
        ];
        let table = preview_ticks(&rows, &tz);
        assert!(table.contains("..."), "{table}");
    }

    // --- File-level hours classification -------------------------------------------------------

    use crate::analysis::ls::ls_file;
    use crate::analysis::output::write_bars_fwob;
    use crate::tick::{Tick as RawTick, tick_schema};
    use fwob::Writer;
    use fwob_v2::WriterOptions;
    use jiff::civil::{Weekday, date};
    use std::path::PathBuf;

    /// A throwaway directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "mdfwob-hours-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn new_york() -> Session {
        Session::new("America/New_York", "09:30-16:00").unwrap()
    }

    /// The epoch of `day` at `hour:minute` New York time.
    fn ny(day: Date, hour: i8, minute: i8) -> u32 {
        let tz = TimeZone::get("America/New_York").unwrap();
        let zoned = day.at(hour, minute, 0, 0).to_zoned(tz).unwrap();
        u32::try_from(zoned.timestamp().as_second()).unwrap()
    }

    /// Monday through Friday from `from` to `to`, inclusive.
    fn weekdays(from: Date, to: Date) -> Vec<Date> {
        let mut days = Vec::new();
        let mut day = from;
        while day <= to {
            if !matches!(day.weekday(), Weekday::Saturday | Weekday::Sunday) {
                days.push(day);
            }
            day = day.tomorrow().unwrap();
        }
        days
    }

    fn write_tick_times(dir: &Path, name: &str, times: &[u32]) -> PathBuf {
        let path = dir.join(format!("{name}.fwob"));
        let mut writer = Writer::create_v2(&path, tick_schema(), WriterOptions::new(name)).unwrap();
        let mut buf = Vec::new();
        for &time in times {
            buf.clear();
            RawTick::new(time, 100.0, 10).unwrap().encode(&mut buf);
            writer.append_frame(&buf).unwrap();
        }
        writer.finish().unwrap();
        path
    }

    /// The hours `inspect` and `ls` report for `path`, asserting they agree.
    fn reported_hours(path: &Path, sample: u64) -> String {
        let rth = new_york();
        let inspected = inspect_file(path, &rth, &rth.time_zone(), sample).unwrap();
        let listed = ls_file(String::new(), path, &rth, sample).unwrap();
        assert_eq!(
            inspected.hours,
            Some(listed.hours),
            "inspect and ls disagree"
        );
        listed.hours.to_owned()
    }

    /// The regression this probe exists for, shaped like the BILI file that exposed it: extended
    /// data whose first day begins mid-session (an IPO day) and whose last day ends mid-session (a
    /// download that stopped at 10:16). Every frame the head/tail windows see sits inside RTH, so
    /// the sample alone says `rth`; the opens of the days between say otherwise. The range also
    /// crosses the 2024-03-10 DST change and a weekend.
    #[test]
    fn hours_come_from_day_opens_not_from_where_the_data_starts_and_stops() {
        let scratch = Scratch::new("mid-session-ends");
        let days = weekdays(date(2024, 3, 4), date(2024, 3, 15));
        let (first_day, last_day) = (days[0], days[days.len() - 1]);
        let mut times = vec![
            ny(first_day, 11, 7),
            ny(first_day, 11, 8),
            ny(first_day, 11, 9),
            ny(first_day, 15, 59),
        ];
        for &day in &days[1..days.len() - 1] {
            for (hour, minute) in [(4, 0), (6, 30), (9, 30), (12, 0), (15, 59), (19, 59)] {
                times.push(ny(day, hour, minute));
            }
        }
        for (hour, minute) in [(4, 0), (9, 30), (10, 14), (10, 15), (10, 16)] {
            times.push(ny(last_day, hour, minute));
        }
        let path = write_tick_times(&scratch.0, "BILI", &times);

        let sample = 3;
        let (lead, tail) = sample_windows(times.len() as u64, sample);
        let windows: Vec<u32> = lead
            .chain(tail.unwrap())
            .map(|i| times[i as usize])
            .collect();
        assert_eq!(
            classify_hours(&windows, &new_york()),
            "rth",
            "the sampled windows alone should reproduce the misclassification"
        );

        assert_eq!(reported_hours(&path, sample), "extended");
    }

    /// Regular-hours data opens at the bell every day, so every probed open shares one
    /// time-of-day. That must read as `rth`, not the `n/a` a single observed minute means for daily
    /// bars.
    #[test]
    fn regular_hours_data_opening_at_the_bell_every_day_is_rth() {
        let scratch = Scratch::new("rth");
        let days = weekdays(date(2024, 3, 4), date(2024, 3, 15));
        let mut times = vec![ny(days[0], 13, 0), ny(days[0], 15, 59)];
        for &day in &days[1..] {
            for (hour, minute) in [(9, 30), (9, 30), (11, 0), (15, 59)] {
                times.push(ny(day, hour, minute));
            }
        }
        let path = write_tick_times(&scratch.0, "RTH", &times);
        assert_eq!(reported_hours(&path, 3), "rth");
    }

    /// The walk skips the file's first day, lands on the first frame of each trading day, jumps a
    /// weekend or a multi-day gap in one step, and takes `days` opens from each end.
    #[test]
    fn day_open_probe_walks_trading_days_from_each_end() {
        let scratch = Scratch::new("walk");
        // 2024-01-02, then nothing until 2024-01-16, then every weekday through 2024-01-31.
        let mut days = vec![date(2024, 1, 2)];
        days.extend(weekdays(date(2024, 1, 16), date(2024, 1, 31)));
        let mut times = Vec::new();
        for &day in &days {
            times.extend([ny(day, 4, 0), ny(day, 12, 0)]);
        }
        let path = write_tick_times(&scratch.0, "GAP", &times);

        let mut reader = Reader::open(&path).unwrap();
        let tz = new_york().time_zone();
        let (first, last) = (times[0], times[times.len() - 1]);
        let mut opens =
            day_open_times(&mut reader, InputKind::Tick, first, last, &tz, 5, 1, |_| {
                false
            })
            .unwrap();
        opens.sort_unstable();

        let expected_days = [
            date(2024, 1, 16),
            date(2024, 1, 17),
            date(2024, 1, 18),
            date(2024, 1, 19),
            date(2024, 1, 22),
            date(2024, 1, 25),
            date(2024, 1, 26),
            date(2024, 1, 29),
            date(2024, 1, 30),
            date(2024, 1, 31),
        ];
        let expected: Vec<u32> = expected_days.iter().map(|&day| ny(day, 4, 0)).collect();
        assert_eq!(opens, expected);

        // A day that satisfies `stop` ends the walk: nothing is read after the first probe.
        let opens = day_open_times(&mut reader, InputKind::Tick, first, last, &tz, 5, 1, |_| {
            true
        })
        .unwrap();
        assert_eq!(opens, [ny(date(2024, 1, 16), 4, 0)]);

        // `per_day` frames from each open, and a file whose ends meet samples each day once.
        let short = &times[..6]; // 2024-01-02, 01-16, 01-17
        let short_path = write_tick_times(&scratch.0, "SHORT", short);
        let mut reader = Reader::open(&short_path).unwrap();
        let mut opens = day_open_times(
            &mut reader,
            InputKind::Tick,
            short[0],
            short[5],
            &tz,
            5,
            2,
            |_| false,
        )
        .unwrap();
        opens.sort_unstable();
        assert_eq!(opens, short[2..].to_vec());
    }

    /// With no second trading day there is nothing to probe, so the sample decides.
    #[test]
    fn a_single_day_file_falls_back_to_its_sample() {
        let scratch = Scratch::new("one-day");
        let day = date(2024, 3, 5);
        let rth = write_tick_times(
            &scratch.0,
            "RTH",
            &[ny(day, 10, 0), ny(day, 10, 30), ny(day, 11, 0)],
        );
        assert_eq!(reported_hours(&rth, 1_024), "rth");
        let ext = write_tick_times(&scratch.0, "EXT", &[ny(day, 5, 0), ny(day, 10, 30)]);
        assert_eq!(reported_hours(&ext, 1_024), "extended");
    }

    /// Intraday bars probe like ticks; daily bars are bucket anchors and stay `n/a`.
    #[test]
    fn bar_files_probe_intraday_and_leave_daily_as_not_applicable() {
        let scratch = Scratch::new("bars");
        let bar = |time| Bar {
            time,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1,
            vwap: 1.0,
            trades: 1,
        };
        let days = weekdays(date(2024, 3, 4), date(2024, 3, 8));
        let mut intraday = vec![bar(ny(days[0], 10, 0)), bar(ny(days[0], 10, 1))];
        for &day in &days[1..] {
            for (hour, minute) in [(4, 0), (4, 1), (9, 30), (9, 31)] {
                intraday.push(bar(ny(day, hour, minute)));
            }
        }
        write_bars_fwob("MIN", &intraday, &scratch.0).unwrap();
        assert_eq!(reported_hours(&scratch.0.join("MIN.fwob"), 2), "extended");

        let daily: Vec<Bar> = days.iter().map(|&day| bar(ny(day, 9, 30))).collect();
        write_bars_fwob("DAY", &daily, &scratch.0).unwrap();
        assert_eq!(reported_hours(&scratch.0.join("DAY.fwob"), 1_024), "n/a");
    }
}
