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

use crate::analysis::inspect::detect_bar_granularity;
use crate::analysis::interval::Granularity;
use crate::analysis::model::Bar;
use crate::analysis::output::format_epoch_tz;
use crate::analysis::read::{
    InputKind, input_kind, open_tick_reader, stream_bars_file, stream_ticks,
};
use crate::analysis::resample::{BarResampler, ForwardFiller, Resampler};
use crate::analysis::schema::decode_bar;
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

/// What a source file holds, finely enough to decide how it may be filtered and resampled.
///
/// The distinction that matters is not tick-vs-bar but *whether a row's timestamp is an instant
/// inside the trading day*. A tick's is. A sub-daily bar's bucket start is too. A daily bar's is
/// local midnight, which sits outside every intraday window — so the same session filter that is
/// correct for the first two empties the third.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceShape {
    /// Trade prints. Every row carries an instant.
    Ticks,
    /// Bars whose buckets are shorter than a day, so their starts are intraday instants.
    SubDayBars(u32),
    /// Bars at daily granularity or coarser, stamped at the bucket start.
    CoarseBars,
    /// Bars whose granularity could not be determined (fewer than two rows).
    UnknownBars,
}

impl SourceShape {
    /// Whether a session may be applied to this source as a row filter.
    ///
    /// Ticks and sub-daily bars, yes: their timestamps are instants within the day, so keeping
    /// only in-session rows yields exactly the in-session subset. Daily bars, no: the filter
    /// would discard every row. Unknown, no — a guess that silently drops the filter is better
    /// than one that silently empties the file, and the caller still gets bars.
    pub fn accepts_session_filter(self) -> bool {
        matches!(self, Self::Ticks | Self::SubDayBars(_))
    }

    /// Bucket width in seconds, for a sub-daily bar source.
    pub fn seconds(self) -> Option<u32> {
        match self {
            Self::SubDayBars(seconds) => Some(seconds),
            _ => None,
        }
    }
}

/// Reads a file's shape from its header plus a small leading sample.
pub fn source_shape(path: &Path) -> Result<SourceShape> {
    if input_kind(path)? == InputKind::Tick {
        return Ok(SourceShape::Ticks);
    }
    let times = leading_bar_times(path, GRANULARITY_SAMPLE)?;
    let Some(label) = detect_bar_granularity(&times) else {
        return Ok(SourceShape::UnknownBars);
    };
    let Some(Ok(interval)) = Interval::parse(&label) else {
        return Ok(SourceShape::UnknownBars);
    };
    Ok(match interval.granularity() {
        Granularity::SubDay(seconds) => SourceShape::SubDayBars(seconds),
        _ => SourceShape::CoarseBars,
    })
}

/// Bars sampled from the front of a file are enough to see the minimum gap between buckets.
const GRANULARITY_SAMPLE: u64 = 64;

/// Reads the first `sample` bar timestamps without decoding the whole file.
fn leading_bar_times(path: &Path, sample: u64) -> Result<Vec<u32>> {
    let mut reader =
        Reader::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let count = reader.frame_count().min(sample);
    let mut times = Vec::with_capacity(count as usize);
    for frame in reader.frames(0..count)? {
        times.push(decode_bar(frame?.bytes())?.time);
    }
    Ok(times)
}

/// The shape every source of one symbol shares, or `None` when there are no sources.
///
/// Mixing is refused rather than resolved: a symbol backed by both ticks and bars would have two
/// different notions of what a timestamp means, and no single query can be right for both.
fn sources_shape(paths: &[PathBuf]) -> Result<Option<SourceShape>> {
    let mut shape: Option<SourceShape> = None;
    for path in paths {
        let this = source_shape(path)?;
        match shape {
            Some(existing)
                if existing.accepts_session_filter() != this.accepts_session_filter() =>
            {
                bail!(
                    "cannot mix tick and bar files for one symbol ({})",
                    path.display()
                )
            }
            // Several bar files of one symbol: the coarsest decides, since a session filter that
            // is wrong for any one of them is wrong for the set.
            Some(SourceShape::SubDayBars(a)) => {
                shape = Some(match this {
                    SourceShape::SubDayBars(b) => SourceShape::SubDayBars(a.max(b)),
                    other => other,
                })
            }
            _ => shape = Some(this),
        }
    }
    Ok(shape)
}

