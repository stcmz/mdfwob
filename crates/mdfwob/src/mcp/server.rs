//! The tool surface.
//!
//! Seven read-only tools over the same analysis engine the CLI drives. Two things differ from the
//! CLI on purpose:
//!
//! * **Parameters are named and typed.** The CLI's positional token bag (`items: Vec<String>`,
//!   re-parsed by `classify_*`) is a grammar a model has to guess at. Here `interval`, `specs`, and
//!   the time window are ordinary schema'd fields, so there is no grammar to get wrong.
//! * **Results are bounded.** A decade of 1s bars would bury a context window, so every
//!   row-returning tool carries the truncation contract described on
//!   [`Series`](super::dto::Series).
//!
//! Nothing here writes to disk or to stdout: stdout belongs to the JSON-RPC transport, and the
//! write paths (`--output`, `download`) are deliberately not exposed.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use jiff::tz::TimeZone;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{Json, ServerHandler, schemars, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::analysis::calc::parse_streaming_spec;
use crate::analysis::config::AnalysisConfig;
use crate::analysis::inspect::inspect_file;
use crate::analysis::interval::Interval;
use crate::analysis::ls::ls_file;
use crate::analysis::model::Bar;
use crate::analysis::plot::{DEFAULT_HEIGHT, DEFAULT_WIDTH, PlotOptions, layout_series, render};
use crate::analysis::read::{TickQuery, detect_kind, file_symbol};
use crate::analysis::resample::BarClock;
use crate::analysis::session::Session;
use crate::analysis::stat::stat_file;
use crate::cli::{INSPECT_SAMPLE, kind_label, parse_bounds, resolve_session, stream_bars};
use crate::mcp::dto::{
    BarRow, CalcRow, InspectReport, Listing, LsEntry, Series, StatEntry, VerifyReport, finite, iso,
    optional_integer_schema,
};
use crate::mcp::root::Root;

/// Rows returned when the caller does not ask for a specific `limit`.
const DEFAULT_LIMIT: usize = 1_000;
/// Ceiling on `limit`, whatever the caller asks for.
const MAX_LIMIT: usize = 10_000;
/// Bars a single chart will render. Well past the useful density of a 1920px canvas.
const MAX_PLOT_BARS: usize = 50_000;

/// Guidance handed to the model at `initialize`. Worth its length: it is the difference between a
/// model that calls `mdfwob_ls` first and one that guesses symbol names.
const INSTRUCTIONS: &str = "\
Read-only access to a local FWOB market-data archive (trade ticks and OHLCV bars).

Start with `mdfwob_ls` to discover which symbols exist, what kind of file each is (tick or bar),
its native granularity, and the time range it covers. Then use `mdfwob_stat` for a quick
price/volume summary, `mdfwob_bars` to resample into OHLCV bars at any interval, `mdfwob_calc` for
indicator series, and `mdfwob_plot` to render a candlestick chart.

Symbols, not paths: pass `[\"AAPL\"]`, not a filename. Times are ISO-8601 in the exchange timezone
(default America/New_York); `start`/`end` accept `2024-01-01`, a local date-time, or an RFC3339
instant. Bar and tick files both resample to whatever `interval` you ask for, so you never need to
match the file's native granularity.

Row-returning tools cap their output. When a result says `truncated: true`, `rows` holds the most
recent `returned` of `total` bars — narrow `start`/`end` or widen `interval` for a complete answer.

This server cannot write files or download data.";

