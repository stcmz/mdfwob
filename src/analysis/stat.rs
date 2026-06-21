//! Per-file tick summary statistics.

use crate::analysis::model::Tick;
use crate::analysis::session::Session;

/// One summary row for a tick file.
#[derive(Debug, Clone)]
pub struct StatRow {
    pub symbol: String,
    pub format: String,
    pub ticks: u64,
    pub first: Option<u32>,
    pub last: Option<u32>,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub vwap: f64,
    pub volume: i64,
    pub gaps: u64,
}

/// Computes the summary for one file's ticks.
///
/// Gaps are counted only between consecutive ticks on the **same** local day
/// (per `session`'s timezone) whose spacing exceeds `max_gap` seconds, so the
/// overnight/weekend inter-day boundary is never miscounted as a gap.
pub fn compute_stat(
    symbol: String,
    format: String,
    ticks: &[Tick],
    max_gap: u32,
    session: &Session,
) -> StatRow {
    if ticks.is_empty() {
        return StatRow {
            symbol,
            format,
            ticks: 0,
            first: None,
            last: None,
            min: f64::NAN,
            max: f64::NAN,
            mean: f64::NAN,
            vwap: f64::NAN,
            volume: 0,
            gaps: 0,
        };
    }

    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut price_sum = 0.0;
    let mut weighted = 0.0;
    let mut volume: i64 = 0;
    let mut gaps = 0u64;

    for (index, tick) in ticks.iter().enumerate() {
        min = min.min(tick.price);
        max = max.max(tick.price);
        price_sum += tick.price;
        weighted += tick.price * f64::from(tick.size);
        volume += i64::from(tick.size);

        if index > 0 {
            let prev = &ticks[index - 1];
            let delta = tick.time.saturating_sub(prev.time);
            if delta > max_gap && session.day_key(prev.time) == session.day_key(tick.time) {
                gaps += 1;
            }
        }
    }

    let count = ticks.len() as f64;
    let vwap = if volume != 0 {
        weighted / volume as f64
    } else {
        f64::NAN
    };

    StatRow {
        symbol,
        format,
        ticks: ticks.len() as u64,
        first: Some(ticks.first().unwrap().time),
        last: Some(ticks.last().unwrap().time),
        min,
        max,
        mean: price_sum / count,
        vwap,
        volume,
        gaps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(time: u32, price: f64, size: i32) -> Tick {
        Tick { time, price, size }
    }

    #[test]
    fn counts_intraday_gaps_only() {
        let session = Session::new("America/New_York", "00:00-24:00").unwrap();
        // 14:30Z, +120s (gap > 60), then next "day" jump (~24h) which must NOT count.
        let base = 1_704_205_800u32; // 2024-01-02 14:30Z
        let ticks = vec![
            tick(base, 10.0, 100),
            tick(base + 120, 10.5, 100), // intraday gap of 120s -> counts
            tick(base + 86_400 + 60, 11.0, 100), // next day -> inter-day, not counted
        ];
        let row = compute_stat("AAPL".into(), "fwob-v2".into(), &ticks, 60, &session);
        assert_eq!(row.gaps, 1);
        assert_eq!(row.ticks, 3);
        assert_eq!(row.volume, 300);
        assert_eq!(row.first, Some(base));
    }
}