/// The session as a **row filter**, which is `Some` only when the sources can be filtered by one.
///
/// A tick carries an instant, so an out-of-hours print has to be dropped. A *sub-daily* bar's
/// bucket start is an instant inside the day too, so the same filter is correct there — keeping
/// only in-session rows yields exactly the in-session subset. A daily bar is stamped at local
/// midnight, outside every intraday window, so filtering it discards the whole file.
///
/// Keying this on tick-vs-bar alone was wrong in the other direction: it dropped the filter for
/// every bar source, so asking a 1-minute file for regular hours silently returned extended-hours
/// numbers — no error, just a different answer than the one requested.
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
    Ok(match sources_shape(paths)? {
        Some(shape) if shape.accepts_session_filter() => Some(session.clone()),
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

    let shape = sources_shape(paths)?;
    let kind = shape.map(|s| match s {
        SourceShape::Ticks => InputKind::Tick,
        _ => InputKind::Bar,
    });

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
/// Whether a sidecar stored at `(have_interval, have_rth)` can answer a request for
/// `(want_interval, want_rth)`.
///
/// Two independent questions, and both must be yes.
///
/// **Interval.** A finer bucket aggregates into a coarser one; the reverse is lost information, so
/// a 1h sidecar can never answer 1m. Sub-daily into sub-daily needs the target to be a whole
/// multiple of the source. Sub-daily into daily-or-coarser additionally needs the source to divide
/// a day evenly, or a bucket would straddle the boundary and land in two sessions at once — which
/// is why 1m, 5m and 1h qualify and 7m does not.
///
/// **Session.** Extended hours are a superset of regular ones, so an `ext` sidecar can answer an
/// `rth` request by dropping the out-of-session rows — but only while its rows are sub-daily, since
/// a daily bar has already aggregated the extended prints into its OHLC and cannot un-mix them. An
/// `rth` sidecar can never answer an `ext` request: the pre- and post-market prints are simply not
/// in the file.
fn sidecar_serves(
    have_interval: Interval,
    have_rth: bool,
    want_interval: Interval,
    want_rth: bool,
) -> bool {
    let intervals_ok = match (have_interval.granularity(), want_interval.granularity()) {
        (Granularity::SubDay(have), Granularity::SubDay(want)) => {
            have <= want && want.is_multiple_of(have)
        }
        (Granularity::SubDay(have), _) => DAY_SECONDS.is_multiple_of(have),
        (Granularity::Day(have), Granularity::Day(want)) => {
            have <= want && want.is_multiple_of(have)
        }
        (Granularity::Day(have), Granularity::Week(_)) => have == 1,
        (Granularity::Week(have), Granularity::Week(want)) => {
            have <= want && want.is_multiple_of(have)
        }
        _ => false,
    };
    if !intervals_ok {
        return false;
    }
    match (have_rth, want_rth) {
        // Same session: nothing to do.
        (true, true) | (false, false) => true,
        // Extended answering regular: filterable only while the rows are intraday instants.
        (false, true) => matches!(have_interval.granularity(), Granularity::SubDay(_)),
        // Regular answering extended: the prints are not there.
        (true, false) => false,
    }
}

const DAY_SECONDS: u32 = 86_400;

/// Sidecar intervals worth probing, coarsest first.
///
/// Coarsest-compatible wins: a 1h sidecar answers a daily request with a twelfth of the rows a 1m
/// one would. Probing is a handful of `exists` calls, so the list stays explicit rather than
/// scanning the directory — a scan would also have to guess which files are this symbol's.
const SIDECAR_CANDIDATES: [&str; 6] = ["1d", "1h", "30m", "15m", "5m", "1m"];

/// A sidecar that can answer this request, with the source it stands in for.
pub struct SidecarMatch {
    pub path: PathBuf,
    /// The sidecar's own interval, which may be finer than the one requested.
    pub interval: Interval,
    /// Whether the sidecar stores regular hours only.
    pub use_rth: bool,
}

pub fn resolve_sidecar(
    paths: &[PathBuf],
    symbol: &str,
    interval: Interval,
    use_rth: bool,
    clock: &BarClock,
    tz: &TimeZone,
) -> Result<Option<PathBuf>> {
    Ok(resolve_sidecar_match(paths, symbol, interval, use_rth, clock, tz)?.map(|m| m.path))
}

/// Picks the cheapest sidecar that can answer `(interval, use_rth)`, or `None` to use the source.
///
/// Exact match first, then any coarser-but-compatible store, then finer ones. Freshness is checked
/// against the *sidecar's own* interval, since that is the bucket it would have to complete.
pub fn resolve_sidecar_match(
    paths: &[PathBuf],
    symbol: &str,
    interval: Interval,
    use_rth: bool,
    clock: &BarClock,
    tz: &TimeZone,
) -> Result<Option<SidecarMatch>> {
    // A sidecar stands in for exactly one tick file; several sources have no single sidecar.
    let [source] = paths else { return Ok(None) };
    if input_kind(source)? != InputKind::Tick {
        return Ok(None);
    }
    let Some(dir) = source.parent() else {
        return Ok(None);
    };
    let Some(tick_last) = last_key(source)? else {
        return Ok(None);
    };

    let mut stale: Option<anyhow::Error> = None;
    for label in SIDECAR_CANDIDATES {
        let Some(Ok(have)) = Interval::parse(label) else {
            continue;
        };
        // Prefer a store in the requested session; fall back to extended, which can be filtered.
        for have_rth in [use_rth, false] {
            if !sidecar_serves(have, have_rth, interval, use_rth) {
                continue;
            }
            let side = sidecar_path(dir, symbol, have, have_rth);
            if !side.exists() {
                continue;
            }
            let Some(bars_last) = last_key(&side)? else {
                continue; // an empty sidecar: fall back to the source
            };
            // Stale only when a whole bucket beyond the last stored one could be formed. Extra
            // ticks inside the final bucket make it partial, which `mdfwob sync` re-derives anyway.
            if tick_last >= clock.next_bucket_start(have, bars_last) {
                // Remember, but keep looking: another sidecar may still be current.
                stale.get_or_insert_with(|| {
                    anyhow::anyhow!(
                        "{} is stale.\n  sidecar ends {}\n  source has ticks through {}\nRefresh \
                         it with: mdfwob sync {} {}{}",
                        side.display(),
                        format_epoch_tz(bars_last, tz),
                        format_epoch_tz(tick_last, tz),
                        source.display(),
                        have.label(),
                        if have_rth { " rth" } else { "" },
                    )
                });
                continue;
            }
            return Ok(Some(SidecarMatch {
                path: side,
                interval: have,
                use_rth: have_rth,
            }));
        }
    }
    // Nothing usable. A stale sidecar is worth saying so about rather than silently re-resampling
    // the ticks it was built to avoid.
    match stale {
        Some(error) => Err(error),
        None => Ok(None),
    }
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
        && let Some(found) =
            resolve_sidecar_match(stream.paths, symbol, interval, use_rth, stream.clock, tz)?
    {
        let paths = [found.path];
        // Two things the sidecar's name already settles, and one it does not.
        //
        // Its `rth` or `ext` says which prints went into its buckets, so re-applying the caller's
        // session filter is wrong whenever the store already matches — and catastrophic for a
        // daily store, whose bucket-start timestamps sit outside every intraday window.
        //
        // What it does *not* settle is an extended store answering a regular-hours request. There
        // the filter is exactly what makes the answer correct, and it is safe to apply because
        // such a store is always sub-daily: its rows are intraday instants.
        let filtering = !found.use_rth && use_rth;
        let query = TickQuery {
            session: match (filtering, stream.clock) {
                (true, BarClock::Session(session)) => Some(session.clone()),
                _ => None,
            },
            start: stream.query.start,
            end: stream.query.end,
        };
        return stream_symbol_bars(
            BarStream {
                paths: &paths,
                query: &query,
                // Keep the caller's target width. When the store is already at it this is a no-op
                // scan; when the store is finer, this is the aggregation that lets one sidecar
                // serve every coarser interval.
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

    /// Ticks at real-world epochs, priced from their index so a 2024 timestamp cannot overflow the
    /// scaled-price range the way `write_ticks`' time-derived price does.
    fn write_dated_ticks(dir: &Path, name: &str, start: u32, count: u32, step: u32) -> PathBuf {
        let path = dir.join(format!("{name}.fwob"));
        let mut writer = Writer::create_v2(&path, tick_schema(), WriterOptions::new(name)).unwrap();
        let mut buf = Vec::new();
        for i in 0..count {
            buf.clear();
            RawTick::new(start + i * step, 100.0 + f64::from(i), 10)
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

    // ---- source shape and the session row filter ---------------------------------------

    /// The rule that was wrong: keying the session filter on tick-vs-bar alone drops it for every
    /// bar source, so a 1-minute file asked for regular hours silently returns extended-hours
    /// numbers. A sub-daily bar's bucket start *is* an intraday instant and must be filtered.
    #[test]
    fn a_sub_daily_bar_source_accepts_the_session_filter() {
        assert!(SourceShape::Ticks.accepts_session_filter());
        assert!(SourceShape::SubDayBars(60).accepts_session_filter());
        assert!(SourceShape::SubDayBars(3_600).accepts_session_filter());
        assert!(
            !SourceShape::CoarseBars.accepts_session_filter(),
            "a daily bar is stamped at local midnight; filtering empties the file"
        );
        assert!(
            !SourceShape::UnknownBars.accepts_session_filter(),
            "an unreadable granularity drops the filter rather than risk emptying the file"
        );
    }

    // ---- sidecar compatibility ---------------------------------------------------------

    fn iv(label: &str) -> Interval {
        Interval::parse(label).unwrap().unwrap()
    }

    /// A finer store aggregates into a coarser request; the reverse loses information.
    #[test]
    fn a_finer_sidecar_serves_a_coarser_request_and_never_the_reverse() {
        assert!(sidecar_serves(iv("1m"), false, iv("5m"), false));
        assert!(sidecar_serves(iv("1m"), false, iv("1h"), false));
        assert!(sidecar_serves(iv("1m"), false, iv("1d"), false));
        assert!(sidecar_serves(iv("5m"), false, iv("1h"), false));
        assert!(sidecar_serves(iv("1h"), false, iv("1d"), false));
        assert!(sidecar_serves(iv("1d"), false, iv("1d"), false), "exact");

        assert!(
            !sidecar_serves(iv("1h"), false, iv("1m"), false),
            "coarser cannot serve finer"
        );
        assert!(!sidecar_serves(iv("1d"), false, iv("1h"), false));
        assert!(!sidecar_serves(iv("5m"), false, iv("1m"), false));
    }

    /// Buckets must tile the target, or one would straddle a boundary and land in two sessions.
    #[test]
    fn an_interval_that_does_not_divide_the_target_is_refused() {
        assert!(
            !sidecar_serves(iv("2m"), false, iv("5m"), false),
            "5 is not a multiple of 2"
        );
        assert!(sidecar_serves(iv("2m"), false, iv("4m"), false));
        // 7 minutes does not divide a day, so a daily bucket would split one of its bars.
        assert!(!sidecar_serves(iv("7m"), false, iv("1d"), false));
        assert!(sidecar_serves(iv("30m"), false, iv("1d"), false));
    }

    /// Extended is a superset of regular, so it can be filtered down — but only while the rows are
    /// intraday. A daily extended bar has already mixed the out-of-hours prints into its OHLC.
    #[test]
    fn extended_serves_regular_only_while_the_rows_are_sub_daily() {
        assert!(
            sidecar_serves(iv("1m"), false, iv("1d"), true),
            "filter, then aggregate"
        );
        assert!(sidecar_serves(iv("1h"), false, iv("1h"), true));
        assert!(
            !sidecar_serves(iv("1d"), false, iv("1d"), true),
            "a daily extended bar cannot be un-mixed into a regular-hours one"
        );
    }

    /// The prints simply are not in a regular-hours file.
    #[test]
    fn regular_never_serves_extended() {
        assert!(!sidecar_serves(iv("1m"), true, iv("1m"), false));
        assert!(!sidecar_serves(iv("1m"), true, iv("1d"), false));
        assert!(
            sidecar_serves(iv("1m"), true, iv("1d"), true),
            "same session is fine"
        );
    }

    /// The whole point, end to end: one extended-hours 1-minute sidecar answers a regular-hours
    /// *daily* request, and answers it with exactly the bars the ticks would have produced.
    ///
    /// Both halves are load-bearing. Serving a coarser request from a finer store is what makes a
    /// single sidecar useful for every interval above it; filtering that store to the requested
    /// session is what keeps the answer correct rather than merely fast. Before this, the filter
    /// was dropped for any bar source and the same call returned extended-hours numbers.
    #[test]
    fn an_extended_minute_sidecar_answers_a_regular_hours_daily_request() {
        let dir = temp_dir("coarser");
        let session = Session::new("America/New_York", "09:30-16:00").unwrap();
        let clock = BarClock::Session(session.clone());
        let tz = session.time_zone();

        // A day of ticks every ten minutes, from 04:00 to 20:00 New York — so some land inside
        // regular hours and many do not. 2024-03-05 is a Tuesday well clear of a DST boundary.
        let open = jiff::civil::date(2024, 3, 5)
            .at(4, 0, 0, 0)
            .in_tz("America/New_York")
            .unwrap()
            .timestamp()
            .as_second() as u32;
        let ticks = write_dated_ticks(&dir, "S", open, 16 * 6, 600);

        // Ground truth: daily regular-hours bars straight from the ticks.
        let query = TickQuery {
            session: Some(session.clone()),
            start: None,
            end: None,
        };
        let mut from_ticks = Vec::new();
        stream_symbol_bars(
            BarStream::new(std::slice::from_ref(&ticks), iv("1d"), &clock, &query),
            |bar| {
                from_ticks.push(bar);
                Ok(())
            },
        )
        .unwrap();
        assert!(!from_ticks.is_empty(), "the fixture must produce a bar");

        // Materialize a 1-minute *extended* sidecar — the only store on disk.
        let plain = TickQuery::default();
        refresh_sidecar(
            &ticks,
            &dir,
            "S",
            &RefreshSpec {
                interval: iv("1m"),
                use_rth: false,
                clock: &clock,
                query: &plain,
                fill: false,
            },
        )
        .unwrap();

        // Assert the mechanism, not just the answer: without this, the test would also pass by
        // silently falling back to the ticks, which is exactly what the old code did.
        let matched = resolve_sidecar_match(
            std::slice::from_ref(&ticks),
            "S",
            iv("1d"),
            true,
            &clock,
            &tz,
        )
        .unwrap()
        .expect("the 1m extended sidecar must answer a 1d regular-hours request");
        assert_eq!(
            matched.interval.label(),
            "1m",
            "served from the minute store"
        );
        assert!(!matched.use_rth, "which is the extended one");

        // Ask for daily regular hours again, now letting the sidecar answer.
        let mut from_sidecar = Vec::new();
        symbol_bars(
            SymbolBars {
                stream: BarStream::new(std::slice::from_ref(&ticks), iv("1d"), &clock, &query),
                use_rth: true,
                prefer_sidecar: true,
            },
            "S",
            &tz,
            |bar| {
                from_sidecar.push(bar);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            from_sidecar.len(),
            from_ticks.len(),
            "same number of daily bars"
        );
        for (side, tick) in from_sidecar.iter().zip(&from_ticks) {
            assert_eq!(side.time, tick.time);
            assert_eq!(side.open.to_bits(), tick.open.to_bits(), "open");
            assert_eq!(side.high.to_bits(), tick.high.to_bits(), "high");
            assert_eq!(side.low.to_bits(), tick.low.to_bits(), "low");
            assert_eq!(side.close.to_bits(), tick.close.to_bits(), "close");
            assert_eq!(side.volume, tick.volume, "volume");
            assert_eq!(side.trades, tick.trades, "trade count");
        }

        // And the filter genuinely did something: an unfiltered read of the same sidecar must
        // differ, or this test would pass even with the session dropped.
        let mut extended = Vec::new();
        symbol_bars(
            SymbolBars {
                stream: BarStream::new(std::slice::from_ref(&ticks), iv("1d"), &clock, &plain),
                use_rth: false,
                prefer_sidecar: true,
            },
            "S",
            &tz,
            |bar| {
                extended.push(bar);
                Ok(())
            },
        )
        .unwrap();
        assert!(
            extended
                .iter()
                .zip(&from_sidecar)
                .any(|(e, r)| e.trades != r.trades || e.volume != r.volume),
            "extended and regular hours must differ, or the filter is a no-op"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
