//! Helpers for the `inspect` command: bar-granularity and trading-hours detection over a bounded
//! frame sample, schema-field labels, and a timezone- and semantic-aware frame preview. Pure
//! functions over decoded values so they are unit-testable without a file.

use fwob_core::{FieldSemantic, FieldType, TimestampUnit};
use jiff::{Timestamp, tz::TimeZone};

use crate::analysis::model::{Bar, Tick};
use crate::analysis::output::{comma_i64, comma_u64, fmt_price, format_epoch_tz};
use crate::analysis::session::Session;

const DAY: u32 = 86_400;

/// Detects a bar series' interval label (`1m`, `30m`, `1h`, `1d`, `1w`, `1mo`, `1y`, …) from the
/// minimum positive gap between consecutive bar times. Intraday gaps map exactly; day-and-larger
/// gaps are matched with tolerance (DST makes a "1 day" gap 23–25h, weekends leave the *minimum*
/// gap at ~1 day). Returns `None` for fewer than two bars or no positive gap.
pub fn detect_bar_granularity(times: &[u32]) -> Option<String> {
    let mut min_delta = u32::MAX;
    for pair in times.windows(2) {
        let delta = pair[1].saturating_sub(pair[0]);
        if delta > 0 && delta < min_delta {
            min_delta = delta;
        }
    }
    (min_delta != u32::MAX).then(|| granularity_label(min_delta))
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

/// Renders up to `limit` ticks as an aligned table (timestamps in `tz`, prices at 4 decimals).
pub fn preview_ticks(ticks: &[Tick], tz: &TimeZone, limit: usize) -> String {
    let headers = ["time", "price", "size"];
    let aligns = [false, true, true];
    let rows: Vec<Vec<String>> = ticks
        .iter()
        .take(limit)
        .map(|tick| {
            vec![
                format_epoch_tz(tick.time, tz),
                fmt_price(tick.price),
                comma_i64(i64::from(tick.size)),
            ]
        })
        .collect();
    align_table(&headers, &aligns, &rows)
}

/// Renders up to `limit` bars as an aligned table (timestamps in `tz`, prices at 4 decimals,
/// volume/trades comma-grouped).
pub fn preview_bars(bars: &[Bar], tz: &TimeZone, limit: usize) -> String {
    let headers = [
        "time", "open", "high", "low", "close", "volume", "vwap", "trades",
    ];
    let aligns = [false, true, true, true, true, true, true, true];
    let rows: Vec<Vec<String>> = bars
        .iter()
        .take(limit)
        .map(|bar| {
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
        })
        .collect();
    align_table(&headers, &aligns, &rows)
}

/// Formats `headers` + `rows` as a whitespace-aligned table (two-space column gap). `aligns[i]`
/// right-justifies column `i`.
fn align_table(headers: &[&str], aligns: &[bool], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
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
    let header_cells: Vec<String> = headers.iter().map(|h| (*h).to_owned()).collect();
    push_row(&mut out, &header_cells);
    for row in rows {
        push_row(&mut out, row);
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
            Tick {
                time: 1_704_205_800,
                price: 100.25,
                size: 500,
            },
            Tick {
                time: 1_704_205_860,
                price: 100.5,
                size: 1_200,
            },
        ];
        let table = preview_ticks(&ticks, &tz, 10);
        assert!(table.contains("time"), "{table}");
        assert!(table.contains("100.2500"), "{table}");
        // Winter ET offset.
        assert!(table.contains("-05:00"), "{table}");
    }
}
