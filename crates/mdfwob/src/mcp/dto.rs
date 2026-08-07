//! Wire types for the MCP tools.
//!
//! These exist so the protocol surface is decoupled from the analysis engine's internal structs,
//! and to enforce two things the engine does not:
//!
//! * **Times are ISO-8601 in the session timezone**, not raw epoch seconds. A model handed
//!   `1704207600` will happily invent a date for it.
//! * **Non-finite floats become `null`.** `Bar::vwap` is `NaN` when a bucket has no volume, and an
//!   empty [`StatRow`](mdfwob_core::analysis::stat::StatRow) carries infinities. JSON has no
//!   encoding for either — `serde_json` fails outright on them — so they are normalized here rather
//!   than crashing a tool call halfway through serialization.

use std::collections::BTreeMap;

use jiff::tz::TimeZone;
use schemars::JsonSchema;
use serde::Serialize;

use crate::analysis::inspect::{Inspection, field_semantic_label, field_type_label};
use crate::analysis::ls::LsRow;
use crate::analysis::model::Bar;
use crate::analysis::output::format_epoch_tz;
use crate::analysis::stat::StatRow;

/// JSON Schema for a non-negative integer, with no `format` annotation.
///
/// `schemars` tags Rust integers with their width — `format: "uint64"`, `"uint"`, `"uint16"`. Those
/// are not in the OpenAPI format registry that MCP clients validate schemas against, so each one
/// produces a warning on every schema fetch (`unknown format "uint64" ignored…`), enough of them to
/// bury the health-check output. The signed widths (`int32`/`int64`) *are* registered, so only the
/// unsigned ones need this; applying it uniformly keeps the rule "integers carry no format".
pub(crate) fn integer_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "integer", "minimum": 0 })
}

/// Same, for an optional field: the wire value may be `null`.
pub(crate) fn optional_integer_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": ["integer", "null"], "minimum": 0 })
}

/// Formats an epoch second in `tz`, e.g. `2024-01-02T09:30:00-05:00`.
pub(crate) fn iso(epoch: u32, tz: &TimeZone) -> String {
    format_epoch_tz(epoch, tz)
}

/// Same, for an optional boundary key.
pub(crate) fn iso_opt(epoch: Option<u32>, tz: &TimeZone) -> Option<String> {
    epoch.map(|e| iso(e, tz))
}

/// Maps `NaN`/`±inf` to `None` so the value survives JSON serialization.
pub(crate) fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

/// Envelope for a tool whose payload is a list.
///
/// MCP requires `outputSchema` to describe a JSON **object**, so a tool cannot advertise a
/// top-level array — a client that validates the schema (Claude Code does) rejects the whole tool
/// list. Every list-valued result is therefore wrapped in a named field, which also reads better
/// than a bare array on the receiving end.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct Listing<T> {
    pub items: Vec<T>,
}

impl<T> Listing<T> {
    pub(crate) fn new(items: Vec<T>) -> Self {
        Self { items }
    }
}

/// One row of `mdfwob_ls`.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct LsEntry {
    /// Path relative to the server root.
    pub file: String,
    pub symbol: String,
    /// `"tick"` or `"bar"`.
    pub kind: String,
    /// On-disk FWOB format, e.g. `"fwob-v2"`.
    pub format: String,
    #[schemars(schema_with = "integer_schema")]
    pub frame_count: u64,
    pub first: Option<String>,
    pub last: Option<String>,
    /// Detected bar interval (`"1m"`, `"1d"`, …); absent for tick files.
    pub granularity: Option<String>,
    /// `"rth"`, `"extended"`, or `"n/a"`.
    pub hours: String,
    #[schemars(schema_with = "integer_schema")]
    pub bytes: u64,
}

impl LsEntry {
    pub(crate) fn new(row: LsRow, tz: &TimeZone) -> Self {
        Self {
            file: row.file,
            symbol: row.symbol,
            kind: row.kind.to_owned(),
            format: row.format,
            frame_count: row.frame_count,
            first: iso_opt(row.first, tz),
            last: iso_opt(row.last, tz),
            granularity: row.granularity,
            hours: row.hours.to_owned(),
            bytes: row.bytes,
        }
    }
}

/// One row of `mdfwob_stat`.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct StatEntry {
    pub symbol: String,
    pub kind: String,
    pub format: String,
    #[schemars(schema_with = "integer_schema")]
    pub trades: u64,
    pub first: Option<String>,
    pub last: Option<String>,
    /// `null` when the file had no rows in range.
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub vwap: Option<f64>,
    pub volume: i64,
}