/// A selection of symbols plus the time window and session to read them under.
///
/// Repeated on each tool rather than `#[serde(flatten)]`-ed so the generated JSON Schema lists the
/// fields inline, where a model will actually read them.
#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct Selection {
    /// Symbols to read, e.g. `["AAPL", "MSFT"]`. Omit for every symbol in the archive.
    #[serde(default)]
    pub symbols: Vec<String>,
    /// Window start: `2024-01-01`, a local date-time, or an RFC3339 instant.
    pub start: Option<String>,
    /// Window end. A bare date includes the whole local day.
    pub end: Option<String>,
    /// Keep only regular-trading-hours activity (09:30-16:00 by default).
    pub use_rth: Option<bool>,
    /// Override the session window as `HH:MM-HH:MM`.
    pub session: Option<String>,
    /// Override the exchange timezone (IANA name, e.g. `America/New_York`).
    pub tz: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct LsParams {
    /// Symbols to list. Omit for the whole archive.
    #[serde(default)]
    pub symbols: Vec<String>,
    /// Override the session window as `HH:MM-HH:MM`.
    pub session: Option<String>,
    /// Override the exchange timezone (IANA name).
    pub tz: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct FileParams {
    /// A single symbol, e.g. `AAPL`.
    pub symbol: String,
    /// Override the session window as `HH:MM-HH:MM`.
    pub session: Option<String>,
    /// Override the exchange timezone (IANA name).
    pub tz: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct BarsParams {
    #[serde(flatten)]
    pub selection: Selection,
    /// Bar interval: `30s`, `5m`, `1h`, `1d`, `1w`, `1mo`, `1y`. Defaults to `1d`.
    pub interval: Option<String>,
    /// Forward-fill intervals with no activity within a session.
    pub fill: Option<bool>,
    /// Maximum rows per symbol (default 1000, max 10000).
    #[schemars(schema_with = "optional_integer_schema")]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct CalcParams {
    #[serde(flatten)]
    pub selection: Selection,
    /// Indicator specs, e.g. `["sma:20", "rsi:14", "ret:log"]`.
    pub specs: Vec<String>,
    /// Bar interval the indicators are computed over. Defaults to `1d`.
    pub interval: Option<String>,
    /// Forward-fill intervals with no activity within a session.
    pub fill: Option<bool>,
    /// Maximum rows per symbol (default 1000, max 10000).
    #[schemars(schema_with = "optional_integer_schema")]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct PlotParams {
    #[serde(flatten)]
    pub selection: Selection,
    /// Bar interval to plot. Defaults to `1d`.
    pub interval: Option<String>,
    /// Indicator overlays, e.g. `["sma:20"]`. Defaults to a single 20-bar SMA.
    #[serde(default)]
    pub specs: Vec<String>,
    /// Draw the volume panel.
    pub volume: Option<bool>,
    /// Forward-fill intervals with no activity within a session.
    pub fill: Option<bool>,
    /// Image width in pixels (default 1920).
    #[schemars(schema_with = "optional_integer_schema")]
    pub width: Option<u32>,
    /// Image height in pixels (default 1080).
    #[schemars(schema_with = "optional_integer_schema")]
    pub height: Option<u32>,
}

/// The MCP server. Cloned per tool call so the blocking work can move to a worker thread.
#[derive(Debug, Clone)]
pub(crate) struct McpServer {
    root: Root,
    acfg: AnalysisConfig,
    session: Option<String>,
    tz: Option<String>,
}

impl McpServer {
    pub(crate) fn new(
        root: Root,
        acfg: AnalysisConfig,
        session: Option<String>,
        tz: Option<String>,
    ) -> Self {
        Self {
            root,
            acfg,
            session,
            tz,
        }
    }

    /// Per-call session/timezone overrides fall back to the server-level ones.
    fn session_for(
        &self,
        use_rth: bool,
        session: Option<&str>,
        tz: Option<&str>,
    ) -> Result<Session> {
        resolve_session(
            &self.acfg,
            use_rth,
            session.or(self.session.as_deref()),
            tz.or(self.tz.as_deref()),
        )
    }

    /// Groups a selection's files by the symbol each reports, preserving discovery order so one
    /// symbol's files feed a single ascending stream — the same grouping `bars`/`calc`/`plot` use.
    fn group(&self, symbols: &[String]) -> Result<BTreeMap<String, Vec<PathBuf>>> {
        let files = self.root.resolve_all(symbols)?;
        if files.is_empty() {
            bail!("no .fwob files found under the server root");
        }
        let mut by_symbol: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for path in &files {
            let symbol = file_symbol(path)
                .with_context(|| format!("failed to read {}", self.root.display(path)))?;
            by_symbol.entry(symbol).or_default().push(path.clone());
        }
        Ok(by_symbol)
    }

    /// Resolves everything a bar-producing tool needs from a [`Selection`].
    fn pipeline(&self, sel: &Selection, interval: Option<&str>) -> Result<Pipeline> {
        let use_rth = sel.use_rth.unwrap_or(false);
        let session = self.session_for(use_rth, sel.session.as_deref(), sel.tz.as_deref())?;
        let interval = match interval {
            Some(token) => {
                Interval::parse(token).with_context(|| format!("unknown interval {token:?}"))??
            }
            None => Interval::parse("1d").expect("\"1d\" is a valid interval")?,
        };
        let tz = session.time_zone();
        let (start, end) = parse_bounds(sel.start.as_deref(), sel.end.as_deref(), &tz)?;
        Ok(Pipeline {
            by_symbol: self.group(&sel.symbols)?,
            interval,
            clock: BarClock::Session(session.clone()),
            query: TickQuery {
                start,
                end,
                session: use_rth.then(|| session.clone()),
            },
            tz,
        })
    }
}

/// Everything resolved from a request, ready to stream bars with.
struct Pipeline {
    by_symbol: BTreeMap<String, Vec<PathBuf>>,
    interval: Interval,
    clock: BarClock,
    query: TickQuery,
    tz: TimeZone,
}

/// Clamps a caller-supplied `limit` into the allowed range.
fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// Builds the truncation contract around the most recent `limit` rows.
fn series<T>(
    symbol: String,
    interval: &Interval,
    tz: &TimeZone,
    total: usize,
    rows: Vec<T>,
) -> Series<T> {
    let truncated = total > rows.len();
    Series {
        symbol,
        interval: interval.label(),
        timezone: tz.iana_name().unwrap_or("UTC").to_owned(),
        total,
        returned: rows.len(),
        truncated,
        hint: truncated.then(|| {
            format!(
                "showing the most recent {} of {total} bars; narrow start/end or widen interval \
                 for a complete answer",
                rows.len()
            )
        }),
        rows,
    }
}

/// Runs blocking analysis work off the async executor.
///
/// Failures come back as `Err(String)` rather than `ErrorData` on purpose: rmcp maps an
/// `ErrorData` to a JSON-RPC *protocol* error, which clients treat as a transport-level failure,
/// while a plain message becomes a `isError: true` tool result whose text the model actually
/// reads. "no file or symbol \"APPL\" under the server root" is something a model can recover from
/// by calling `mdfwob_ls`; a bare `-32603` is not.
async fn blocking<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("{error:#}")),
        Err(join) => Err(format!("analysis task failed: {join}")),
    }
}

