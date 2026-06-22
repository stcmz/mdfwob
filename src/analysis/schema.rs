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

/// Sentinel stored for an absent (`None`) calc value. It is the minimum of the column's
/// signed integer type, which fwob's formatter renders as `-` / `null` / empty.
pub const CALC_NULL: i32 = i32::MIN;

/// Scales a finite value into an `i32` fixed-point representation with `decimals` fractional
/// digits, clamped to the representable range (never the null sentinel).
pub fn scale_fixed(value: f64, decimals: u8) -> i32 {
    let scaled = (value * 10f64.powi(i32::from(decimals))).round();
    if scaled >= f64::from(i32::MAX) {
        i32::MAX
    } else if scaled <= f64::from(i32::MIN + 1) {
        i32::MIN + 1
    } else {
        scaled as i32
    }
}

/// Builds a render-only display schema that prepends a 1-byte `Symbol` string-table column to
/// `base`, shifting every field by one byte. Used only for multi-symbol stdout rendering; the
/// resulting frames are never written to a file.
pub fn with_symbol_column(base: &Schema) -> Schema {
    let mut fields = Vec::with_capacity(base.fields.len() + 1);
    fields.push(Field::new("Symbol", FieldType::StringTableIndex, 1, 0));
    for field in &base.fields {
        let mut shifted = field.clone();
        shifted.offset += 1;
        fields.push(shifted);
    }
    Schema::new(base.frame_type.clone(), fields, base.key_field_index + 1)
        .expect("symbol-prefixed display schema is valid")
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
            // FixedPoint(0) is display-only: it comma-groups the integer (and never nulls,
            // since Volume is never i64::MIN).
            Field::new("Volume", FieldType::SignedInteger, 8, 20)
                .with_semantic(FieldSemantic::FixedPoint(0)),
            Field::new("VWAP", FieldType::UnsignedInteger, 4, 28)
                .with_semantic(FieldSemantic::FixedPoint(4)),
            Field::new("Trades", FieldType::UnsignedInteger, 8, 32)
                .with_semantic(FieldSemantic::FixedPoint(0)),
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

/// The schema used by `calc`: time, close, then one 4-byte `i32` fixed-point column per
/// indicator (precision `decimals[i]`). Absent values are stored as [`CALC_NULL`].
pub fn calc_schema(names: &[String], decimals: &[u8]) -> Result<Schema> {
    assert_eq!(names.len(), decimals.len(), "names and decimals must align");
    let mut fields = vec![
        Field::new("Time", FieldType::UnsignedInteger, 4, 0)
            .with_semantic(FieldSemantic::UnixTimestamp(TimestampUnit::Seconds)),
        Field::new("Close", FieldType::UnsignedInteger, 4, 4)
            .with_semantic(FieldSemantic::FixedPoint(4)),
    ];
    let mut offset = 8u32;
    for (name, &points) in names.iter().zip(decimals) {
        fields.push(
            Field::new(name, FieldType::SignedInteger, 4, offset)
                .with_semantic(FieldSemantic::FixedPoint(points)),
        );
        offset += 4;
    }
    Ok(Schema::new(CALC_FRAME_TYPE, fields, 0)?)
}

pub fn encode_calc_row(
    time: u32,
    close: f64,
    values: &[Option<f64>],
    decimals: &[u8],
    out: &mut Vec<u8>,
) {
    out.extend_from_slice(&time.to_le_bytes());
    out.extend_from_slice(&scale_price(close).to_le_bytes());
    for (value, &points) in values.iter().zip(decimals) {
        let raw = match value {
            Some(v) if v.is_finite() => scale_fixed(*v, points),
            _ => CALC_NULL,
        };
        out.extend_from_slice(&raw.to_le_bytes());
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
        let schema = calc_schema(&["sma_20".into(), "rsi_14".into()], &[4, 8]).unwrap();
        assert_eq!(schema.frame_len, 8 + 4 + 4);
        assert_eq!(schema.fields.len(), 4);
        assert_eq!(
            schema.fields[3].semantic,
            FieldSemantic::FixedPoint(8),
            "indicator precision should be carried into the schema"
        );
    }

    #[test]
    fn scale_fixed_rounds_and_clamps() {
        assert_eq!(scale_fixed(1.23456789, 8), 123_456_789);
        assert_eq!(scale_fixed(184.7, 4), 1_847_000);
        // Overflow clamps to the representable range, never the null sentinel.
        assert_eq!(scale_fixed(1e9, 8), i32::MAX);
        assert_eq!(scale_fixed(-1e9, 8), i32::MIN + 1);
        assert_ne!(scale_fixed(-1e9, 8), CALC_NULL);
    }

    #[test]
    fn symbol_display_schema_prepends_and_shifts() {
        let display = with_symbol_column(&bar_schema());
        assert_eq!(display.fields[0].name, "Symbol");
        assert_eq!(display.fields[0].offset, 0);
        assert_eq!(display.fields[1].name, "Time");
        assert_eq!(display.fields[1].offset, 1);
        assert_eq!(display.frame_len, BAR_FRAME_LEN + 1);
    }
}
