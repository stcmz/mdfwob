//! Input discovery and decoding of tick / bar FWOB files.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fwob::Reader;
use fwob_core::Key;

use crate::analysis::model::{Bar, Tick};
use crate::analysis::schema::{BAR_FRAME_TYPE, TICK_FRAME_LEN, TICK_FRAME_TYPE, decode_bar};
use crate::analysis::session::Session;
use crate::config::normalize_symbol;
use crate::tick::PRICE_SCALE;

/// Whether a file holds raw ticks or pre-aggregated bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Tick,
    Bar,
}

/// Optional time window and session filter applied while reading ticks.
#[derive(Debug, Clone, Default)]
pub struct TickQuery {
    pub start: Option<u32>,
    pub end: Option<u32>,
    pub session: Option<Session>,
}

/// Decodes a 12-byte `ShortTick` frame.
pub fn decode_tick(bytes: &[u8]) -> Tick {
    Tick {
        time: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        price: f64::from(u32::from_le_bytes(bytes[4..8].try_into().unwrap())) / PRICE_SCALE,
        size: i32::from_le_bytes(bytes[8..12].try_into().unwrap()),
    }
}

fn symbol_of(path: &Path, title: &str) -> String {
    if !title.is_empty() {
        title.to_owned()
    } else {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_owned()
    }
}

/// Identifies whether an open reader is a tick or bar file.
pub fn detect_kind(reader: &Reader) -> Result<InputKind> {
    let schema = reader.schema();
    match schema.frame_type.as_str() {
        TICK_FRAME_TYPE => Ok(InputKind::Tick),
        BAR_FRAME_TYPE => Ok(InputKind::Bar),
        other if schema.frame_len == TICK_FRAME_LEN => {
            let _ = other;
            Ok(InputKind::Tick)
        }
        other => bail!(
            "unsupported frame type {other:?}; expected a tick ({TICK_FRAME_TYPE}) or bar ({BAR_FRAME_TYPE}) file"
        ),
    }
}

/// Detects whether a file holds ticks or bars by opening its header.
pub fn input_kind(path: &Path) -> Result<InputKind> {
    let reader =
        Reader::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    detect_kind(&reader)
}

/// Opens a tick file, validating that it holds ticks (not bars). Returns the open reader and the
/// resolved symbol, ready for [`stream_ticks`].
pub fn open_tick_reader(path: &Path) -> Result<(Reader, String)> {
    let reader =
        Reader::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    if detect_kind(&reader)? != InputKind::Tick {
        bail!("{} is a bar file, not a tick file", path.display());
    }
    let symbol = symbol_of(path, reader.title());
    Ok((reader, symbol))
}

/// Reads ticks from a file, applying the time-range and session filters.
/// Returns the resolved symbol and the (ascending) ticks.
pub fn read_ticks(path: &Path, query: &TickQuery) -> Result<(String, Vec<Tick>)> {
    let (mut reader, symbol) = open_tick_reader(path)?;
    let mut ticks = Vec::new();
    stream_ticks(&mut reader, query, |tick| ticks.push(tick))?;
    Ok((symbol, ticks))
}

/// Target bulk-read size; reading frames in chunks avoids a per-frame heap allocation.
const READ_CHUNK_BYTES: usize = 256 * 1024;

/// Resolves the `[start, end)` frame index window for a key range, or `None` for an empty range.
fn index_window(
    reader: &mut Reader,
    start: Option<u32>,
    end: Option<u32>,
) -> Result<Option<(u64, u64)>> {
    let count = reader.frame_count();
    let (lo, hi) = match (start, end) {
        (Some(s), Some(e)) if s > e => return Ok(None),
        (Some(s), Some(e)) => (
            reader.lower_bound(Key::U32(s))?,
            reader.upper_bound(Key::U32(e))?,
        ),
        (Some(s), None) => (reader.lower_bound(Key::U32(s))?, count),
        (None, Some(e)) => (0, reader.upper_bound(Key::U32(e))?),
        (None, None) => (0, count),
    };
    Ok((lo < hi).then_some((lo, hi)))
}