#[tool_router]
impl McpServer {
    /// List the tick and bar files in the archive: symbol, kind, native granularity, coverage.
    ///
    /// Cheap — reads headers and a bounded sample, never a full scan. Call this first to find out
    /// what data exists.
    #[tool(
        description = "List available symbols with their kind (tick/bar), native granularity, \
                          row count, and covered time range. Cheap; call this first to discover \
                          what data exists."
    )]
    async fn mdfwob_ls(
        &self,
        Parameters(params): Parameters<LsParams>,
    ) -> Result<Json<Listing<LsEntry>>, String> {
        let this = self.clone();
        blocking(move || {
            let display =
                this.session_for(false, params.session.as_deref(), params.tz.as_deref())?;
            let rth = this.session_for(true, params.session.as_deref(), params.tz.as_deref())?;
            let tz = display.time_zone();
            let files = this.root.resolve_all(&params.symbols)?;
            let mut rows = Vec::new();
            for path in &files {
                let row = ls_file(this.root.display(path), path, &rth, INSPECT_SAMPLE)
                    .with_context(|| format!("failed to read {}", this.root.display(path)))?;
                rows.push(LsEntry::new(row, &tz));
            }
            Ok(Listing::new(rows))
        })
        .await
        .map(Json)
    }

    /// Metadata overview of one file: format, schema columns, coverage, and a frame preview.
    #[tool(
        description = "Quick metadata overview of one symbol's file: FWOB format, on-disk \
                          schema columns, time range, detected granularity, and a small preview \
                          of decoded rows. No full scan."
    )]
    async fn mdfwob_inspect(
        &self,
        Parameters(params): Parameters<FileParams>,
    ) -> Result<Json<InspectReport>, String> {
        let this = self.clone();
        blocking(move || this.inspect(&params)).await.map(Json)
    }

    /// Full integrity check plus a scanned market summary.
    #[tool(
        description = "Verify one symbol's file end to end: structural integrity, that it is a \
                          canonical tick/bar file, and a scanned price/volume summary. Reads every \
                          frame, so this is slower than inspect."
    )]
    async fn mdfwob_verify(
        &self,
        Parameters(params): Parameters<FileParams>,
    ) -> Result<Json<VerifyReport>, String> {
        let this = self.clone();
        blocking(move || this.verify(&params)).await.map(Json)
    }

    /// Per-file price and volume statistics.
    #[tool(
        description = "Summarize each selected symbol: trade count, min/max price, VWAP, total \
                          volume, and covered range. Scans every row in the window."
    )]
    async fn mdfwob_stat(
        &self,
        Parameters(params): Parameters<Selection>,
    ) -> Result<Json<Listing<StatEntry>>, String> {
        let this = self.clone();
        blocking(move || this.stat(&params).map(Listing::new))
            .await
            .map(Json)
    }

    /// Resample into OHLCV bars at any interval.
    #[tool(
        description = "Resample ticks (or re-resample bars) into OHLCV bars at any interval \
                          (30s, 5m, 1h, 1d, 1w, 1mo, 1y). Prices are real prices; times are \
                          ISO-8601 in the exchange timezone. Output is capped - see `truncated`."
    )]
    async fn mdfwob_bars(
        &self,
        Parameters(params): Parameters<BarsParams>,
    ) -> Result<Json<Listing<Series<BarRow>>>, String> {
        let this = self.clone();
        blocking(move || this.bars(&params).map(Listing::new))
            .await
            .map(Json)
    }

    /// Indicator series over resampled bars.
    #[tool(
        description = "Compute indicator series over resampled bars. Specs: sma:N ema:N dema:N \
                          vsma:N vema:N vdema:N rsi:N vol:N ret:log ret:simple. A value is null \
                          during that indicator's warm-up. Output is capped - see `truncated`."
    )]
    async fn mdfwob_calc(
        &self,
        Parameters(params): Parameters<CalcParams>,
    ) -> Result<Json<Listing<Series<CalcRow>>>, String> {
        let this = self.clone();
        blocking(move || this.calc(&params).map(Listing::new))
            .await
            .map(Json)
    }

    /// Render a candlestick chart as a PNG.
    #[tool(
        description = "Render one symbol's bars as a candlestick chart, returned as a PNG \
                          image. Optional indicator overlays and a volume panel. Select exactly \
                          one symbol."
    )]
    async fn mdfwob_plot(
        &self,
        Parameters(params): Parameters<PlotParams>,
    ) -> Result<CallToolResult, String> {
        let this = self.clone();
        let (png, caption) = blocking(move || this.plot(&params)).await?;
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD.encode(png);
        Ok(CallToolResult::success(vec![
            ContentBlock::text(caption),
            ContentBlock::image(data, "image/png"),
        ]))
    }
}

