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
use crate::analysis::{BarClock, Interval, Session, TickQuery};

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

/// The kind every source of one symbol shares, or `None` when there are no sources.
///
/// Mixing is refused rather than resolved: a symbol backed by both ticks and bars would have two
/// different notions of what a timestamp means, and no single query can be right for both.
fn sources_kind(paths: &[PathBuf]) -> Result<Option<InputKind>> {
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
    Ok(kind)
}

/// The session as a tick **row filter**, which is `Some` only when the sources are ticks.
///
/// A tick carries an instant, so an out-of-hours print has to be dropped. A bar carries its
/// *bucket-start* timestamp — local midnight for a daily bar, outside every intraday window — so
/// applying the same filter to it discards the entire file and reports an empty archive. Worse, it
/// discards it *selectively*: a 1-minute bar file survives, because those timestamps do fall inside
/// the session, so the mistake looks correct until someone reads a daily file.
///
/// Any caller assembling a [`TickQuery`] by hand should get the `session` field from here rather
/// than from `use_rth` alone. [`request_bars`] does this internally.
pub fn session_row_filter(
    paths: &[PathBuf],
    use_rth: bool,
    session: &Session,
) -> Result<Option<Session>> {
    if !use_rth {
        return Ok(None);
    }
    Ok(match sources_kind(paths)? {
        Some(InputKind::Tick) => Some(session.clone()),
        _ => None,
    })
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

    let kind = sources_kind(paths)?;

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
        // The caller's session filter was written for a *tick* source, where it selects in-session
        // prints. A sidecar holds bars already bucketed under that session, and a bar carries its
        // bucket-start timestamp -- local midnight for a daily bar, outside every intraday window
        // -- so re-applying the filter here silently discards the whole file. The `rth` in the
        // sidecar's name IS that filter, already applied.
        let query = TickQuery {
            session: None,
            start: stream.query.start,
            end: stream.query.end,
        };
        return stream_symbol_bars(
            BarStream {
                paths: &paths,
                query: &query,
                // The sidecar is already at this interval; re-resampling it to the same width is a
                // no-op scan, but re-resampling to a *different* one would be wrong.
                ..stream
            },
            sink,
        );
    }
    stream_symbol_bars(stream, sink)
}

/// What bars a consumer wants, stated without reference to how the archive stores them.
///
/// This is the entry point research code should reach for. Whether a symbol is backed by raw
/// ticks, by 1-minute bars, or by a materialized daily sidecar is the archive's business, and the
/// answer must be identical either way — so the caller says "1d, regular hours, this window" and
/// nothing about files.
///
/// The part that cannot be left to callers is the session. Against ticks it is a **row filter**,
/// because a tick carries an instant and out-of-hours prints have to be dropped. Against bars it
/// must not be: a bar carries its bucket-start timestamp, which for a daily bar is local midnight
/// — outside every intraday window — so the same filter silently discards the whole file. A
/// consumer building its own [`TickQuery`] had to know which case it was in, and that knowledge
/// went stale the moment a sidecar was substituted underneath it.
pub struct BarRequest<'a> {
    /// One symbol's source files, ascending. All must be the same kind.
    pub paths: &'a [PathBuf],
    /// Target bar width. `None` keeps a bar source at its stored resolution, and is an error for a
    /// tick source, which has no resolution of its own.
    pub interval: Option<Interval>,
    /// The trading session, which anchors bucket boundaries and — for ticks — filters rows.
    pub session: &'a Session,
    /// Keep only in-session activity. When false the session still anchors buckets.
    pub use_rth: bool,
    /// Emit flat bars for empty buckets inside a session.
    pub fill: bool,
    /// Inclusive scan bounds, in epoch seconds.
    pub start: Option<u32>,
    pub end: Option<u32>,
    /// Read a current sidecar instead of re-resampling. Pure optimization; identical result.
    pub prefer_sidecar: bool,
}

impl<'a> BarRequest<'a> {
    /// Regular hours, no fill, whole file, sidecars preferred.
    pub fn new(
        paths: &'a [PathBuf],
        interval: impl Into<Option<Interval>>,
        session: &'a Session,
    ) -> Self {
        Self {
            paths,
            interval: interval.into(),
            session,
            use_rth: true,
            fill: false,
            start: None,
            end: None,
            prefer_sidecar: true,
        }
    }
    pub fn use_rth(mut self, use_rth: bool) -> Self {
        self.use_rth = use_rth;
        self
    }
    pub fn fill(mut self, fill: bool) -> Self {
        self.fill = fill;
        self
    }
    pub fn window(mut self, start: Option<u32>, end: Option<u32>) -> Self {
        self.start = start;
        self.end = end;
        self
    }
    pub fn prefer_sidecar(mut self, prefer: bool) -> Self {
        self.prefer_sidecar = prefer;
        self
    }
}

