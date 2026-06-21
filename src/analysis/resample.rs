//! Tick → OHLCV bar resampling.

use jiff::{Timestamp, ToSpan, Unit, civil, tz::TimeZone};

use crate::analysis::interval::{Granularity, Interval};
use crate::analysis::model::{Bar, Tick};

/// Controls how bar buckets are anchored.
///
/// Sub-day intervals are always UTC-epoch aligned (which, for whole-hour UTC
/// offsets, coincides with local clock boundaries). **Day-granular** intervals
/// are aligned to the local civil date in the exchange timezone, so a trading
/// day's extended-hours ticks that cross UTC midnight (e.g. US after-hours,
/// which in winter ends at 20:00 ET = 01:00 UTC the next day) stay in the
/// correct day's bar instead of bleeding into the next.
#[derive(Clone)]
pub enum BarClock {
    Utc,
    Zoned(TimeZone),
}

impl BarClock {
    /// Builds a zoned clock from an IANA timezone name.
    pub fn for_timezone(name: &str) -> anyhow::Result<Self> {
        Ok(Self::Zoned(TimeZone::get(name)?))
    }

    fn time_zone(&self) -> TimeZone {
        match self {
            BarClock::Utc => TimeZone::UTC,
            BarClock::Zoned(tz) => tz.clone(),
        }
    }

    fn bucket_start(&self, interval: Interval, time: u32) -> u32 {
        match interval.granularity() {
            Granularity::SubDay(secs) => sub_day_floor(time, secs),
            calendar => calendar_bucket_start(&self.time_zone(), calendar, time)
                .unwrap_or_else(|| sub_day_floor(time, 86_400)),
        }
    }

    fn advance(&self, interval: Interval, bucket_start: u32) -> u32 {
        match interval.granularity() {
            Granularity::SubDay(secs) => bucket_start.saturating_add(secs),
            calendar => calendar_advance(&self.time_zone(), calendar, bucket_start)
                .unwrap_or_else(|| bucket_start.saturating_add(86_400)),
        }
    }
}

fn sub_day_floor(time: u32, secs: u32) -> u32 {
    if secs == 0 { time } else { time - time % secs }
}

fn local_date(tz: &TimeZone, time: u32) -> Option<civil::Date> {
    Some(
        Timestamp::from_second(i64::from(time))
            .ok()?
            .to_zoned(tz.clone())
            .date(),
    )
}

fn date_epoch(tz: &TimeZone, date: civil::Date) -> Option<u32> {
    u32::try_from(date.to_zoned(tz.clone()).ok()?.timestamp().as_second()).ok()
}

/// UTC epoch of the start of the calendar bucket containing `time`.
fn calendar_bucket_start(tz: &TimeZone, granularity: Granularity, time: u32) -> Option<u32> {
    let date = local_date(tz, time)?;
    let bucket = match granularity {
        Granularity::Day(n) => floor_days(date, n)?,
        Granularity::Week(n) => floor_weeks(date, n)?,
        Granularity::Month(n) => floor_months(date, n)?,
        Granularity::Year(n) => floor_years(date, n)?,
        Granularity::SubDay(_) => return None,
    };
    date_epoch(tz, bucket)
}

/// UTC epoch of the next calendar bucket start (DST/calendar-safe).
fn calendar_advance(tz: &TimeZone, granularity: Granularity, bucket_start: u32) -> Option<u32> {
    let date = local_date(tz, bucket_start)?;
    let next = match granularity {
        Granularity::Day(n) => date.checked_add(i32::try_from(n).ok()?.days()).ok()?,
        Granularity::Week(n) => date.checked_add(i32::try_from(n).ok()?.weeks()).ok()?,
        Granularity::Month(n) => date.checked_add(i32::try_from(n).ok()?.months()).ok()?,
        Granularity::Year(n) => date.checked_add(i32::try_from(n).ok()?.years()).ok()?,
        Granularity::SubDay(_) => return None,
    };
    date_epoch(tz, next)
}

