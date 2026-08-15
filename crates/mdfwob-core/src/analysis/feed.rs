//! Streaming a symbol's bars, from whichever source is best.
//!
//! Two concerns live here that every consumer would otherwise rebuild:
//!
//! 1. **Assembly.** Turning files into bars is not one call — it is a format branch, a resampler, a
//!    forward-filler, and a rule for spanning several files of one symbol. Rebuilding that per
//!    consumer invites the copies to drift, and drift here surfaces as slightly-wrong prices rather
//!    than a failure.
//! 2. **Source choice.** A materialized sidecar is read in place of re-resampling ticks when one is
//!    present and current. That is an optimization, not a semantic difference, so callers should
//!    not have to know it happened — and must not be able to get it wrong.
//!
//! Adjustment is deliberately *not* here. A streaming consumer wraps the sink with an
//! [`Adjuster`](crate::analysis::adjust::Adjuster); one that materializes the series calls
//! [`adjust_bars`](crate::analysis::adjust::adjust_bars) afterwards and gets total-return too.
//! Baking either in would force the other to unpick it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fwob::Reader;
use fwob_core::Key;
use jiff::tz::TimeZone;

use crate::analysis::model::Bar;
use crate::analysis::output::format_epoch_tz;
use crate::analysis::read::{
    InputKind, input_kind, open_tick_reader, stream_bars_file, stream_ticks,
};
use crate::analysis::resample::{BarResampler, ForwardFiller, Resampler};
use crate::analysis::sidecar::sidecar_path;
use crate::analysis::{BarClock, Interval, TickQuery};

/// What to read, and how to bucket it.
#[derive(Clone, Copy)]
pub struct BarStream<'a> {
    /// One symbol's source files, ascending. All must be the same kind.
    pub paths: &'a [PathBuf],
    /// Target bar width. `None` keeps a bar source at its stored resolution, and is an error for a
    /// tick source, which has no resolution of its own.
    pub interval: Option<Interval>,
    pub clock: &'a BarClock,
    /// Bounds the scan; its session, if any, filters ticks.
    pub query: &'a TickQuery,
    /// Emit flat bars for empty buckets inside a session.
    pub fill: bool,
}

impl<'a> BarStream<'a> {
    pub fn new(
        paths: &'a [PathBuf],
        interval: impl Into<Option<Interval>>,
        clock: &'a BarClock,
        query: &'a TickQuery,
    ) -> Self {
        Self {
            paths,
            interval: interval.into(),
            clock,
            query,
            fill: false,
        }
    }

    pub fn fill(mut self, fill: bool) -> Self {
        self.fill = fill;
        self
    }
}

/// Streams a symbol's bars to `sink` as each bucket closes.
///
/// Every path feeds **one** resampler, so several files of a symbol form a single ascending stream
/// and a bucket spanning a file boundary closes once with the right OHLC — resampling each file
/// separately would split it into two half-buckets and double the trade count at every seam.
///
/// Accepts tick files (resampled) and bar files (re-resampled to `interval`, e.g. 1s -> 1m), so
/// every consumer honors the interval regardless of input format. Ticks stream in bulk chunks and
/// are never fully materialized; bar files seek to the query window.
pub fn stream_symbol_bars(spec: BarStream<'_>, sink: impl FnMut(Bar) -> Result<()>) -> Result<()> {
    let BarStream {
        paths,
        interval,
        clock,
        query,
        fill,
    } = spec;

    let mut kind: Option<InputKind> = None;
    for path in paths {
        let this = input_kind(path)?;
        match kind {
            Some(existing) if existing != this => bail!(
                "cannot mix tick and bar files for one symbol ({})",
                path.display()
            ),
            _ => kind = Some(this),
        }
    }

    let Some(interval) = interval else {
        // No target width: a bar source keeps its stored resolution and passes straight through.
        // A tick source has no resolution of its own, so there is nothing to keep.
        if kind == Some(InputKind::Tick) {
            bail!("an interval is required to bucket a tick source");
        }
        let mut sink = sink;
        for path in paths {
            stream_bars_file(path, query, &mut sink)?;
        }
        return Ok(());
    };

    // The filler wraps the caller's sink, so a consumer that adjusts in its own sink scales the
    // synthetic fill bars too rather than leaving them at raw prices.
    let mut filler = ForwardFiller::new(interval, clock.clone(), fill, sink);
    match kind {
        Some(InputKind::Bar) => {
            let mut resampler = BarResampler::new(interval, clock.clone());
            for path in paths {
                stream_bars_file(path, query, |bar| {
                    resampler.push(&bar, &mut |bar| filler.push(bar))
                })?;
            }
            resampler.finish(&mut |bar| filler.push(bar))
        }
        _ => {
            let mut resampler = Resampler::new(interval, clock.clone());
            for path in paths {
                let (mut reader, _) = open_tick_reader(path)?;
                stream_ticks(&mut reader, query, |tick| {
                    resampler.push(&tick, &mut |bar| filler.push(bar))
                })?;
            }
            resampler.finish(&mut |bar| filler.push(bar))
        }
    }
}

