//! FWOB schemas and (de)serialization for derived `bars` / `calc` output.

use anyhow::{Result, bail};
use fwob_core::{Field, FieldSemantic, FieldType, Schema, TimestampUnit};

use crate::analysis::model::Bar;
use crate::tick::PRICE_SCALE;

pub const TICK_FRAME_TYPE: &str = "ShortTick";
pub const BAR_FRAME_TYPE: &str = "Bar";
pub const CALC_FRAME_TYPE: &str = "Calc";

pub const TICK_FRAME_LEN: u32 = 12;
pub const BAR_FRAME_LEN: u32 = 40;

/// Scales a real price into the `FixedPoint(4)` integer representation.
pub fn scale_price(price: f64) -> u32 {
    if !price.is_finite() || price < 0.0 {
        return 0;
    }
    (price * PRICE_SCALE).round().min(u32::MAX as f64) as u32
}

/// Recovers a real price from the `FixedPoint(4)` integer representation.
pub fn unscale_price(raw: u32) -> f64 {
    f64::from(raw) / PRICE_SCALE
}

/// The OHLCV bar schema used by `bars --format fwob`.
pub fn bar_schema() -> Schema {
    Schema::new(
        BAR_FRAME_TYPE,
        vec![
            Field::new("Time", FieldType::UnsignedInteger, 4, 0)
                .with_semantic(FieldSemantic::UnixTimestamp(TimestampUnit::Seconds)),
            Field::new("Open", FieldType::UnsignedInteger, 4, 4)
                .with_semantic(FieldSemantic::FixedPoint(4)),
            Field::new("High", FieldType::UnsignedInteger, 4, 8)
                .with_semantic(FieldSemantic::FixedPoint(4)),
            Field::new("Low", FieldType::UnsignedInteger, 4, 12)
                .with_semantic(FieldSemantic::FixedPoint(4)),
            Field::new("Close", FieldType::UnsignedInteger, 4, 16)
                .with_semantic(FieldSemantic::FixedPoint(4)),
            Field::new("Volume", FieldType::SignedInteger, 8, 20),
            Field::new("VWAP", FieldType::UnsignedInteger, 4, 28)
                .with_semantic(FieldSemantic::FixedPoint(4)),
            Field::new("Trades", FieldType::UnsignedInteger, 8, 32),
        ],
        0,
    )
    .expect("static Bar schema is valid")
}

pub fn encode_bar(bar: &Bar, out: &mut Vec<u8>) {
    out.extend_from_slice(&bar.time.to_le_bytes());
    out.extend_from_slice(&scale_price(bar.open).to_le_bytes());
    out.extend_from_slice(&scale_price(bar.high).to_le_bytes());
    out.extend_from_slice(&scale_price(bar.low).to_le_bytes());
    out.extend_from_slice(&scale_price(bar.close).to_le_bytes());
    out.extend_from_slice(&bar.volume.to_le_bytes());
    out.extend_from_slice(&scale_price(bar.vwap).to_le_bytes());
    out.extend_from_slice(&bar.trades.to_le_bytes());
}

pub fn decode_bar(bytes: &[u8]) -> Result<Bar> {
    if bytes.len() < BAR_FRAME_LEN as usize {
        bail!("bar frame is too short: {} bytes", bytes.len());
    }
    let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    Ok(Bar {
        time: u32_at(0),
        open: unscale_price(u32_at(4)),
        high: unscale_price(u32_at(8)),
        low: unscale_price(u32_at(12)),
        close: unscale_price(u32_at(16)),
        volume: i64::from_le_bytes(bytes[20..28].try_into().unwrap()),
        vwap: unscale_price(u32_at(28)),
        trades: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
    })
}

/// The schema used by `calc --format fwob`: time, close, then one `f64` column
/// per indicator. `None` values are stored as `NaN`.
pub fn calc_schema(columns: &[String]) -> Result<Schema> {
    let mut fields = vec![
        Field::new("Time", FieldType::UnsignedInteger, 4, 0)
            .with_semantic(FieldSemantic::UnixTimestamp(TimestampUnit::Seconds)),
        Field::new("Close", FieldType::UnsignedInteger, 4, 4)
            .with_semantic(FieldSemantic::FixedPoint(4)),
    ];
    let mut offset = 8u32;
    for name in columns {
        fields.push(Field::new(name, FieldType::FloatingPoint, 8, offset));
        offset += 8;
    }
    Ok(Schema::new(CALC_FRAME_TYPE, fields, 0)?)
}

pub fn encode_calc_row(time: u32, close: f64, values: &[Option<f64>], out: &mut Vec<u8>) {
    out.extend_from_slice(&time.to_le_bytes());
    out.extend_from_slice(&scale_price(close).to_le_bytes());
    for value in values {
        let v = value.unwrap_or(f64::NAN);
        out.extend_from_slice(&v.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_round_trips() {
        let bar = Bar {
            time: 1_704_205_800,
            open: 185.0,
            high: 185.42,
            low: 184.88,
            close: 185.3,
            volume: 402_118,
            vwap: 185.21,
            trades: 1_204,
        };
        let mut bytes = Vec::new();
        encode_bar(&bar, &mut bytes);
        assert_eq!(bytes.len(), BAR_FRAME_LEN as usize);
        let decoded = decode_bar(&bytes).unwrap();
        assert_eq!(decoded.time, bar.time);
        assert!((decoded.close - bar.close).abs() < 1e-9);
        assert_eq!(decoded.volume, bar.volume);
        assert_eq!(decoded.trades, bar.trades);
    }

    #[test]
    fn calc_schema_offsets_are_contiguous() {
        let schema = calc_schema(&["sma_20".into(), "rsi_14".into()]).unwrap();
        assert_eq!(schema.frame_len, 8 + 8 + 8);
        assert_eq!(schema.fields.len(), 4);
    }
}