fn floor_days(date: civil::Date, n: u32) -> Option<civil::Date> {
    let reference = civil::date(1970, 1, 1);
    let day_index = i64::from(date.since((Unit::Day, reference)).ok()?.get_days());
    let step = i64::from(n);
    let offset = day_index.div_euclid(step).checked_mul(step)?;
    reference
        .checked_add(i32::try_from(offset).ok()?.days())
        .ok()
}

fn floor_weeks(date: civil::Date, n: u32) -> Option<civil::Date> {
    // 1970-01-05 is a Monday; week buckets are aligned to it.
    let reference = civil::date(1970, 1, 5);
    let days = i64::from(date.since((Unit::Day, reference)).ok()?.get_days());
    let week_index = days.div_euclid(7);
    let step = i64::from(n);
    let bucket_week = week_index.div_euclid(step).checked_mul(step)?;
    let offset_days = bucket_week.checked_mul(7)?;
    reference
        .checked_add(i32::try_from(offset_days).ok()?.days())
        .ok()
}

fn floor_months(date: civil::Date, n: u32) -> Option<civil::Date> {
    let month_index = i64::from(date.year()) * 12 + i64::from(date.month() - 1);
    let step = i64::from(n);
    let bucket = month_index.div_euclid(step).checked_mul(step)?;
    let year = i16::try_from(bucket.div_euclid(12)).ok()?;
    let month = i8::try_from(bucket.rem_euclid(12) + 1).ok()?;
    civil::Date::new(year, month, 1).ok()
}

fn floor_years(date: civil::Date, n: u32) -> Option<civil::Date> {
    let step = i32::try_from(n).ok()?;
    let bucket = i32::from(date.year()).div_euclid(step).checked_mul(step)?;
    civil::Date::new(i16::try_from(bucket).ok()?, 1, 1).ok()
}

struct Accumulator {
    time: u32,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: i64,
    weighted: f64,
    trades: u64,
}

impl Accumulator {
    fn new(time: u32, tick: &Tick) -> Self {
        Self {
            time,
            open: tick.price,
            high: tick.price,
            low: tick.price,
            close: tick.price,
            volume: i64::from(tick.size),
            weighted: tick.price * f64::from(tick.size),
            trades: 1,
        }
    }

    fn update(&mut self, tick: &Tick) {
        self.high = self.high.max(tick.price);
        self.low = self.low.min(tick.price);
        self.close = tick.price;
        self.volume += i64::from(tick.size);
        self.weighted += tick.price * f64::from(tick.size);
        self.trades += 1;
    }

    fn finish(self) -> Bar {
        let vwap = if self.volume != 0 {
            self.weighted / self.volume as f64
        } else {
            f64::NAN
        };
        Bar {
            time: self.time,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
            vwap,
            trades: self.trades,
        }
    }
}

/// Resamples ascending ticks into bars at `interval`, anchoring buckets per
/// `clock`. When `fill` is set, every empty interval between the first and last
/// bar is forward-filled (a flat bar at the previous close, zero volume).
pub fn resample(ticks: &[Tick], interval: Interval, fill: bool, clock: &BarClock) -> Vec<Bar> {
    let mut bars = Vec::new();
    let mut current: Option<Accumulator> = None;
    for tick in ticks {
        let bucket = clock.bucket_start(interval, tick.time);
        match &mut current {
            Some(acc) if acc.time == bucket => acc.update(tick),
            _ => {
                if let Some(acc) = current.take() {
                    bars.push(acc.finish());
                }
                current = Some(Accumulator::new(bucket, tick));
            }
        }
    }
    if let Some(acc) = current {
        bars.push(acc.finish());
    }

    if fill && bars.len() > 1 {
        bars = forward_fill(&bars, interval, clock);
    }
    bars
}

