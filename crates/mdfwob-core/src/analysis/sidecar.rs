//! Incrementally maintained bar sidecars.
//!
//! Resampling a decade of ticks into daily bars costs ~50 s for a 1.5 GB tick file, and a research
//! loop pays it on every run. Materializing the bars once into `<SYMBOL>.<interval>.bars.fwob`
//! turns that into a seek, but only helps if refreshing is cheap: an archive that grows daily
//! cannot afford a full rebuild each time.
//!
//! [`refresh_sidecar`] appends only what is missing. It re-derives the **final** bucket rather than
//! trusting it, because a sidecar written mid-session captures a partial bar — and a partial bar
//! that silently persists is a data error no downstream check would catch.
//!
//! Sidecars store **raw** bars. Corporate-action adjustment is applied in memory at read time (see
//! [`crate::analysis::adjust`]), so a newly discovered split never invalidates a materialized file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fwob::{Editor, Reader};
use fwob_core::Key;

use crate::analysis::model::Bar;
use crate::analysis::output::FrameWriter;
use crate::analysis::read::{
    InputKind, input_kind, open_tick_reader, stream_bars_file, stream_ticks,
};
use crate::analysis::resample::{BarResampler, ForwardFiller, Resampler};
use crate::analysis::schema::{bar_schema, encode_bar};
use crate::analysis::{BarClock, Interval, TickQuery};

/// Filename for a symbol's materialized bars at one interval.
pub fn sidecar_name(symbol: &str, interval: Interval) -> String {
    format!("{symbol}.{}.bars.fwob", interval.label())
}

/// Path to a symbol's sidecar inside `dir`.
pub fn sidecar_path(dir: &Path, symbol: &str, interval: Interval) -> PathBuf {
    dir.join(sidecar_name(symbol, interval))
}

/// What a refresh did.
#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub symbol: String,
    pub path: PathBuf,
    /// Bars appended (for a rebuild, the whole series).
    pub appended: u64,
    /// Bars in the file afterwards.
    pub total: u64,
    /// True when the file was created or rewritten from scratch.
    pub rebuilt: bool,
    /// Timestamp of the last bar afterwards.
    pub last_time: Option<u32>,
}

/// The last key in a bar file, or `None` when the file is absent or empty.
fn last_bar_time(path: &Path) -> Result<Option<u32>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut reader =
        Reader::open(path).with_context(|| format!("failed to open sidecar {}", path.display()))?;
    if reader.frame_count() == 0 {
        return Ok(None);
    }
    Ok(match reader.last_key()? {
        Some(Key::U32(time)) => Some(time),
        _ => None,
    })
}

/// Number of frames in a bar file, or 0 when absent.
fn frame_count(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    Ok(Reader::open(path)?.frame_count())
}