/// Streams the bars a [`BarRequest`] describes, choosing the source and the query internally.
pub fn request_bars(
    symbol: &str,
    req: BarRequest<'_>,
    sink: impl FnMut(Bar) -> Result<()>,
) -> Result<()> {
    let clock = BarClock::Session(req.session.clone());
    let query = TickQuery {
        start: req.start,
        end: req.end,
        // The whole reason this function exists: the row filter belongs to ticks alone.
        session: session_row_filter(req.paths, req.use_rth, req.session)?,
    };
    symbol_bars(
        SymbolBars {
            stream: BarStream::new(req.paths, req.interval, &clock, &query).fill(req.fill),
            use_rth: req.use_rth,
            prefer_sidecar: req.prefer_sidecar,
        },
        symbol,
        &req.session.time_zone(),
        sink,
    )
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

    /// A caller reading a tick file installs the session as a *row* filter, because that is how
    /// out-of-hours prints get dropped. Once a sidecar is substituted the input is bars, and a
    /// daily bar's timestamp is local midnight -- outside every intraday window -- so carrying that
    /// filter over would drop every row and report an empty archive. Regression for exactly that:
    /// the sidecar and the ticks must still agree.
    #[test]
    fn a_session_row_filter_does_not_follow_the_query_onto_a_daily_sidecar() {
        let dir = temp_dir("session-sidecar");
        let session = Session::new("America/New_York", "09:30-16:00").unwrap();
        let clock = BarClock::Session(session.clone());
        let daily = Interval::parse("1d").unwrap().unwrap();

        // 09:30 ET on 2024-03-04, then a tick every 30 minutes for three trading days.
        let open = |y: i16, m: i8, d: i8| {
            jiff::civil::date(y, m, d)
                .at(9, 30, 0, 0)
                .in_tz("America/New_York")
                .unwrap()
                .timestamp()
                .as_second() as u32
        };
        let mut times = Vec::new();
        for (y, m, d) in [(2024, 3, 4), (2024, 3, 5), (2024, 3, 6)] {
            let start = open(y, m, d);
            times.extend((0..12).map(|i| start + i * 1_800));
        }
        let path = dir.join("T.fwob");
        let mut writer = Writer::create_v2(&path, tick_schema(), WriterOptions::new("T")).unwrap();
        let mut buf = Vec::new();
        for (i, &time) in times.iter().enumerate() {
            buf.clear();
            RawTick::new(time, 100.0 + i as f64, 10)
                .unwrap()
                .encode(&mut buf);
            writer.append_frame(&buf).unwrap();
        }
        writer.finish().unwrap();

        // The window a research run would ask for, with the session installed as a row filter.
        let query = TickQuery {
            start: Some(open(2024, 3, 4) - 34_200),
            end: Some(open(2024, 3, 7)),
            session: Some(session.clone()),
        };
        let spec = |prefer| SymbolBars {
            stream: BarStream::new(std::slice::from_ref(&path), daily, &clock, &query),
            use_rth: true,
            prefer_sidecar: prefer,
        };
        let collect = |prefer| {
            let mut out = Vec::new();
            symbol_bars(spec(prefer), "T", &session.time_zone(), |bar| {
                out.push(bar);
                Ok(())
            })
            .unwrap();
            out
        };

        let from_ticks = collect(false);
        assert_eq!(from_ticks.len(), 3, "three trading days");

        refresh_sidecar(
            &path,
            &dir,
            "T",
            &RefreshSpec {
                interval: daily,
                use_rth: true,
                clock: &clock,
                query: &TickQuery {
                    session: Some(session.clone()),
                    ..Default::default()
                },
                fill: false,
            },
        )
        .unwrap();

        let via_sidecar = collect(true);
        assert_eq!(
            via_sidecar.len(),
            from_ticks.len(),
            "the sidecar must serve the same days, not an empty file"
        );
        for (a, b) in from_ticks.iter().zip(&via_sidecar) {
            assert_eq!(a.time, b.time);
            assert_eq!(a.open.to_bits(), b.open.to_bits(), "at {}", a.time);
            assert_eq!(a.close.to_bits(), b.close.to_bits(), "at {}", a.time);
            assert_eq!(a.volume, b.volume, "at {}", a.time);
        }

        // The window still has to be honoured through the sidecar path.
        let narrowed = TickQuery {
            start: Some(open(2024, 3, 5) - 34_200),
            end: Some(open(2024, 3, 6)),
            session: Some(session.clone()),
        };
        let mut windowed = Vec::new();
        symbol_bars(
            SymbolBars {
                stream: BarStream::new(std::slice::from_ref(&path), daily, &clock, &narrowed),
                use_rth: true,
                prefer_sidecar: true,
            },
            "T",
            &session.time_zone(),
            |bar| {
                windowed.push(bar);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(windowed.len(), 2, "start/end must still apply");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The property `request_bars` exists to guarantee: asking for 1d regular-hours bars gives the
    /// same answer whether the symbol is stored as ticks, as finer bars, or as a daily sidecar. A
    /// caller that had to build the session filter itself got this wrong for the bar cases.
    #[test]
    fn the_same_request_gives_the_same_bars_from_ticks_bars_or_a_sidecar() {
        let dir = temp_dir("agnostic");
        let session = Session::new("America/New_York", "09:30-16:00").unwrap();
        let daily = Interval::parse("1d").unwrap().unwrap();
        let minute = Interval::parse("1m").unwrap().unwrap();

        let open = |d: i8| {
            jiff::civil::date(2024, 3, d)
                .at(9, 30, 0, 0)
                .in_tz("America/New_York")
                .unwrap()
                .timestamp()
                .as_second() as u32
        };
        // Three trading days of half-hourly in-session ticks.
        let mut times = Vec::new();
        for d in [4i8, 5, 6] {
            times.extend((0..12).map(|i| open(d) + i * 1_800));
        }
        let ticks = dir.join("T.fwob");
        let mut writer = Writer::create_v2(&ticks, tick_schema(), WriterOptions::new("T")).unwrap();
        let mut buf = Vec::new();
        for (i, &time) in times.iter().enumerate() {
            buf.clear();
            RawTick::new(time, 100.0 + i as f64, 10)
                .unwrap()
                .encode(&mut buf);
            writer.append_frame(&buf).unwrap();
        }
        writer.finish().unwrap();

        let ask = |paths: &[PathBuf], prefer_sidecar: bool| {
            let mut out = Vec::new();
            request_bars(
                "T",
                BarRequest::new(paths, daily, &session)
                    .window(Some(open(4) - 34_200), Some(open(7)))
                    .prefer_sidecar(prefer_sidecar),
                |bar| {
                    out.push(bar);
                    Ok(())
                },
            )
            .unwrap();
            out
        };

        let tick_sources = vec![ticks.clone()];
        let from_ticks = ask(&tick_sources, false);
        assert_eq!(from_ticks.len(), 3, "three trading days");

        // 1) A finer bar file as the source: same daily answer, re-resampled.
        refresh_sidecar(
            &ticks,
            &dir,
            "T",
            &RefreshSpec {
                interval: minute,
                use_rth: true,
                clock: &BarClock::Session(session.clone()),
                query: &TickQuery {
                    session: Some(session.clone()),
                    ..Default::default()
                },
                fill: false,
            },
        )
        .unwrap();
        let minute_bars = vec![sidecar_path(&dir, "T", minute, true)];
        let from_minutes = ask(&minute_bars, false);

        // 2) A daily sidecar beside the ticks, chosen transparently.
        refresh_sidecar(
            &ticks,
            &dir,
            "T",
            &RefreshSpec {
                interval: daily,
                use_rth: true,
                clock: &BarClock::Session(session.clone()),
                query: &TickQuery {
                    session: Some(session.clone()),
                    ..Default::default()
                },
                fill: false,
            },
        )
        .unwrap();
        let from_sidecar = ask(&tick_sources, true);

        for (label, got) in [("1m bars", &from_minutes), ("sidecar", &from_sidecar)] {
            assert_eq!(got.len(), from_ticks.len(), "{label} row count");
            for (a, b) in from_ticks.iter().zip(got.iter()) {
                assert_eq!(a.time, b.time, "{label} time");
                assert_eq!(a.open.to_bits(), b.open.to_bits(), "{label} open");
                assert_eq!(a.close.to_bits(), b.close.to_bits(), "{label} close");
                assert_eq!(a.volume, b.volume, "{label} volume");
            }
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