/// Streams ticks from an open tick `reader` through `f`, applying the time-range and session
/// filters, reading in bulk chunks so no per-frame allocation occurs and the full tick set is
/// never materialized.
pub fn stream_ticks(reader: &mut Reader, query: &TickQuery, mut f: impl FnMut(Tick)) -> Result<()> {
    let session = query.session.as_ref();
    let Some((lo, hi)) = index_window(reader, query.start, query.end)? else {
        return Ok(());
    };
    let frame_len = reader.schema().frame_len as usize;
    let batch = (READ_CHUNK_BYTES / frame_len.max(1)).max(1);
    let mut index = lo;
    while index < hi {
        let want = ((hi - index) as usize).min(batch);
        let raw = reader.read_raw_frames_chunk(index, want)?;
        if raw.is_empty() {
            break;
        }
        for bytes in raw.chunks_exact(frame_len) {
            let tick = decode_tick(bytes);
            if session.is_none_or(|s| s.contains(tick.time)) {
                f(tick);
            }
        }
        index += (raw.len() / frame_len) as u64;
    }
    Ok(())
}

/// Reads a bar file in full. Returns the resolved symbol and bars.
pub fn read_bars(path: &Path) -> Result<(String, Vec<Bar>)> {
    let mut reader =
        Reader::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    if detect_kind(&reader)? != InputKind::Bar {
        bail!("{} is a tick file, not a bar file", path.display());
    }
    let symbol = symbol_of(path, reader.title());
    let count = reader.frame_count();
    let frame_len = reader.schema().frame_len as usize;
    let batch = (READ_CHUNK_BYTES / frame_len.max(1)).max(1);
    let mut bars = Vec::with_capacity(count as usize);
    let mut index = 0u64;
    while index < count {
        let want = ((count - index) as usize).min(batch);
        let raw = reader.read_raw_frames_chunk(index, want)?;
        if raw.is_empty() {
            break;
        }
        for bytes in raw.chunks_exact(frame_len) {
            bars.push(decode_bar(bytes)?);
        }
        index += (raw.len() / frame_len) as u64;
    }
    Ok((symbol, bars))
}

/// Expands positional tokens into concrete `*.fwob` paths.
///
/// Each token may be an existing file, an existing directory (its immediate
/// `*.fwob` files), or a bare symbol resolved to `<output_dir>/<symbol>.fwob`.
/// With no tokens, the immediate `*.fwob` files of the current directory are used.
pub fn discover_inputs(tokens: &[String], output_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if tokens.is_empty() {
        collect_dir(Path::new("."), &mut out)?;
        if out.is_empty() {
            bail!("no .fwob files found in the current directory");
        }
        return Ok(out);
    }
    for token in tokens {
        let path = Path::new(token);
        if path.is_dir() {
            collect_dir(path, &mut out)?;
        } else if path.is_file() {
            push_unique(&mut out, path.to_path_buf());
        } else if let Some(resolved) = resolve_symbol(token, output_dir) {
            push_unique(&mut out, resolved);
        } else {
            bail!(
                "no file, directory, or symbol matches {token:?} (looked under {})",
                output_dir.display()
            );
        }
    }
    Ok(out)
}

fn resolve_symbol(token: &str, output_dir: &Path) -> Option<PathBuf> {
    let exact = output_dir.join(format!("{token}.fwob"));
    if exact.is_file() {
        return Some(exact);
    }
    let normalized = output_dir.join(format!("{}.fwob", normalize_symbol(token)));
    normalized.is_file().then_some(normalized)
}

fn collect_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("failed to read directory {}", dir.display()))?
    {
        let path = entry?.path();
        if path.is_file() && has_fwob_extension(&path) {
            found.push(path);
        }
    }
    found.sort();
    for path in found {
        push_unique(out, path);
    }
    Ok(())
}

fn has_fwob_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("fwob"))
}

fn push_unique(out: &mut Vec<PathBuf>, path: PathBuf) {
    if !out.contains(&path) {
        out.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_tick_layout() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1_461_572_280u32.to_le_bytes());
        bytes.extend_from_slice(&1_052_200u32.to_le_bytes());
        bytes.extend_from_slice(&500i32.to_le_bytes());
        let tick = decode_tick(&bytes);
        assert_eq!(tick.time, 1_461_572_280);
        assert!((tick.price - 105.22).abs() < 1e-9);
        assert_eq!(tick.size, 500);
    }
}
