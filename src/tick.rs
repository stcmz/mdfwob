use anyhow::{Result, bail};
use fwob_core::{Field, FieldSemantic, FieldType, Schema, TimestampUnit};

pub const PRICE_SCALE: f64 = 10_000.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShortTick {
    pub time: u32,
    pub price: u32,
    pub size: i32,
}

impl ShortTick {
    pub fn new(time: u32, price: f64, size: i32) -> Result<Self> {
        if !price.is_finite() || price < 0.0 {
            bail!("price must be a non-negative finite number");
        }
        let scaled = (price * PRICE_SCALE).round();
        if scaled > u32::MAX as f64 {
            bail!("scaled price exceeds u32");
        }
        Ok(Self {
            time,
            price: scaled as u32,
            size,
        })
    }

    pub fn encode(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.time.to_le_bytes());
        out.extend_from_slice(&self.price.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
    }
}

pub fn tick_schema() -> Schema {
    Schema::new(
        "Tick",
        vec![
            // time is an absolute UTC epoch second (see downloader::provider_tick_to_short_tick).
            // V2 persists this semantic; V1 accepts but does not store it (reads back as None).
            Field::new("time", FieldType::UnsignedInteger, 4, 0)
                .with_semantic(FieldSemantic::UnixTimestamp(TimestampUnit::Seconds)),
            // price is stored as the real price * PRICE_SCALE (10,000), i.e. 4 decimal places.
            // V2 persists this semantic so `fwob cat` shows the real price; V1 reads it back as None.
            Field::new("price", FieldType::UnsignedInteger, 4, 4)
                .with_semantic(FieldSemantic::FixedPoint(4)),
            Field::new("size", FieldType::SignedInteger, 4, 8),
        ],
        0,
    )
    .expect("static Tick schema is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_legacy_short_tick_layout() {
        let tick = ShortTick::new(1_461_572_280, 105.22, 500).unwrap();
        let mut bytes = Vec::new();
        tick.encode(&mut bytes);
        assert_eq!(bytes.len(), 12);
        assert_eq!(&bytes[0..4], &1_461_572_280u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &1_052_200u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &500i32.to_le_bytes());
    }
}