impl StatEntry {
    pub(crate) fn new(row: StatRow, tz: &TimeZone) -> Self {
        Self {
            symbol: row.symbol,
            kind: row.kind.to_owned(),
            format: row.format,
            trades: row.trades,
            first: iso_opt(row.first, tz),
            last: iso_opt(row.last, tz),
            min: finite(row.min),
            max: finite(row.max),
            vwap: finite(row.vwap),
            volume: row.volume,
        }
    }
}

/// One OHLCV bar. Prices are real prices — the analysis engine already unscales them.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct BarRow {
    /// Bucket start, ISO-8601 in the session timezone.
    pub time: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    /// `null` when the bucket had no volume to weight by.
    pub vwap: Option<f64>,
    #[schemars(schema_with = "integer_schema")]
    pub trades: u64,
}

impl BarRow {
    pub(crate) fn new(bar: &Bar, tz: &TimeZone) -> Self {
        Self {
            time: iso(bar.time, tz),
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
            volume: bar.volume,
            vwap: finite(bar.vwap),
            trades: bar.trades,
        }
    }
}

/// One bar plus its indicator values. A value is `null` during an indicator's warm-up.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct CalcRow {
    pub time: String,
    pub close: f64,
    pub values: BTreeMap<String, Option<f64>>,
}

/// A per-symbol series, carrying the truncation contract.
///
/// `returned` is what is in `rows`; `total` is how many bars the query actually produced. When
/// they differ, `rows` holds the **most recent** `returned` of them — a model asking about a long
/// history almost always means "and what happened lately", and the indicators in a
/// [`CalcRow`] are still computed over the full series, so warm-up is correct either way.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct Series<T> {
    pub symbol: String,
    /// Bar interval the rows were resampled to, e.g. `"1d"`.
    pub interval: String,
    pub timezone: String,
    #[schemars(schema_with = "integer_schema")]
    pub total: usize,
    #[schemars(schema_with = "integer_schema")]
    pub returned: usize,
    pub truncated: bool,
    /// Present only when `truncated`, explaining how to get a complete answer.
    pub hint: Option<String>,
    pub rows: Vec<T>,
}

/// The `[file]`/`[storage]`/`[range]`/`[schema]` view of one file, as JSON.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct InspectReport {
    pub file: String,
    pub symbol: String,
    pub kind: String,
    pub format: String,
    pub frame_type: String,
    #[schemars(schema_with = "integer_schema")]
    pub frame_count: u64,
    #[schemars(schema_with = "integer_schema")]
    pub bytes: u64,
    pub timezone: String,
    pub first: Option<String>,
    pub last: Option<String>,
    pub granularity: Option<String>,
    pub hours: String,
    pub fields: Vec<FieldInfo>,
    /// A short head/tail sample of decoded frames.
    pub preview: String,
}

impl InspectReport {
    /// Renders a core [`Inspection`] as the wire form: labels for the enums, ISO-8601 for the
    /// boundary keys, and the schema flattened into per-column entries.
    pub(crate) fn new(report: &Inspection, file: String, tz: &TimeZone) -> Self {
        Self {
            file,
            symbol: report.symbol.clone(),
            kind: report.kind_label().to_owned(),
            format: report.format_label().to_owned(),
            frame_type: report.schema.frame_type.clone(),
            frame_count: report.frame_count,
            bytes: report.physical_bytes,
            timezone: tz.iana_name().unwrap_or("UTC").to_owned(),
            first: iso_opt(report.first, tz),
            last: iso_opt(report.last, tz),
            granularity: report.granularity.clone(),
            // A file with no frames has nothing to classify; the CLI omits the key entirely, and
            // JSON says so explicitly rather than leaving the field absent.
            hours: report.hours.unwrap_or("n/a").to_owned(),
            fields: report
                .schema
                .fields
                .iter()
                .map(|field| FieldInfo {
                    name: field.name.clone(),
                    field_type: field_type_label(field.field_type).to_owned(),
                    length: field.length,
                    offset: field.offset,
                    semantic: field_semantic_label(field.semantic),
                })
                .collect(),
            preview: report.preview.clone(),
        }
    }
}

/// One column of the on-disk schema.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct FieldInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
    #[schemars(schema_with = "integer_schema")]
    pub length: u16,
    #[schemars(schema_with = "integer_schema")]
    pub offset: u32,
    pub semantic: String,
}

/// Structural integrity plus the scanned market summary.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct VerifyReport {
    pub file: String,
    pub symbol: String,
    pub kind: String,
    /// Always `"ok"` — a failed verification is returned as a tool error instead.
    pub status: String,
    #[schemars(schema_with = "integer_schema")]
    pub frame_count: u64,
    pub data: StatEntry,
}