/// Brings `<SYMBOL>.<interval>.bars.fwob` up to date with `source`.
///
/// Creates it when missing, otherwise drops the (possibly partial) last bucket and appends
/// everything from there on. `query`'s `start`/`end` bound the source scan; its session, if any,
/// filters ticks exactly as it does elsewhere.
pub fn refresh_sidecar(
    source: &Path,
    dir: &Path,
    symbol: &str,
    interval: Interval,
    clock: &BarClock,
    query: &TickQuery,
    fill: bool,
) -> Result<RefreshOutcome> {
    let path = sidecar_path(dir, symbol, interval);
    let resume = last_bar_time(&path)?;

    // Re-derive the last bucket: it may have been written while its session was still open.
    if let Some(time) = resume {
        let mut editor = Editor::open(&path)
            .with_context(|| format!("failed to open {} for trimming", path.display()))?;
        editor
            .delete_key(Key::U32(time))
            .with_context(|| format!("failed to trim the trailing bar from {}", path.display()))?;
    }

    // Scan from the resumed bucket, never before the caller's own window.
    let scan = TickQuery {
        start: match (query.start, resume) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        },
        end: query.end,
        session: query.session.clone(),
    };

    let mut bars: Vec<Bar> = Vec::new();
    match input_kind(source)? {
        InputKind::Tick => {
            let (mut reader, _) = open_tick_reader(source)?;
            let mut filler = ForwardFiller::new(interval, clock.clone(), fill, |bar| {
                bars.push(bar);
                Ok(())
            });
            let mut resampler = Resampler::new(interval, clock.clone());
            stream_ticks(&mut reader, &scan, |tick| {
                resampler.push(&tick, &mut |bar| filler.push(bar))
            })?;
            resampler.finish(&mut |bar| filler.push(bar))?;
        }
        InputKind::Bar => {
            let mut filler = ForwardFiller::new(interval, clock.clone(), fill, |bar| {
                bars.push(bar);
                Ok(())
            });
            let mut resampler = BarResampler::new(interval, clock.clone());
            stream_bars_file(source, &scan, |bar| {
                resampler.push(&bar, &mut |bar| filler.push(bar))
            })?;
            resampler.finish(&mut |bar| filler.push(bar))?;
        }
    }

    // Guard against a resumed scan re-emitting a bucket that is already stored.
    if let Some(time) = resume {
        bars.retain(|bar| bar.time >= time);
    }

    let rebuilt = resume.is_none();
    let appended = bars.len() as u64;
    if appended > 0 {
        let mut writer = if rebuilt {
            FrameWriter::create(&path, bar_schema(), symbol)?
        } else {
            FrameWriter::open_append(&path)?
        };
        for bar in &bars {
            writer.push(|buf| encode_bar(bar, buf))?;
        }
        writer.finish()?;
    } else if rebuilt {
        // No data at all: still create the file so its absence never masks an empty source.
        FrameWriter::create(&path, bar_schema(), symbol)?.finish()?;
    }

    Ok(RefreshOutcome {
        symbol: symbol.to_string(),
        last_time: last_bar_time(&path)?,
        total: frame_count(&path)?,
        appended,
        rebuilt,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::read::read_bars;
    use crate::tick::{Tick as RawTick, tick_schema};
    use fwob::Writer;
    use fwob_v2::WriterOptions;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mdfwob-sidecar-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes a tick file with one tick per `step` seconds starting at `start`.
    fn write_ticks(dir: &Path, symbol: &str, start: u32, count: u32, step: u32) -> PathBuf {
        let path = dir.join(format!("{symbol}.fwob"));
        let mut writer =
            Writer::create_v2(&path, tick_schema(), WriterOptions::new(symbol)).unwrap();
        let mut buf = Vec::new();
        for i in 0..count {
            buf.clear();
            RawTick::new(start + i * step, 100.0 + i as f64, 10)
                .unwrap()
                .encode(&mut buf);
            writer.append_frame(&buf).unwrap();
        }
        writer.finish().unwrap();
        path
    }

    fn hourly() -> Interval {
        Interval::parse("1h").unwrap().unwrap()
    }

    #[test]
    fn a_missing_sidecar_is_built_in_full() {
        let dir = temp_dir("build");
        // 10 hours of ticks, one every 10 minutes.
        let source = write_ticks(&dir, "TEST", 0, 60, 600);
        let out = refresh_sidecar(
            &source,
            &dir,
            "TEST",
            hourly(),
            &BarClock::Utc,
            &TickQuery::default(),
            false,
        )
        .unwrap();

        assert!(out.rebuilt);
        assert_eq!(out.total, 10, "ten hourly buckets");
        assert_eq!(out.appended, out.total);
        assert!(out.path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refreshing_an_up_to_date_sidecar_reproduces_the_same_series() {
        let dir = temp_dir("idem");
        let source = write_ticks(&dir, "TEST", 0, 60, 600);
        let q = TickQuery::default();
        refresh_sidecar(&source, &dir, "TEST", hourly(), &BarClock::Utc, &q, false).unwrap();
        let (_, first) = read_bars(&sidecar_path(&dir, "TEST", hourly())).unwrap();

        let out =
            refresh_sidecar(&source, &dir, "TEST", hourly(), &BarClock::Utc, &q, false).unwrap();
        let (_, second) = read_bars(&sidecar_path(&dir, "TEST", hourly())).unwrap();

        assert!(!out.rebuilt);
        assert_eq!(first.len(), second.len(), "refresh must not duplicate bars");
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(a.time, b.time);
            assert_eq!(a.close.to_bits(), b.close.to_bits());
            assert_eq!(a.volume, b.volume);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_incremental_refresh_matches_a_full_rebuild() {
        let dir = temp_dir("incr");

        // Materialize from a truncated source, then grow the source and refresh.
        let partial = write_ticks(&dir, "PART", 0, 30, 600);
        std::fs::rename(&partial, dir.join("TEST.fwob")).unwrap();
        let source = dir.join("TEST.fwob");
        let q = TickQuery::default();
        refresh_sidecar(&source, &dir, "TEST", hourly(), &BarClock::Utc, &q, false).unwrap();

        std::fs::remove_file(&source).unwrap();
        write_ticks(&dir, "TEST", 0, 60, 600);
        let out =
            refresh_sidecar(&source, &dir, "TEST", hourly(), &BarClock::Utc, &q, false).unwrap();
        assert!(!out.rebuilt, "should have resumed, not rebuilt");
        let (_, incremental) = read_bars(&sidecar_path(&dir, "TEST", hourly())).unwrap();

        // Ground truth: one full pass over the grown source.
        let full_dir = temp_dir("incr-full");
        write_ticks(&full_dir, "TEST", 0, 60, 600);
        refresh_sidecar(
            &full_dir.join("TEST.fwob"),
            &full_dir,
            "TEST",
            hourly(),
            &BarClock::Utc,
            &q,
            false,
        )
        .unwrap();
        let (_, full) = read_bars(&sidecar_path(&full_dir, "TEST", hourly())).unwrap();

        assert_eq!(incremental.len(), full.len());
        for (a, b) in incremental.iter().zip(&full) {
            assert_eq!(a.time, b.time);
            assert_eq!(a.open.to_bits(), b.open.to_bits(), "at {}", a.time);
            assert_eq!(a.high.to_bits(), b.high.to_bits(), "at {}", a.time);
            assert_eq!(a.low.to_bits(), b.low.to_bits(), "at {}", a.time);
            assert_eq!(a.close.to_bits(), b.close.to_bits(), "at {}", a.time);
            assert_eq!(a.volume, b.volume, "at {}", a.time);
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&full_dir);
    }

    /// The case a naive "append after the last bar" implementation gets wrong: the stored final
    /// bar covered a bucket that was still filling.
    #[test]
    fn a_partial_trailing_bar_is_rebuilt_not_kept() {
        let dir = temp_dir("partial");
        let q = TickQuery::default();

        // First pass sees only the first half of the final hour.
        write_ticks(&dir, "TEST", 0, 4, 600); // 0..1800s, all inside hour 0
        let source = dir.join("TEST.fwob");
        refresh_sidecar(&source, &dir, "TEST", hourly(), &BarClock::Utc, &q, false).unwrap();
        let (_, before) = read_bars(&sidecar_path(&dir, "TEST", hourly())).unwrap();
        assert_eq!(before.len(), 1);
        let partial_close = before[0].close;

        // The hour completes; the stored bar must be replaced, not preserved.
        std::fs::remove_file(&source).unwrap();
        write_ticks(&dir, "TEST", 0, 6, 600); // 0..3000s, still hour 0 but more ticks
        refresh_sidecar(&source, &dir, "TEST", hourly(), &BarClock::Utc, &q, false).unwrap();
        let (_, after) = read_bars(&sidecar_path(&dir, "TEST", hourly())).unwrap();

        assert_eq!(after.len(), 1, "still one bucket, not two");
        assert_ne!(
            after[0].close.to_bits(),
            partial_close.to_bits(),
            "the partial bar should have been re-derived"
        );
        assert_eq!(after[0].volume, 60, "six ticks of size 10");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
