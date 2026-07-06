//! Core value types shared across the analysis engine.

/// A single decoded trade tick.
///
/// `price` is the real price (already divided by [`crate::tick::PRICE_SCALE`]);
/// `time` is a UTC epoch second; `size` is the raw signed trade size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tick {
    pub time: u32,
    pub price: f64,
    pub size: i32,
}

/// An OHLCV bar produced by resampling ticks (or read back from a bar file).
///
/// `time` is the bucket-start UTC epoch second. `volume` is the raw signed sum
/// of tick sizes in the bucket. `vwap` is volume-weighted average price
/// (`NaN` when the summed size is zero).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bar {
    pub time: u32,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    pub vwap: f64,
    pub trades: u64,
}