/// The blocking bodies. Kept off the `#[tool_router]` block so that impl stays a readable index of
/// the protocol surface.
impl McpServer {
    fn inspect(&self, params: &FileParams) -> Result<InspectReport> {
        let display = self.session_for(false, params.session.as_deref(), params.tz.as_deref())?;
        let rth = self.session_for(true, params.session.as_deref(), params.tz.as_deref())?;
        let tz = display.time_zone();
        let path = self.single_file(&params.symbol)?;
        // Assembled in core, exactly as `mdfwob inspect` does it — only the rendering differs.
        let report = inspect_file(&path, &rth, &tz, INSPECT_SAMPLE)?;
        Ok(InspectReport::new(&report, self.root.display(&path), &tz))
    }

    fn verify(&self, params: &FileParams) -> Result<VerifyReport> {
        let tz = self
            .session_for(false, params.session.as_deref(), params.tz.as_deref())?
            .time_zone();
        let path = self.single_file(&params.symbol)?;

        // Structural integrity first: every page, frame, ordering, and the string table.
        let report = fwob::Maintenance::verify(&path, fwob::ReaderOptions::default())
            .with_context(|| format!("failed to verify {}", self.root.display(&path)))?;
        // Then identity: a structurally sound file that is not a canonical tick/bar file would
        // otherwise be silently mis-decoded by the stat scan below.
        let reader = fwob::Reader::open(&path)
            .with_context(|| format!("failed to open {}", self.root.display(&path)))?;
        let kind = detect_kind(&reader)
            .with_context(|| format!("failed to verify {}", self.root.display(&path)))?;
        drop(reader);
        let row = stat_file(&path, &TickQuery::default())?;

        Ok(VerifyReport {
            file: self.root.display(&path),
            symbol: row.symbol.clone(),
            kind: kind_label(kind).to_owned(),
            status: "ok".to_owned(),
            frame_count: report.frame_count,
            data: StatEntry::new(row, &tz),
        })
    }

