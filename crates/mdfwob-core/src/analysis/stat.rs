//! Per-file summary statistics, computed identically from tick or bar inputs.

use crate::analysis::model::{Bar, Tick};

/// One summary row for a tick or bar file.
///
/// Every field is derivable from either format, so summarizing a tick file and summarizing the
/// bars it resamples into yields the same `min`/`max`/`vwap`/`volume`/`trades`. `kind` records
/// which format the row was read from (`"tick"` or `"bar"`).
#[derive(Debug, Clone)]
pub struct StatRow {
    pub symbol: String,
    pub kind: &'static str,
    pub format: String,
    pub trades: u64,
    pub first: Option<u32>,
    pub last: Option<u32>,
    pub min: f64,
    pub max: f64,
    pub vwap: f64,
    pub volume: i64,
}

/// Incrementally accumulates summary statistics from a stream of ascending ticks or bars, so a
/// file's rows never need to be materialized all at once. Ticks and bars feed the same fields:
/// a tick contributes its price as both the low and high and one trade; a bar contributes its
/// low/high, its volume-weighted price mass (`vwap * volume`), and its trade count.
#[derive(Default)]
pub struct StatAccumulator {
    trades: u64,
    first: Option<u32>,
    last: u32,
    min: f64,
    max: f64,
    weighted: f64,
    volume: i64,
}

impl StatAccumulator {
    pub fn new() -> Self {
        Self {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            ..Default::default()
        }
    }

    pub fn push_tick(&mut self, tick: &Tick) {
        self.observe(
            tick.time,
            tick.price,
            tick.price,
            i64::from(tick.size),
            tick.price * f64::from(tick.size),
            1,
        );
    }

    pub fn push_bar(&mut self, bar: &Bar) {
        // A bar's `vwap * volume` is exactly the sum of `price * size` over the ticks it covers, so
        // accumulating it reconstructs the overall VWAP. Zero-volume (empty) bars contribute no
        // price mass but still extend the low/high with their flat price.
        let weighted = if bar.vwap.is_finite() {
            bar.vwap * bar.volume as f64
        } else {
            0.0
        };
        self.observe(
            bar.time, bar.low, bar.high, bar.volume, weighted, bar.trades,
        );
    }

    fn observe(&mut self, time: u32, low: f64, high: f64, volume: i64, weighted: f64, trades: u64) {
        if self.first.is_none() {
            self.first = Some(time);
        }
        self.last = time;
        self.trades += trades;
        self.min = self.min.min(low);
        self.max = self.max.max(high);
        self.weighted += weighted;
        self.volume += volume;
    }

    pub fn finish(self, symbol: String, kind: &'static str, format: String) -> StatRow {
        if self.trades == 0 && self.first.is_none() {
            return StatRow {
                symbol,
                kind,
                format,
                trades: 0,
                first: None,
                last: None,
                min: f64::NAN,
                max: f64::NAN,
                vwap: f64::NAN,
                volume: 0,
            };
        }
        let vwap = if self.volume != 0 {
            self.weighted / self.volume as f64
        } else {
            f64::NAN
        };
        StatRow {
            symbol,
            kind,
            format,
            trades: self.trades,
            first: self.first,
            last: Some(self.last),
            min: self.min,
            max: self.max,
            vwap,
            volume: self.volume,
        }
    }
}

/// Computes the summary for one file's ticks (convenience wrapper over [`StatAccumulator`]).
pub fn compute_stat(symbol: String, format: String, ticks: &[Tick]) -> StatRow {
    let mut acc = StatAccumulator::new();
    for tick in ticks {
        acc.push_tick(tick);
    }
    acc.finish(symbol, "tick", format)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(time: u32, price: f64, size: i32) -> Tick {
        Tick { time, price, size }
    }

    #[test]
    fn tick_and_bar_stats_agree() {
        // Two 1s ticks in one bucket, one in the next.
        let ticks = [
            tick(100, 10.0, 100),
            tick(100, 12.0, 300),
            tick(160, 11.0, 200),
        ];
        let mut tick_acc = StatAccumulator::new();
        for t in &ticks {
            tick_acc.push_tick(t);
        }
        let tick_row = tick_acc.finish("AAPL".into(), "tick", "fwob-v2".into());

        // The equivalent bars: bucket [100] has 2 trades, vol 400, vwap (10*100+12*300)/400 = 11.5,
        // low 10 high 12; bucket [160] has 1 trade, vol 200, vwap 11, low/high 11.
        let bars = [
            Bar {
                time: 100,
                open: 10.0,
                high: 12.0,
                low: 10.0,
                close: 12.0,
                volume: 400,
                vwap: 11.5,
                trades: 2,
            },
            Bar {
                time: 160,
                open: 11.0,
                high: 11.0,
                low: 11.0,
                close: 11.0,
                volume: 200,
                vwap: 11.0,
                trades: 1,
            },
        ];
        let mut bar_acc = StatAccumulator::new();
        for b in &bars {
            bar_acc.push_bar(b);
        }
        let bar_row = bar_acc.finish("AAPL".into(), "bar", "fwob-v2".into());

        assert_eq!(tick_row.trades, bar_row.trades);
        assert_eq!(tick_row.volume, bar_row.volume);
        assert!((tick_row.min - bar_row.min).abs() < 1e-9);
        assert!((tick_row.max - bar_row.max).abs() < 1e-9);
        assert!((tick_row.vwap - bar_row.vwap).abs() < 1e-9);
        assert_eq!(tick_row.first, bar_row.first);
        assert_eq!(tick_row.last, bar_row.last);
        assert_eq!(tick_row.kind, "tick");
        assert_eq!(bar_row.kind, "bar");
        // trades = total underlying ticks either way
        assert_eq!(tick_row.trades, 3);
    }
}
