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

/// Reads ticks from a file, applying the time-range and session filters.
/// Returns the resolved symbol and the (ascending) ticks.
pub fn read_ticks(path: &Path, query: &TickQuery) -> Result<(String, Vec<Tick>)> {
    let mut reader =
        Reader::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    if detect_kind(&reader)? != InputKind::Tick {
        bail!("{} is a bar file, not a tick file", path.display());
    }
    let symbol = symbol_of(path, reader.title());
    let mut ticks = Vec::new();
    collect_ticks(&mut reader, query, &mut ticks)?;
    Ok((symbol, ticks))
}

fn collect_ticks(reader: &mut Reader, query: &TickQuery, out: &mut Vec<Tick>) -> Result<()> {
    let session = query.session.as_ref();
    let push = |bytes: &[u8], out: &mut Vec<Tick>| {
        let tick = decode_tick(bytes);
        if session.is_none_or(|s| s.contains(tick.time)) {
            out.push(tick);
        }
    };
    match (query.start, query.end) {
        (Some(s), Some(e)) if s > e => {}
        (Some(s), Some(e)) => {
            for frame in reader.frames_by_key(Key::U32(s)..=Key::U32(e))? {
                push(frame?.bytes(), out);
            }
        }
        (Some(s), None) => {
            for frame in reader.frames_after(Key::U32(s))? {
                push(frame?.bytes(), out);
            }
        }
        (None, Some(e)) => {
            for frame in reader.frames_before(Key::U32(e))? {
                push(frame?.bytes(), out);
            }
        }
        (None, None) => {
            let count = reader.frame_count();
            for frame in reader.frames(0..count)? {
                push(frame?.bytes(), out);
            }
        }
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
    let mut bars = Vec::with_capacity(count as usize);
    for frame in reader.frames(0..count)? {
        bars.push(decode_bar(frame?.bytes())?);
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