    fn stat(&self, sel: &Selection) -> Result<Vec<StatEntry>> {
        let use_rth = sel.use_rth.unwrap_or(false);
        let session = self.session_for(use_rth, sel.session.as_deref(), sel.tz.as_deref())?;
        let tz = session.time_zone();
        let (start, end) = parse_bounds(sel.start.as_deref(), sel.end.as_deref(), &tz)?;
        let query = TickQuery {
            start,
            end,
            session: use_rth.then(|| session.clone()),
        };
        let files = self.root.resolve_all(&sel.symbols)?;
        if files.is_empty() {
            bail!("no .fwob files found under the server root");
        }
        let mut rows = Vec::new();
        for path in &files {
            let row = stat_file(path, &query)
                .with_context(|| format!("failed to read {}", self.root.display(path)))?;
            rows.push(StatEntry::new(row, &tz));
        }
        Ok(rows)
    }

    fn bars(&self, params: &BarsParams) -> Result<Vec<Series<BarRow>>> {
        let pipeline = self.pipeline(&params.selection, params.interval.as_deref())?;
        let limit = clamp_limit(params.limit);
        let fill = params.fill.unwrap_or(false);

        let mut out = Vec::new();
        for (symbol, paths) in &pipeline.by_symbol {
            // Keep only the trailing `limit` bars: a model asking about a long history almost
            // always wants the recent end of it, and this keeps memory flat regardless of range.
            let mut window: VecDeque<BarRow> = VecDeque::with_capacity(limit);
            let mut total = 0usize;
            stream_bars(
                paths,
                pipeline.interval,
                &pipeline.clock,
                &pipeline.query,
                fill,
                |bar| {
                    total += 1;
                    if window.len() == limit {
                        window.pop_front();
                    }
                    window.push_back(BarRow::new(&bar, &pipeline.tz));
                    Ok(())
                },
            )
            .with_context(|| format!("failed to resample {symbol}"))?;
            out.push(series(
                symbol.clone(),
                &pipeline.interval,
                &pipeline.tz,
                total,
                window.into(),
            ));
        }
        Ok(out)
    }

    fn calc(&self, params: &CalcParams) -> Result<Vec<Series<CalcRow>>> {
        if params.specs.is_empty() {
            bail!("at least one indicator spec is required, e.g. [\"sma:20\"]");
        }
        let pipeline = self.pipeline(&params.selection, params.interval.as_deref())?;
        let limit = clamp_limit(params.limit);
        let fill = params.fill.unwrap_or(false);

        let mut out = Vec::new();
        for (symbol, paths) in &pipeline.by_symbol {
            // Rebuilt per symbol: an indicator carries rolling state that must not leak across
            // series.
            let mut indicators = Vec::new();
            for spec in &params.specs {
                let parsed = parse_streaming_spec(spec)
                    .with_context(|| format!("unknown indicator spec {spec:?}"))??;
                indicators.push(parsed);
            }
            let mut window: VecDeque<CalcRow> = VecDeque::with_capacity(limit);
            let mut total = 0usize;
            stream_bars(
                paths,
                pipeline.interval,
                &pipeline.clock,
                &pipeline.query,
                fill,
                |bar| {
                    total += 1;
                    // Every bar updates every indicator, even ones that get dropped from the
                    // window, so warm-up and rolling state stay correct under truncation.
                    let values = indicators
                        .iter_mut()
                        .map(|ind| (ind.name(), ind.update(&bar).and_then(finite)))
                        .collect();
                    if window.len() == limit {
                        window.pop_front();
                    }
                    window.push_back(CalcRow {
                        time: iso(bar.time, &pipeline.tz),
                        close: bar.close,
                        values,
                    });
                    Ok(())
                },
            )
            .with_context(|| format!("failed to compute indicators for {symbol}"))?;
            out.push(series(
                symbol.clone(),
                &pipeline.interval,
                &pipeline.tz,
                total,
                window.into(),
            ));
        }
        Ok(out)
    }

