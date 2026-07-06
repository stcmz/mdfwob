//! Tick → OHLCV bar resampling.

use anyhow::Result;
use jiff::{Timestamp, ToSpan, Unit, civil, tz::TimeZone};

use crate::analysis::interval::{Granularity, Interval};
use crate::analysis::model::{Bar, Tick};
use crate::analysis::session::Session;

/// Controls how bar buckets are anchored.
///
/// For all variants, **day-granular and larger** intervals are aligned to the local civil date in
/// the exchange timezone, so a trading day's extended-hours ticks that cross UTC midnight (e.g. US
/// after-hours, which in winter ends at 20:00 ET = 01:00 UTC the next day) stay in the correct
/// day's bar instead of bleeding into the next. **Sub-day** intervals are UTC-epoch aligned for
/// [`BarClock::Utc`]/[`BarClock::Zoned`], but anchored to the trading-session open for
/// [`BarClock::Session`] (what `bars`/`calc` use), so intraday bars start at 09:30/04:00 and tile
/// the session.
#[derive(Clone)]
pub enum BarClock {
    Utc,
    Zoned(TimeZone),
    /// Sub-day buckets anchored to the session **open** (e.g. 09:30) in the session timezone;
    /// day-and-larger buckets anchored to exchange-local midnight. Used by `bars`/`calc` so
    /// intraday bars start at the session open and tile the session.
    Session(Session),
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
            BarClock::Session(session) => session.time_zone(),
        }
    }

    fn bucket_start(&self, interval: Interval, time: u32) -> u32 {
        match interval.granularity() {
            Granularity::SubDay(secs) => match self {
                BarClock::Session(session) => {
                    session_floor(session, secs, time).unwrap_or_else(|| sub_day_floor(time, secs))
                }
                _ => sub_day_floor(time, secs),
            },
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

    /// Exclusive upper bound for forward-fill bars: the session **close** of `prev_start`'s day,
    /// so sub-day fill never spans the overnight gap (or whole empty days). `None` for non-session
    /// clocks and calendar intervals, where fill is bounded only by the next real bar.
    fn fill_stop(&self, interval: Interval, prev_start: u32) -> Option<u32> {
        match (self, interval.granularity()) {
            (BarClock::Session(session), Granularity::SubDay(_)) => Some(
                session
                    .open_epoch(prev_start)?
                    .saturating_add(session.length_seconds()),
            ),
            _ => None,
        }
    }
}

fn sub_day_floor(time: u32, secs: u32) -> u32 {
    if secs == 0 { time } else { time - time % secs }
}

/// Floors `time` to the sub-day bucket anchored at the session open of `time`'s local day.
fn session_floor(session: &Session, secs: u32, time: u32) -> Option<u32> {
    if secs == 0 {
        return Some(time);
    }
    let open = session.open_epoch(time)?;
    let delta = i64::from(time) - i64::from(open);
    let start = i64::from(open) + delta.div_euclid(i64::from(secs)) * i64::from(secs);
    u32::try_from(start).ok()
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

/// The price mass (`vwap * volume`, i.e. the sum of `price * size` over the bar's ticks) a bar
/// contributes to a re-resampled bucket's VWAP. Zero-volume/empty bars carry no mass.
fn bar_weighted(bar: &Bar) -> f64 {
    if bar.vwap.is_finite() {
        bar.vwap * bar.volume as f64
    } else {
        0.0
    }
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

    /// Opens a bucket from an existing bar (used when re-resampling a bar file to a coarser or
    /// equal interval). `time` is the target bucket start, not the source bar's time.
    fn from_bar(time: u32, bar: &Bar) -> Self {
        Self {
            time,
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
            volume: bar.volume,
            weighted: bar_weighted(bar),
            trades: bar.trades,
        }
    }

    /// Folds another source bar into the open bucket. Source bars are ascending, so `close` is the
    /// latest bar's close; the VWAP is rebuilt from each bar's `vwap * volume` price mass.
    fn update_bar(&mut self, bar: &Bar) {
        self.high = self.high.max(bar.high);
        self.low = self.low.min(bar.low);
        self.close = bar.close;
        self.volume += bar.volume;
        self.weighted += bar_weighted(bar);
        self.trades += bar.trades;
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

/// Incremental tick → bar resampler. Feed ascending ticks with [`Resampler::push`], passing a
/// sink that receives each bar **the moment its bucket closes** (so rows can stream straight to
/// the terminal); flush the final open bucket with [`Resampler::finish`]. Neither the ticks nor
/// the resulting bars are ever all held in memory at once.
pub struct Resampler {
    interval: Interval,
    clock: BarClock,
    current: Option<Accumulator>,
    /// Exclusive end of the open bucket, cached so the (potentially expensive) timezone bucket
    /// computation runs once per bucket instead of once per tick.
    bucket_end: u32,
}

impl Resampler {
    pub fn new(interval: Interval, clock: BarClock) -> Self {
        Self {
            interval,
            clock,
            current: None,
            bucket_end: 0,
        }
    }

    /// Folds `tick` into the open bucket, emitting the just-completed bar through `emit` when the
    /// tick opens a new bucket. Ticks must be ascending.
    pub fn push(&mut self, tick: &Tick, emit: &mut impl FnMut(Bar) -> Result<()>) -> Result<()> {
        match &mut self.current {
            // Ticks are ascending, so a tick belongs to the open bucket while `time < bucket_end`.
            Some(acc) if tick.time < self.bucket_end => acc.update(tick),
            _ => {
                if let Some(acc) = self.current.take() {
                    emit(acc.finish())?;
                }
                let start = self.clock.bucket_start(self.interval, tick.time);
                self.bucket_end = self
                    .clock
                    .advance(self.interval, start)
                    .max(start.saturating_add(1));
                self.current = Some(Accumulator::new(start, tick));
            }
        }
        Ok(())
    }

    /// Emits the final open bucket (if any).
    pub fn finish(mut self, emit: &mut impl FnMut(Bar) -> Result<()>) -> Result<()> {
        if let Some(acc) = self.current.take() {
            emit(acc.finish())?;
        }
        Ok(())
    }
}

/// Incremental bar → bar resampler: re-buckets an ascending stream of source bars into a coarser
/// (or equal) `interval`, aggregating OHLCV the same way [`Resampler`] aggregates ticks. Feeding a
/// finer interval than the source simply re-times each bar into its own bucket (no sub-division is
/// possible). Mirrors [`Resampler`]: emit fires as each bucket closes; [`BarResampler::finish`]
/// flushes the final open bucket.
pub struct BarResampler {
    interval: Interval,
    clock: BarClock,
    current: Option<Accumulator>,
    bucket_end: u32,
}

impl BarResampler {
    pub fn new(interval: Interval, clock: BarClock) -> Self {
        Self {
            interval,
            clock,
            current: None,
            bucket_end: 0,
        }
    }

    /// Folds `bar` into the open target bucket, emitting the just-completed bar when `bar` opens a
    /// new bucket. Source bars must be ascending by time.
    pub fn push(&mut self, bar: &Bar, emit: &mut impl FnMut(Bar) -> Result<()>) -> Result<()> {
        match &mut self.current {
            Some(acc) if bar.time < self.bucket_end => acc.update_bar(bar),
            _ => {
                if let Some(acc) = self.current.take() {
                    emit(acc.finish())?;
                }
                let start = self.clock.bucket_start(self.interval, bar.time);
                self.bucket_end = self
                    .clock
                    .advance(self.interval, start)
                    .max(start.saturating_add(1));
                self.current = Some(Accumulator::from_bar(start, bar));
            }
        }
        Ok(())
    }

    /// Emits the final open bucket (if any).
    pub fn finish(mut self, emit: &mut impl FnMut(Bar) -> Result<()>) -> Result<()> {
        if let Some(acc) = self.current.take() {
            emit(acc.finish())?;
        }
        Ok(())
    }
}

/// Wraps a bar sink to optionally forward-fill empty intervals. When `fill` is set, each real bar
/// is preceded by flat bars (previous close, zero volume) for every empty interval since the last
/// real bar, so gaps between (but not before the first or after the last) real bar are filled.
/// Streams: it only ever holds the previous real bar, never the whole series.
pub struct ForwardFiller<F: FnMut(Bar) -> Result<()>> {
    interval: Interval,
    clock: BarClock,
    fill: bool,
    prev: Option<Bar>,
    sink: F,
}

impl<F: FnMut(Bar) -> Result<()>> ForwardFiller<F> {
    pub fn new(interval: Interval, clock: BarClock, fill: bool, sink: F) -> Self {
        Self {
            interval,
            clock,
            fill,
            prev: None,
            sink,
        }
    }

    /// Forwards `bar` to the sink, first emitting any flat fill bars before it. Fill never crosses
    /// a session close (see [`BarClock::fill_stop`]), so the overnight gap and empty days stay
    /// empty rather than being padded with flat bars.
    pub fn push(&mut self, bar: Bar) -> Result<()> {
        if self.fill {
            if let Some(prev) = self.prev {
                let cap = self.clock.fill_stop(self.interval, prev.time);
                let mut expected = self.clock.advance(self.interval, prev.time);
                while expected < bar.time && cap.is_none_or(|stop| expected < stop) {
                    (self.sink)(Bar {
                        time: expected,
                        open: prev.close,
                        high: prev.close,
                        low: prev.close,
                        close: prev.close,
                        volume: 0,
                        vwap: prev.close,
                        trades: 0,
                    })?;
                    let next = self.clock.advance(self.interval, expected);
                    if next <= expected {
                        break; // guard against a non-advancing clock
                    }
                    expected = next;
                }
            }
            self.prev = Some(bar);
        }
        (self.sink)(bar)
    }
}

/// Resamples ascending ticks into bars at `interval`, anchoring buckets per
/// `clock`. When `fill` is set, every empty interval between the first and last
/// bar is forward-filled (a flat bar at the previous close, zero volume).
pub fn resample(ticks: &[Tick], interval: Interval, fill: bool, clock: &BarClock) -> Vec<Bar> {
    let mut bars = Vec::new();
    {
        let mut filler = ForwardFiller::new(interval, clock.clone(), fill, |bar: Bar| {
            bars.push(bar);
            Ok(())
        });
        let mut resampler = Resampler::new(interval, clock.clone());
        for tick in ticks {
            resampler
                .push(tick, &mut |bar| filler.push(bar))
                .expect("collecting into a Vec is infallible");
        }
        resampler
            .finish(&mut |bar| filler.push(bar))
            .expect("collecting into a Vec is infallible");
    }
    bars
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
    fn bar_resampler_aggregates_bars_to_coarser_interval() {
        let interval = Interval::parse("1m").unwrap().unwrap();
        let input = vec![
            Bar {
                time: 0,
                open: 10.0,
                high: 11.0,
                low: 9.0,
                close: 10.5,
                volume: 100,
                vwap: 10.2,
                trades: 5,
            },
            Bar {
                time: 30,
                open: 10.5,
                high: 12.0,
                low: 10.0,
                close: 11.5,
                volume: 200,
                vwap: 11.0,
                trades: 8,
            },
            Bar {
                time: 90,
                open: 11.5,
                high: 11.6,
                low: 11.0,
                close: 11.2,
                volume: 50,
                vwap: 11.3,
                trades: 2,
            },
        ];
        let mut out = Vec::new();
        {
            let mut r = BarResampler::new(interval, BarClock::Utc);
            for bar in &input {
                r.push(bar, &mut |b| {
                    out.push(b);
                    Ok(())
                })
                .unwrap();
            }
            r.finish(&mut |b| {
                out.push(b);
                Ok(())
            })
            .unwrap();
        }
        // Bucket [0,60) merges the first two bars; bucket [60,120) holds the third.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].time, 0);
        assert_eq!(out[0].open, 10.0); // first bar's open
        assert_eq!(out[0].high, 12.0); // max high
        assert_eq!(out[0].low, 9.0); // min low
        assert_eq!(out[0].close, 11.5); // last bar's close
        assert_eq!(out[0].volume, 300);
        assert_eq!(out[0].trades, 13);
        // vwap rebuilt from price mass: (10.2*100 + 11.0*200) / 300.
        assert!((out[0].vwap - (10.2 * 100.0 + 11.0 * 200.0) / 300.0).abs() < 1e-9);
        assert_eq!(out[1].time, 60);
        assert_eq!(out[1].trades, 2);
        assert!((out[1].vwap - 11.3).abs() < 1e-9);
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
    fn sub_day_bars_anchor_to_session_open() {
        let interval = Interval::parse("1h").unwrap().unwrap();
        let session = Session::new("America/New_York", "09:30-16:00").unwrap();
        let clock = BarClock::Session(session);
        // 2024-01-02 09:30 ET (winter, UTC-5).
        let open = 1_704_205_800u32;
        let ticks = vec![
            tick(open, 10.0, 1),                      // 09:30 -> 09:30 bucket
            tick(open + 30 * 60, 11.0, 1),            // 10:00 -> 09:30 bucket (09:30-10:30)
            tick(open + 2 * 3600 + 15 * 60, 12.0, 1), // 11:45 -> 11:30 bucket
            tick(open + 6 * 3600 + 15 * 60, 13.0, 1), // 15:45 -> 15:30 bucket (partial: closes 16:00)
        ];
        let bars = resample(&ticks, interval, false, &clock);
        assert_eq!(bars.len(), 3);
        assert_eq!(bars[0].time, open); // 09:30, not 09:00
        assert_eq!(bars[0].trades, 2);
        assert_eq!(bars[1].time, open + 2 * 3600); // 11:30
        assert_eq!(bars[2].time, open + 6 * 3600); // 15:30 (session's short last bar)
    }

    #[test]
    fn fill_within_session_does_not_cross_overnight_gap() {
        let interval = Interval::parse("1h").unwrap().unwrap();
        let session = Session::new("America/New_York", "09:30-16:00").unwrap();
        let clock = BarClock::Session(session);
        let day0 = 1_704_205_800u32; // 2024-01-02 09:30 ET
        let day1 = day0 + 86_400; // 2024-01-03 09:30 ET (no DST change between)
        let ticks = vec![
            tick(day0, 10.0, 1),            // 09:30 day0
            tick(day0 + 3 * 3600, 11.0, 1), // 12:30 day0
            tick(day1, 12.0, 1),            // 09:30 day1
        ];
        let bars = resample(&ticks, interval, true, &clock);
        // Day0 fills 10:30/11:30 (between real bars) and 13:30/14:30/15:30 (tail up to the 16:00
        // close), then day1 starts fresh at 09:30 with NO overnight fill in between.
        let times: Vec<u32> = bars.iter().map(|b| b.time).collect();
        assert_eq!(
            times,
            vec![
                day0,
                day0 + 3600,
                day0 + 2 * 3600,
                day0 + 3 * 3600,
                day0 + 4 * 3600,
                day0 + 5 * 3600,
                day0 + 6 * 3600,
                day1,
            ]
        );
        assert_eq!(bars[1].volume, 0); // filled
        assert_eq!(bars[1].close, 10.0);
        assert_eq!(bars[3].volume, 1); // real 12:30
        assert_eq!(bars[6].volume, 0); // filled tail (15:30)
        assert_eq!(bars[7].volume, 1); // real 09:30 day1
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