fn forward_fill(bars: &[Bar], interval: Interval, clock: &BarClock) -> Vec<Bar> {
    let mut filled = Vec::with_capacity(bars.len());
    let mut iter = bars.iter();
    let Some(first) = iter.next() else {
        return filled;
    };
    filled.push(*first);
    let mut prev_close = first.close;
    let mut expected = clock.advance(interval, first.time);
    for bar in iter {
        while expected < bar.time {
            filled.push(Bar {
                time: expected,
                open: prev_close,
                high: prev_close,
                low: prev_close,
                close: prev_close,
                volume: 0,
                vwap: prev_close,
                trades: 0,
            });
            let next = clock.advance(interval, expected);
            if next <= expected {
                break; // guard against a non-advancing clock
            }
            expected = next;
        }
        filled.push(*bar);
        prev_close = bar.close;
        expected = clock.advance(interval, bar.time);
    }
    filled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(time: u32, price: f64, size: i32) -> Tick {
        Tick { time, price, size }
    }

    #[test]
    fn aggregates_ohlcv_within_buckets() {
        let interval = Interval::parse("1m").unwrap().unwrap();
        let ticks = vec![
            tick(0, 10.0, 100),
            tick(30, 12.0, 50),
            tick(59, 11.0, 25),
            tick(60, 11.5, 200),
        ];
        let bars = resample(&ticks, interval, false, &BarClock::Utc);
        assert_eq!(bars.len(), 2);
        let first = bars[0];
        assert_eq!(first.time, 0);
        assert_eq!(first.open, 10.0);
        assert_eq!(first.high, 12.0);
        assert_eq!(first.low, 10.0);
        assert_eq!(first.close, 11.0);
        assert_eq!(first.volume, 175);
        assert_eq!(first.trades, 3);
        // vwap = (10*100 + 12*50 + 11*25) / 175
        assert!((first.vwap - (1000.0 + 600.0 + 275.0) / 175.0).abs() < 1e-9);
    }

    #[test]
    fn fill_inserts_flat_bars_for_empty_buckets() {
        let interval = Interval::parse("1m").unwrap().unwrap();
        // Buckets 0 and 180 are present; 60 and 120 are empty.
        let ticks = vec![tick(0, 10.0, 100), tick(180, 13.0, 100)];
        let bars = resample(&ticks, interval, true, &BarClock::Utc);
        assert_eq!(bars.len(), 4);
        assert_eq!(bars[1].time, 60);
        assert_eq!(bars[1].close, 10.0);
        assert_eq!(bars[1].volume, 0);
        assert_eq!(bars[1].trades, 0);
        assert_eq!(bars[3].time, 180);
        assert_eq!(bars[3].close, 13.0);
    }

    #[test]
    fn daily_bars_bucket_by_exchange_local_date() {
        let interval = Interval::parse("1d").unwrap().unwrap();
        let clock = BarClock::for_timezone("America/New_York").unwrap();

        // 2024-01-02 15:00:00Z == 10:00 ET on Jan 2 (winter, UTC-5).
        let jan2_morning = 1_704_207_600u32;
        // 2024-01-03 00:30:00Z == 19:30 ET on Jan 2 -> still Jan 2's trading day.
        let jan2_after_hours = 1_704_241_800u32;
        // 2024-01-03 15:00:00Z == 10:00 ET on Jan 3 -> next trading day.
        let jan3_morning = 1_704_294_000u32;

        let ticks = vec![
            tick(jan2_morning, 10.0, 100),
            tick(jan2_after_hours, 11.0, 100),
            tick(jan3_morning, 12.0, 100),
        ];
        let bars = resample(&ticks, interval, false, &clock);

        // The 00:30Z tick must NOT start a new (Jan 3 UTC) bar; it belongs to Jan 2 ET.
        assert_eq!(bars.len(), 2);
        // Bar 0 spans Jan 2 ET (midnight ET = 2024-01-02 05:00:00Z = 1_704_171_600).
        assert_eq!(bars[0].time, 1_704_171_600);
        assert_eq!(bars[0].trades, 2);
        assert_eq!(bars[0].close, 11.0);
        // Bar 1 starts at Jan 3 ET midnight (2024-01-03 05:00:00Z = 1_704_258_000).
        assert_eq!(bars[1].time, 1_704_258_000);
        assert_eq!(bars[1].trades, 1);
    }

    #[test]
    fn utc_clock_keeps_daily_bars_on_utc_midnight() {
        let interval = Interval::parse("1d").unwrap().unwrap();
        let ticks = vec![
            tick(1_704_207_600, 10.0, 100),
            tick(1_704_241_800, 11.0, 100),
        ];
        // With the UTC clock, the 00:30Z tick falls into the next UTC day.
        let bars = resample(&ticks, interval, false, &BarClock::Utc);
        assert_eq!(bars.len(), 2);
    }

    /// UTC epoch of a New York wall-clock instant.
    fn ny(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> u32 {
        civil::datetime(year, month, day, hour, minute, 0, 0)
            .to_zoned(TimeZone::get("America/New_York").unwrap())
            .unwrap()
            .timestamp()
            .as_second() as u32
    }

    #[test]
    fn weekly_bars_group_by_calendar_week() {
        let interval = Interval::parse("1w").unwrap().unwrap();
        let clock = BarClock::for_timezone("America/New_York").unwrap();
        // Tue 2024-01-09 and Thu 2024-01-11 share an ISO week (Mon 2024-01-08);
        // Mon 2024-01-15 starts the next week.
        let ticks = vec![
            tick(ny(2024, 1, 9, 12, 0), 1.0, 1),
            tick(ny(2024, 1, 11, 12, 0), 1.0, 1),
            tick(ny(2024, 1, 15, 12, 0), 1.0, 1),
        ];
        let bars = resample(&ticks, interval, false, &clock);
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].trades, 2);
        assert_eq!(bars[0].time, ny(2024, 1, 8, 0, 0)); // Monday midnight ET
        assert_eq!(bars[1].time, ny(2024, 1, 15, 0, 0));
    }

    #[test]
    fn monthly_bars_group_by_calendar_month() {
        let interval = Interval::parse("1mo").unwrap().unwrap();
        let clock = BarClock::for_timezone("America/New_York").unwrap();
        let ticks = vec![
            tick(ny(2024, 1, 15, 12, 0), 1.0, 1),
            tick(ny(2024, 1, 31, 20, 0), 1.0, 1), // late after-hours, still January
            tick(ny(2024, 2, 1, 10, 0), 1.0, 1),
        ];
        let bars = resample(&ticks, interval, false, &clock);
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].trades, 2);
        assert_eq!(bars[0].time, ny(2024, 1, 1, 0, 0));
        assert_eq!(bars[1].time, ny(2024, 2, 1, 0, 0));
    }

    #[test]
    fn yearly_bars_group_by_calendar_year() {
        let interval = Interval::parse("1y").unwrap().unwrap();
        let clock = BarClock::for_timezone("America/New_York").unwrap();
        let ticks = vec![
            tick(ny(2024, 3, 1, 12, 0), 1.0, 1),
            tick(ny(2024, 12, 31, 18, 0), 1.0, 1),
            tick(ny(2025, 1, 2, 10, 0), 1.0, 1),
        ];
        let bars = resample(&ticks, interval, false, &clock);
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].trades, 2);
        assert_eq!(bars[0].time, ny(2024, 1, 1, 0, 0));
        assert_eq!(bars[1].time, ny(2025, 1, 1, 0, 0));
    }

    #[test]
    fn monthly_fill_steps_calendar_months() {
        let interval = Interval::parse("1mo").unwrap().unwrap();
        let clock = BarClock::for_timezone("America/New_York").unwrap();
        // January and April present; February and March must be filled.
        let ticks = vec![
            tick(ny(2024, 1, 10, 12, 0), 10.0, 1),
            tick(ny(2024, 4, 10, 12, 0), 13.0, 1),
        ];
        let bars = resample(&ticks, interval, true, &clock);
        assert_eq!(bars.len(), 4);
        assert_eq!(bars[1].time, ny(2024, 2, 1, 0, 0));
        assert_eq!(bars[2].time, ny(2024, 3, 1, 0, 0));
        assert_eq!(bars[1].volume, 0);
        assert_eq!(bars[3].time, ny(2024, 4, 1, 0, 0));
    }
}