    fn plot(&self, params: &PlotParams) -> Result<(Vec<u8>, String)> {
        let pipeline = self.pipeline(&params.selection, params.interval.as_deref())?;
        if pipeline.by_symbol.len() > 1 {
            bail!(
                "plot renders one chart, but {} symbols were selected; pass a single symbol",
                pipeline.by_symbol.len()
            );
        }
        let (symbol, paths) = pipeline
            .by_symbol
            .iter()
            .next()
            .context("no symbol selected")?;

        let mut bars: Vec<Bar> = Vec::new();
        stream_bars(
            paths,
            pipeline.interval,
            &pipeline.clock,
            &pipeline.query,
            params.fill.unwrap_or(false),
            |bar| {
                if bars.len() >= MAX_PLOT_BARS {
                    bail!(
                        "range holds more than {MAX_PLOT_BARS} bars at {}; narrow start/end or \
                         widen interval",
                        pipeline.interval.label()
                    );
                }
                bars.push(bar);
                Ok(())
            },
        )
        .with_context(|| format!("failed to resample {symbol}"))?;
        if bars.is_empty() {
            bail!("no bars for {symbol} in the requested range");
        }

        // Routed in core, so this chart is laid out exactly like `mdfwob plot`'s.
        let layout = layout_series(&bars, &params.specs, params.volume.unwrap_or(false))?;
        let caption = format!(
            "{symbol}: {} {} bars from {} to {}",
            bars.len(),
            pipeline.interval.label(),
            iso(bars[0].time, &pipeline.tz),
            iso(bars[bars.len() - 1].time, &pipeline.tz),
        );

        let opts = PlotOptions {
            width: params.width.unwrap_or(DEFAULT_WIDTH),
            height: params.height.unwrap_or(DEFAULT_HEIGHT),
            title: layout.title(symbol, &pipeline.interval.label()),
            tz: pipeline.tz.clone(),
            overlays: layout.overlays,
            panels: layout.panels,
            volume: layout.volume,
            volume_overlays: layout.volume_overlays,
        };
        Ok((render(&bars, &opts).encode_png()?, caption))
    }

    /// Resolves a single-file tool's `symbol` to exactly one path.
    fn single_file(&self, symbol: &str) -> Result<PathBuf> {
        let mut paths = self.root.resolve(symbol)?;
        match paths.len() {
            1 => Ok(paths.remove(0)),
            n => bail!("{symbol:?} resolved to {n} files; pass a single symbol"),
        }
    }
}

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            // Set explicitly: `Implementation::from_build_env()` — what rmcp defaults to — reads
            // *rmcp's* build environment, so the server would otherwise announce itself as
            // "rmcp 3.1.0" and be indistinguishable from every other rmcp server a user has
            // installed.
            .with_server_info(
                Implementation::new("mdfwob", env!("CARGO_PKG_VERSION"))
                    .with_title("mdfwob market data")
                    .with_description(
                        "Read-only access to a local FWOB archive of trade ticks and OHLCV bars.",
                    ),
            )
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_names() -> Vec<String> {
        let mut names: Vec<String> = McpServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        names
    }

    /// The advertised surface is the contract that makes this server safe to hand to an agent.
    /// A new tool has to be added here deliberately.
    #[test]
    fn advertises_exactly_the_read_only_tools() {
        assert_eq!(
            tool_names(),
            vec![
                "mdfwob_bars",
                "mdfwob_calc",
                "mdfwob_inspect",
                "mdfwob_ls",
                "mdfwob_plot",
                "mdfwob_stat",
                "mdfwob_verify",
            ]
        );
    }

    #[test]
    fn limit_is_clamped_into_range() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(50)), 50);
        assert_eq!(clamp_limit(Some(usize::MAX)), MAX_LIMIT);
    }

    /// `serde_json` cannot encode `NaN`/`inf`, and both occur naturally — an empty `StatRow`
    /// carries infinities, and `Bar::vwap` is `NaN` for a zero-volume bucket.
    #[test]
    fn non_finite_values_become_null() {
        assert_eq!(finite(1.5), Some(1.5));
        assert_eq!(finite(f64::NAN), None);
        assert_eq!(finite(f64::INFINITY), None);
        assert_eq!(finite(f64::NEG_INFINITY), None);
    }
}
