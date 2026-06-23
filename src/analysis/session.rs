//! Trading-session windows defined in an exchange timezone (DST-correct via jiff).

use anyhow::{Context, Result, bail};
use jiff::{Timestamp, tz::TimeZone};

/// A daily session window (e.g. RTH `09:30-16:00 America/New_York`).
#[derive(Debug, Clone)]
pub struct Session {
    tz: TimeZone,
    start_min: i32,
    end_min: i32,
}

impl Session {
    /// Builds a session from an IANA timezone name and an `HH:MM-HH:MM` window.
    pub fn new(tz_name: &str, hours: &str) -> Result<Self> {
        let tz = TimeZone::get(tz_name).with_context(|| format!("unknown timezone {tz_name:?}"))?;
        let (start, end) = hours
            .split_once('-')
            .with_context(|| format!("session hours must be HH:MM-HH:MM, got {hours:?}"))?;
        let start_min = parse_hhmm(start.trim())?;
        let end_min = parse_hhmm(end.trim())?;
        if end_min <= start_min {
            bail!("session end {end} must be after start {start}");
        }
        Ok(Self {
            tz,
            start_min,
            end_min,
        })
    }

    /// Returns the local time-of-day (minutes from midnight) for an epoch second,
    /// plus a comparable local-day key (`year*10000 + month*100 + day`).
    fn local(&self, epoch: u32) -> Option<(i32, i64)> {
        let ts = Timestamp::from_second(i64::from(epoch)).ok()?;
        let zoned = ts.to_zoned(self.tz.clone());
        let minute_of_day = i32::from(zoned.hour()) * 60 + i32::from(zoned.minute());
        let day_key = i64::from(zoned.year()) * 10_000
            + i64::from(zoned.month()) * 100
            + i64::from(zoned.day());
        Some((minute_of_day, day_key))
    }

    /// Whether an epoch second falls inside the session window on its local day.
    pub fn contains(&self, epoch: u32) -> bool {
        match self.local(epoch) {
            Some((minute_of_day, _)) => {
                self.start_min <= minute_of_day && minute_of_day < self.end_min
            }
            None => false,
        }
    }

    /// A comparable local-day key, used to decide whether two ticks share a
    /// trading day (so inter-day gaps are not miscounted).
    pub fn day_key(&self, epoch: u32) -> i64 {
        self.local(epoch).map(|(_, day)| day).unwrap_or(i64::MIN)
    }

    /// The session's exchange timezone, used to anchor daily bar buckets.
    pub fn time_zone(&self) -> TimeZone {
        self.tz.clone()
    }

    /// The session's wall-clock length in seconds (e.g. 23 400 for RTH 09:30–16:00,
    /// 57 600 for extended 04:00–20:00). Constant across DST since the window is local.
    pub fn length_seconds(&self) -> u32 {
        ((self.end_min - self.start_min) * 60).max(0) as u32
    }

    /// Absolute UTC epoch of the session open on the local day containing `epoch` (e.g. the
    /// instant of 09:30 exchange-local on that date). Used to anchor sub-day bar buckets to the
    /// session open. `None` only if the timestamp/zoned conversion fails.
    pub fn open_epoch(&self, epoch: u32) -> Option<u32> {
        let date = Timestamp::from_second(i64::from(epoch))
            .ok()?
            .to_zoned(self.tz.clone())
            .date();
        let hour = i8::try_from(self.start_min / 60).ok()?;
        let minute = i8::try_from(self.start_min % 60).ok()?;
        let open = date.at(hour, minute, 0, 0).to_zoned(self.tz.clone()).ok()?;
        u32::try_from(open.timestamp().as_second()).ok()
    }
}

fn parse_hhmm(value: &str) -> Result<i32> {
    let (h, m) = value
        .split_once(':')
        .with_context(|| format!("time must be HH:MM, got {value:?}"))?;
    let hour: i32 = h
        .parse()
        .with_context(|| format!("bad hour in {value:?}"))?;
    let minute: i32 = m
        .parse()
        .with_context(|| format!("bad minute in {value:?}"))?;
    if !(0..=24).contains(&hour) || !(0..60).contains(&minute) {
        bail!("time {value:?} is out of range");
    }
    Ok(hour * 60 + minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2024-01-02 14:30:00Z == 09:30 America/New_York (winter, UTC-5).
    const JAN2_0930_ET: u32 = 1_704_205_800;

    #[test]
    fn rth_contains_open_excludes_premarket() {
        let rth = Session::new("America/New_York", "09:30-16:00").unwrap();
        assert!(rth.contains(JAN2_0930_ET));
        // One minute before the open (09:29 ET) is outside RTH.
        assert!(!rth.contains(JAN2_0930_ET - 60));
    }

    #[test]
    fn day_key_is_stable_within_a_day() {
        let rth = Session::new("America/New_York", "09:30-16:00").unwrap();
        let open = rth.day_key(JAN2_0930_ET);
        let later = rth.day_key(JAN2_0930_ET + 3_600);
        assert_eq!(open, later);
    }

    #[test]
    fn length_seconds_matches_window() {
        let rth = Session::new("America/New_York", "09:30-16:00").unwrap();
        assert_eq!(rth.length_seconds(), 23_400); // 6h30m
        let ext = Session::new("America/New_York", "04:00-20:00").unwrap();
        assert_eq!(ext.length_seconds(), 57_600); // 16h
    }

    #[test]
    fn open_epoch_is_local_session_open() {
        let rth = Session::new("America/New_York", "09:30-16:00").unwrap();
        // A mid-session tick (10:30 ET = JAN2_0930_ET + 1h) resolves to that day's 09:30 ET open.
        assert_eq!(rth.open_epoch(JAN2_0930_ET + 3_600), Some(JAN2_0930_ET));
        // The open instant maps to itself.
        assert_eq!(rth.open_epoch(JAN2_0930_ET), Some(JAN2_0930_ET));
        // A late-session tick on the same local day still maps back to the same open.
        assert_eq!(rth.open_epoch(JAN2_0930_ET + 6 * 3_600), Some(JAN2_0930_ET));
    }

    #[test]
    fn rejects_bad_hours() {
        assert!(Session::new("America/New_York", "0930-1600").is_err());
        assert!(Session::new("America/New_York", "16:00-09:30").is_err());
        assert!(Session::new("Not/AZone", "09:30-16:00").is_err());
    }
}