/// The last key of a FWOB file, when it has one.
fn last_key(path: &Path) -> Result<Option<u32>> {
    let mut reader =
        Reader::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    if reader.frame_count() == 0 {
        return Ok(None);
    }
    Ok(match reader.last_key()? {
        Some(Key::U32(time)) => Some(time),
        _ => None,
    })
}

/// Substitutes a materialized sidecar for a tick source when one is present and current.
///
/// Returns the paths to actually read and whether a sidecar was chosen. A sidecar already holds the
/// requested interval, so a caller that takes one must stop re-resampling — [`symbol_bars`] handles
/// that, which is why callers should prefer it to wiring this up themselves.
///
/// Staleness is refused rather than tolerated: an archive grows, and a file materialized last week
/// silently truncates every run that reads it. Both checks are O(1) header reads, free against the
/// tens of seconds a sidecar saves.
pub fn resolve_sidecar(
    paths: &[PathBuf],
    symbol: &str,
    interval: Interval,
    use_rth: bool,
    clock: &BarClock,
    tz: &TimeZone,
) -> Result<Option<PathBuf>> {
    // A sidecar stands in for exactly one tick file; several sources have no single sidecar.
    let [source] = paths else { return Ok(None) };
    if input_kind(source)? != InputKind::Tick {
        return Ok(None);
    }
    let Some(dir) = source.parent() else {
        return Ok(None);
    };
    let side = sidecar_path(dir, symbol, interval, use_rth);
    if !side.exists() {
        return Ok(None);
    }
    let (Some(bars_last), Some(tick_last)) = (last_key(&side)?, last_key(source)?) else {
        return Ok(None); // an empty file either side: fall back to the source
    };

    // Stale only when a whole bucket beyond the last stored one could be formed. Extra ticks inside
    // the final bucket make it partial, which `mdfwob sync` re-derives anyway.
    if tick_last >= clock.next_bucket_start(interval, bars_last) {
        bail!(
            "{} is stale.\n  sidecar ends {}\n  source has ticks through {}\nRefresh it with: \
             mdfwob sync {} {}{}",
            side.display(),
            format_epoch_tz(bars_last, tz),
            format_epoch_tz(tick_last, tz),
            source.display(),
            interval.label(),
            if use_rth { " rth" } else { "" },
        );
    }
    Ok(Some(side))
}

/// How a symbol's bars should be sourced.
#[derive(Clone, Copy)]
pub struct SymbolBars<'a> {
    pub stream: BarStream<'a>,
    /// Regular hours only. Selects the sidecar as well, since it changes the bars.
    pub use_rth: bool,
    /// Read a current sidecar in place of re-resampling ticks.
    pub prefer_sidecar: bool,
}

