//! Bar interval token parsing.
//!
//! Supported units: `s` seconds, `m` minutes, `h` hours, `d` days, `w` weeks,
//! `mo` months, `y` years. Sub-day units (`s`/`m`/`h`) are bucketed by a fixed
//! number of seconds; day-and-larger units are bucketed by **calendar** date in
//! the exchange timezone (see [`crate::analysis::resample::BarClock`]).

use anyhow::{Result, bail};

/// Upper bound on the unit count, to keep calendar arithmetic well within range.
const MAX_COUNT: u32 = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalUnit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

/// How an interval's buckets are anchored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    /// A fixed number of seconds, UTC-epoch aligned.
    SubDay(u32),
    /// `n` calendar days, aligned to the local date.
    Day(u32),
    /// `n` calendar weeks, aligned to the local Monday.
    Week(u32),
    /// `n` calendar months, aligned to the first of the local month.
    Month(u32),
    /// `n` calendar years, aligned to local January 1.
    Year(u32),
}

/// A resampling interval: a count and a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval {
    count: u32,
    unit: IntervalUnit,
}

impl Interval {
    /// Parses an interval token like `5m`, `1h`, `1d`, `2w`, `3mo`, `1y`.
    ///
    /// Returns `None` when `value` is not interval-shaped (so a token classifier
    /// can fall through to treating it as a path/symbol). Returns `Some(Err(..))`
    /// when it *looks* like an interval but is out of range.
    pub fn parse(value: &str) -> Option<Result<Self>> {
        let (digits, unit) = split_unit(value)?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some((|| {
            let count: u32 = digits
                .parse()
                .map_err(|_| anyhow::anyhow!("interval count is too large"))?;
            if count == 0 {
                bail!("interval must be at least 1");
            }
            if count > MAX_COUNT {
                bail!("interval count must not exceed {MAX_COUNT}");
            }
            Ok(Self { count, unit })
        })())
    }

    pub fn count(self) -> u32 {
        self.count
    }

    pub fn unit(self) -> IntervalUnit {
        self.unit
    }

    /// How buckets for this interval are anchored.
    pub fn granularity(self) -> Granularity {
        match self.unit {
            IntervalUnit::Second => Granularity::SubDay(self.count),
            IntervalUnit::Minute => Granularity::SubDay(self.count.saturating_mul(60)),
            IntervalUnit::Hour => Granularity::SubDay(self.count.saturating_mul(3_600)),
            IntervalUnit::Day => Granularity::Day(self.count),
            IntervalUnit::Week => Granularity::Week(self.count),
            IntervalUnit::Month => Granularity::Month(self.count),
            IntervalUnit::Year => Granularity::Year(self.count),
        }
    }

    /// The canonical token label, e.g. `5m` or `3mo`.
    pub fn label(self) -> String {
        let suffix = match self.unit {
            IntervalUnit::Second => "s",
            IntervalUnit::Minute => "m",
            IntervalUnit::Hour => "h",
            IntervalUnit::Day => "d",
            IntervalUnit::Week => "w",
            IntervalUnit::Month => "mo",
            IntervalUnit::Year => "y",
        };
        format!("{}{}", self.count, suffix)
    }
}

fn split_unit(value: &str) -> Option<(&str, IntervalUnit)> {
    // The only two-character unit is "mo"; check it before the single-char units
    // so it is not mistaken for minutes.
    if let Some(digits) = value.strip_suffix("mo") {
        return Some((digits, IntervalUnit::Month));
    }
    let last = value.chars().last()?;
    let unit = match last {
        's' => IntervalUnit::Second,
        'm' => IntervalUnit::Minute,
        'h' => IntervalUnit::Hour,
        'd' => IntervalUnit::Day,
        'w' => IntervalUnit::Week,
        'y' => IntervalUnit::Year,
        _ => return None,
    };
    Some((&value[..value.len() - last.len_utf8()], unit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_units() {
        use IntervalUnit::*;
        let cases = [
            ("30s", 30, Second),
            ("5m", 5, Minute),
            ("1h", 1, Hour),
            ("1d", 1, Day),
            ("2w", 2, Week),
            ("3mo", 3, Month),
            ("1y", 1, Year),
        ];
        for (token, count, unit) in cases {
            let interval = Interval::parse(token).unwrap().unwrap();
            assert_eq!(interval.count(), count, "{token}");
            assert_eq!(interval.unit(), unit, "{token}");
            assert_eq!(interval.label(), token, "{token}");
        }
    }

    #[test]
    fn granularity_splits_sub_day_from_calendar() {
        assert_eq!(
            Interval::parse("5m").unwrap().unwrap().granularity(),
            Granularity::SubDay(300)
        );
        assert_eq!(
            Interval::parse("2h").unwrap().unwrap().granularity(),
            Granularity::SubDay(7_200)
        );
        assert_eq!(
            Interval::parse("1d").unwrap().unwrap().granularity(),
            Granularity::Day(1)
        );
        assert_eq!(
            Interval::parse("2w").unwrap().unwrap().granularity(),
            Granularity::Week(2)
        );
        assert_eq!(
            Interval::parse("3mo").unwrap().unwrap().granularity(),
            Granularity::Month(3)
        );
        assert_eq!(
            Interval::parse("1y").unwrap().unwrap().granularity(),
            Granularity::Year(1)
        );
    }

    #[test]
    fn rejects_non_interval_tokens() {
        for token in ["AAPL", "csv", "md", "demo", "m", "mo", "5x", "rth"] {
            assert!(Interval::parse(token).is_none(), "{token} should not parse");
        }
    }

    #[test]
    fn rejects_zero_and_oversized() {
        assert!(Interval::parse("0m").unwrap().is_err());
        assert!(Interval::parse("200000d").unwrap().is_err());
    }
}