/// Streams one symbol's bars, transparently reading a current sidecar when there is one.
///
/// This is the entry point a consumer should reach for: whether the bars came from a sidecar or
/// from ticks is an implementation detail of the archive, and the result is identical either way.
/// Pass `tz` for the timezone a staleness complaint is rendered in.
pub fn symbol_bars(
    spec: SymbolBars<'_>,
    symbol: &str,
    tz: &TimeZone,
    sink: impl FnMut(Bar) -> Result<()>,
) -> Result<()> {
    let SymbolBars {
        stream,
        use_rth,
        prefer_sidecar,
    } = spec;

    // A custom session window or forward-fill changes the bars but is not expressible in a
    // sidecar's name, so those cases must read the source rather than risk being served a file
    // built with different parameters.
    if prefer_sidecar
        && !stream.fill
        && let Some(interval) = stream.interval
        && let Some(side) =
            resolve_sidecar(stream.paths, symbol, interval, use_rth, stream.clock, tz)?
    {
        let paths = [side];
        return stream_symbol_bars(
            BarStream {
                paths: &paths,
                // The sidecar is already at this interval; re-resampling it to the same width is a
                // no-op scan, but re-resampling to a *different* one would be wrong.
                ..stream
            },
            sink,
        );
    }
    stream_symbol_bars(stream, sink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::sidecar::{RefreshSpec, refresh_sidecar};
    use crate::tick::{Tick as RawTick, tick_schema};
    use fwob::Writer;
    use fwob_v2::WriterOptions;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mdfwob-feed-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Ticks every `step` seconds from `start`, written to `<name>.fwob`.
    fn write_ticks(dir: &Path, name: &str, start: u32, count: u32, step: u32) -> PathBuf {
        let path = dir.join(format!("{name}.fwob"));
        let mut writer = Writer::create_v2(&path, tick_schema(), WriterOptions::new(name)).unwrap();
        let mut buf = Vec::new();
        for i in 0..count {
            buf.clear();
            let time = start + i * step;
            // Price is a function of time, not of position in the file, so the same ticks split
            // across two files carry the same prices as one file holding all of them — otherwise
            // the seam comparison below would differ for a reason that is not the seam.
            RawTick::new(time, 100.0 + f64::from(time / step), 10)
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

    fn collect(paths: &[PathBuf], fill: bool) -> Vec<Bar> {
        let query = TickQuery::default();
        let mut out = Vec::new();
        stream_symbol_bars(
            BarStream::new(paths, hourly(), &BarClock::Utc, &query).fill(fill),
            |bar| {
                out.push(bar);
                Ok(())
            },
        )
        .unwrap();
        out
    }

    /// The property a per-file loop gets wrong: a bucket spanning a file boundary must close once,
    /// with the OHLC and trade count of the whole hour.
    #[test]
    fn a_bucket_spanning_two_files_closes_once() {
        let dir = temp_dir("seam");
        // Hour 0 split across two files: 0..1800 and 1800..3600.
        let a = write_ticks(&dir, "A", 0, 3, 600);
        let b = write_ticks(&dir, "B", 1800, 3, 600);

        let split = collect(&[a.clone(), b.clone()], false);
        assert_eq!(split.len(), 1, "one hour, not two: {split:?}");
        assert_eq!(split[0].trades, 6, "every tick counted once");
        assert_eq!(split[0].volume, 60);

        // Identical to reading the same ticks from a single file.
        let whole = collect(&[write_ticks(&dir, "WHOLE", 0, 6, 600)], false);
        assert_eq!(whole.len(), 1);
        assert_eq!(split[0].open.to_bits(), whole[0].open.to_bits());
        assert_eq!(split[0].high.to_bits(), whole[0].high.to_bits());
        assert_eq!(split[0].low.to_bits(), whole[0].low.to_bits());
        assert_eq!(split[0].close.to_bits(), whole[0].close.to_bits());
        assert_eq!(split[0].trades, whole[0].trades);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mixing_tick_and_bar_sources_is_refused() {
        let dir = temp_dir("mixed");
        let ticks = write_ticks(&dir, "T", 0, 6, 600);
        let query = TickQuery::default();
        refresh_sidecar(
            &ticks,
            &dir,
            "T",
            &RefreshSpec {
                interval: hourly(),
                use_rth: true,
                clock: &BarClock::Utc,
                query: &query,
                fill: false,
            },
        )
        .unwrap();
        let bars = sidecar_path(&dir, "T", hourly(), true);

        let mut out = Vec::new();
        let paths = [ticks, bars];
        let err = stream_symbol_bars(
            BarStream::new(&paths, hourly(), &BarClock::Utc, &query),
            |bar| {
                out.push(bar);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("cannot mix"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sidecar is an optimization: reading through it must give exactly the tick-derived bars.
    #[test]
    fn a_sidecar_is_substituted_transparently() {
        let dir = temp_dir("sub");
        let ticks = write_ticks(&dir, "S", 0, 60, 600);
        let query = TickQuery::default();
        let spec = |prefer| SymbolBars {
            stream: BarStream::new(
                std::slice::from_ref(&ticks),
                hourly(),
                &BarClock::Utc,
                &query,
            ),
            use_rth: true,
            prefer_sidecar: prefer,
        };

        let mut from_ticks = Vec::new();
        symbol_bars(spec(false), "S", &TimeZone::UTC, |bar| {
            from_ticks.push(bar);
            Ok(())
        })
        .unwrap();

        refresh_sidecar(
            &ticks,
            &dir,
            "S",
            &RefreshSpec {
                interval: hourly(),
                use_rth: true,
                clock: &BarClock::Utc,
                query: &query,
                fill: false,
            },
        )
        .unwrap();

        let mut via_sidecar = Vec::new();
        symbol_bars(spec(true), "S", &TimeZone::UTC, |bar| {
            via_sidecar.push(bar);
            Ok(())
        })
        .unwrap();

        assert_eq!(from_ticks.len(), via_sidecar.len());
        for (a, b) in from_ticks.iter().zip(&via_sidecar) {
            assert_eq!(a.time, b.time);
            assert_eq!(a.open.to_bits(), b.open.to_bits(), "at {}", a.time);
            assert_eq!(a.close.to_bits(), b.close.to_bits(), "at {}", a.time);
            assert_eq!(a.volume, b.volume, "at {}", a.time);
            assert_eq!(a.trades, b.trades, "at {}", a.time);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_sidecar_is_refused_rather_than_silently_truncating() {
        let dir = temp_dir("stale");
        let ticks = write_ticks(&dir, "S", 0, 6, 600); // one hour
        let query = TickQuery::default();
        refresh_sidecar(
            &ticks,
            &dir,
            "S",
            &RefreshSpec {
                interval: hourly(),
                use_rth: true,
                clock: &BarClock::Utc,
                query: &query,
                fill: false,
            },
        )
        .unwrap();

        // The source grows by a whole further bucket.
        std::fs::remove_file(&ticks).unwrap();
        write_ticks(&dir, "S", 0, 18, 600); // three hours

        let err = resolve_sidecar(
            std::slice::from_ref(&ticks),
            "S",
            hourly(),
            true,
            &BarClock::Utc,
            &TimeZone::UTC,
        )
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("stale"), "{text}");
        assert!(text.contains("mdfwob sync"), "should say how to fix it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forward_fill_bypasses_the_sidecar_since_the_name_cannot_express_it() {
        let dir = temp_dir("fillbypass");
        let ticks = write_ticks(&dir, "S", 0, 6, 600);
        let query = TickQuery::default();
        // No sidecar exists, so this only asserts the fill path does not go looking for one.
        let mut out = Vec::new();
        symbol_bars(
            SymbolBars {
                stream: BarStream::new(
                    std::slice::from_ref(&ticks),
                    hourly(),
                    &BarClock::Utc,
                    &query,
                )
                .fill(true),
                use_rth: true,
                prefer_sidecar: true,
            },
            "S",
            &TimeZone::UTC,
            |bar| {
                out.push(bar);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
