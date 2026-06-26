use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use fwob::{FormatVersion, detect_format};

use std::collections::BTreeMap;

use crate::{
    analysis::{
        calc::{Calc, parse_spec, summarize},
        config::{AnalysisConfig, DEFAULT_EXTENDED_HOURS, DEFAULT_RTH_HOURS, ReturnMethod},
        interval::{Granularity, Interval},
        model::Bar,
        output::{
            AnalysisFormat, BarStream, CalcSeries, guard_symbol_count, write_bars_fwob, write_calc,
            write_stat,
        },
        read::{
            InputKind, TickQuery, discover_inputs, input_kind, open_tick_reader, read_bars,
            stream_ticks, tick_symbol,
        },
        resample::{BarClock, ForwardFiller, Resampler},
        schema::{bar_schema, with_symbol_column},
        session::Session,
        stat::StatAccumulator,
    },
    config::{Config, StockContractConfig},
    downloader::{DownloadPlan, Downloader},
    fwob_options::{parse_tokens, validate_zstd_level},
};

#[derive(Debug, Parser)]
#[command(name = "mdfwob")]
#[command(version)]
#[command(about = "Download market data into FWOB files")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

impl Cli {
    pub fn run(self) -> Result<()> {
        match self.command {
            Command::Download(args) => args.run(),
            Command::Verify(args) => args.run(),
            Command::Stat(args) => args.run(),
            Command::Bars(args) => args.run(),
            Command::Calc(args) => args.run(),
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Download configured or ad hoc historical market data.
    Download(DownloadArgs),
    /// Fully verify a FWOB v1 or v2 output file.
    Verify(VerifyArgs),
    /// Summarize tick files: one row per file with price/volume/gap stats.
    Stat(StatArgs),
    /// Resample ticks into OHLCV bars (table/csv/md/jsonl/raw/hex/fwob).
    Bars(BarsArgs),
    /// Compute per-bar indicator series (sma/ema/rsi/ret/vol) over bars or ticks.
    Calc(CalcArgs),
}

#[derive(Debug, Args)]
#[command(override_usage = "mdfwob download [OPTIONS] [CONFIG.toml] [SYMBOLS...] [FWOB_TOKENS...]")]
#[command(after_help = "FWOB tokens:
  providers: ibkr, databento, polygon, thetadata
  formats: v1, v2
  codecs: zstd, lz4, smallest, none
  encodings: row-raw, columnar-basic, columnar-delta, smallest
  page packing: estimate-shrink, tight-fit
  page size: INTEGER{B|KB|KiB|MB|MiB} (1KiB..16MiB)
  switches: compress-partial-page

Tokens are case-sensitive. Defaults follow the fwob conversion defaults:
v2 512KiB columnar-basic zstd.")]
struct DownloadArgs {
    /// Output directory. Defaults to config value or current directory.
    #[arg(long)]
    output: Option<PathBuf>,

    /// IB Gateway/TWS host. Defaults to 127.0.0.1.
    #[arg(long)]
    host: Option<String>,

    /// IB Gateway/TWS port. Defaults to 4002.
    #[arg(long)]
    port: Option<u16>,

    /// IB API client id. Defaults to 0.
    #[arg(long)]
    client_id: Option<i32>,

    /// Seconds to retry IBKR reconnects. -1 retries forever; 0 disables retries.
    #[arg(long, allow_hyphen_values = true)]
    reconnect_timeout_seconds: Option<i64>,

    /// Seconds a request may stall with no data before forcing a reconnect. 0 disables stall
    /// detection. Defaults to 30.
    #[arg(long)]
    stall_timeout_seconds: Option<u64>,

    /// Default primary exchange for ad hoc stock symbols.
    #[arg(long)]
    primary_exchange: Option<String>,

    /// Start date/time. Accepts YYYY-MM-DD or RFC3339.
    #[arg(long)]
    start: Option<String>,

    /// End date/time. Accepts YYYY-MM-DD or RFC3339. Defaults to now.
    #[arg(long)]
    end: Option<String>,

    /// Exchange timezone (IANA name) for day-advance alignment and log timestamps.
    /// Defaults to the config value (America/New_York).
    #[arg(long)]
    timezone: Option<String>,

    /// Download regular trading hours only.
    #[arg(long)]
    rth: bool,

    /// Include extended-hours data. This is the default.
    #[arg(long)]
    all_hours: bool,

    /// zstd compression level for zstd pages.
    #[arg(long)]
    zstd_level: Option<i32>,

    /// Maximum symbols processed concurrently. Defaults to 4.
    #[arg(long)]
    parallelism: Option<usize>,

    /// Minimum interval between provider requests in milliseconds. Defaults to 3000.
    #[arg(long)]
    request_interval_ms: Option<u64>,

    /// Minimum interval between retry attempts after a recoverable error, in milliseconds.
    /// Independent of --request-interval-ms. Defaults to 3000.
    #[arg(long)]
    retry_interval_ms: Option<u64>,

    /// Optional CONFIG.toml, symbols, and FWOB tokens.
    #[arg(value_name = "ITEM", num_args = 0..)]
    items: Vec<String>,
}

impl DownloadArgs {
    fn run(self) -> Result<()> {
        let (config_path, items) = split_config_target(self.items)?;
        let parsed = parse_tokens(&items)?;
        let mut config = match &config_path {
            Some(path) => Config::read(path)
                .with_context(|| format!("failed to read config {}", path.display()))?,
            None => Config::default(),
        };

        if let Some(output) = self.output {
            config.download.output_dir = output;
        }
        if let Some(provider) = parsed.provider {
            config.download.provider = provider;
        }
        if let Some(host) = self.host {
            config.ibkr.host = host;
        }
        if let Some(port) = self.port {
            config.ibkr.port = port;
        }
        if let Some(client_id) = self.client_id {
            config.ibkr.client_id = client_id;
        }
        if let Some(reconnect_timeout_seconds) = self.reconnect_timeout_seconds {
            config.ibkr.reconnect_timeout_seconds = reconnect_timeout_seconds;
        }
        if let Some(stall_timeout_seconds) = self.stall_timeout_seconds {
            config.ibkr.stall_timeout_seconds = stall_timeout_seconds;
        }
        if let Some(start) = self.start {
            config.download.start = Some(start);
        }
        if let Some(end) = self.end {
            config.download.end = Some(end);
        }
        if self.rth && self.all_hours {
            bail!("--rth and --all-hours are mutually exclusive");
        }
        if self.rth {
            config.download.use_rth = true;
        }
        if self.all_hours {
            config.download.use_rth = false;
        }
        if let Some(parallelism) = self.parallelism {
            if parallelism == 0 {
                bail!("--parallelism must be at least 1");
            }
            config.download.parallelism = parallelism;
        }
        if let Some(request_interval_ms) = self.request_interval_ms {
            config.download.request_interval_ms = request_interval_ms;
        }
        if let Some(retry_interval_ms) = self.retry_interval_ms {
            config.download.retry_interval_ms = retry_interval_ms;
        }
        if let Some(timezone) = self.timezone {
            config.download.timezone = timezone;
        }

        let mut fwob = parsed.options;
        if let Some(level) = self.zstd_level {
            validate_zstd_level(level)?;
            fwob.zstd_level = level;
            fwob.explicit_v2_options = true;
        }

        let symbols = parsed.symbols;
        if config_path.is_none() && symbols.is_empty() {
            bail!("symbols are required when no config file is supplied");
        }

        if config_path.is_none() {
            config.stocks.push(StockContractConfig {
                symbols,
                currency: "USD".into(),
                exchange: "SMART".into(),
                primary_exchange: self.primary_exchange,
            });
        } else if !symbols.is_empty() {
            config.filter_symbols(&symbols);
        }

        let plan = DownloadPlan::new(config, fwob)?;
        Downloader::new(plan).run()
    }
}

fn split_config_target(items: Vec<String>) -> Result<(Option<PathBuf>, Vec<String>)> {
    let Some((first, rest)) = items.split_first() else {
        return Ok((None, Vec::new()));
    };
    if first.to_ascii_lowercase().ends_with(".toml") {
        return Ok((Some(PathBuf::from(first)), rest.to_vec()));
    }
    if items
        .iter()
        .skip(1)
        .any(|item| item.to_ascii_lowercase().ends_with(".toml"))
    {
        bail!("config file must be the first positional argument");
    }
    Ok((None, items))
}

#[derive(Debug, Args)]
struct VerifyArgs {
    path: PathBuf,
}

impl VerifyArgs {
    fn run(self) -> Result<()> {
        fwob::Maintenance::verify(&self.path, fwob::ReaderOptions::default())
            .with_context(|| format!("failed to verify {}", self.path.display()))?;
        println!("verified {}", self.path.display());
        Ok(())
    }
}

#[derive(Debug, Args)]
#[command(
    override_usage = "mdfwob stat [CONFIG.toml] [PATHS_OR_SYMBOLS...] [START..END] [FORMAT] [OPTIONS]"
)]
#[command(after_help = "Tokens (case-sensitive, any order):
  paths/symbols: FILE.fwob, a DIR, or a bare SYMBOL (resolved under output_dir)
  formats: table (default), csv, md, jsonl
  session: rth (keep only regular-trading-hours ticks)
  time range: START..END (either side optional), e.g. 2024-01-01..2026-01-01 or ..2026-01-01
              bare dates/times use the exchange tz; add Z or +/-HH for an absolute instant")]
struct StatArgs {
    /// Window start. Bare dates/times use the exchange tz; add Z or +/-HH for an absolute
    /// instant. Overrides the start side of a START..END token.
    #[arg(long)]
    start: Option<String>,
    /// Window end (a bare date is inclusive of the whole local day). Overrides a START..END token.
    #[arg(long)]
    end: Option<String>,
    /// Keep only regular-trading-hours ticks.
    #[arg(long = "use-rth")]
    use_rth: bool,
    /// Override the session window (HH:MM-HH:MM). Default 09:30-16:00 (rth) / 04:00-20:00.
    #[arg(long)]
    session: Option<String>,
    /// Override the session timezone (IANA name). Default America/New_York.
    #[arg(long)]
    tz: Option<String>,
    /// Intra-day tick spacing (seconds) above which a gap is counted. Default 60.
    #[arg(long = "max-gap")]
    max_gap: Option<u32>,
    /// Optional CONFIG.toml, then files/dirs/symbols and tokens (see below).
    #[arg(value_name = "ITEM", num_args = 0..)]
    items: Vec<String>,
}

impl StatArgs {
    fn run(self) -> Result<()> {
        let (config_path, tokens) = split_config_target(self.items)?;
        let acfg = load_analysis_config(config_path.as_deref())?;
        let StatTokens {
            paths,
            format,
            use_rth,
            start: range_start,
            end: range_end,
        } = classify_paths_format(&tokens)?;
        let use_rth = self.use_rth || use_rth;
        let frame = match format {
            AnalysisFormat::Fwob => bail!("stat does not support fwob output"),
            AnalysisFormat::Frame(frame) => frame,
        };
        let session = resolve_session(&acfg, use_rth, self.session.as_deref(), self.tz.as_deref())?;
        let start = self.start.clone().or(range_start);
        let end = self.end.clone().or(range_end);
        let (start, end) = parse_bounds(start.as_deref(), end.as_deref(), &session.time_zone())?;
        let max_gap = self.max_gap.unwrap_or(acfg.stat.max_gap);
        let files = resolve_files(&paths, &acfg)?;
        let query = TickQuery {
            start,
            end,
            session: use_rth.then(|| session.clone()),
        };

        let mut rows = Vec::new();
        let mut failures = 0u32;
        for path in &files {
            let result = (|| -> Result<_> {
                let (mut reader, symbol) = open_tick_reader(path)?;
                let format_label = format_label(path)?;
                let mut acc = StatAccumulator::new(max_gap, &session);
                stream_ticks(&mut reader, &query, |tick| {
                    acc.push(&tick);
                    Ok(())
                })?;
                Ok(acc.finish(symbol, format_label))
            })();
            match result {
                Ok(row) => rows.push(row),
                Err(error) => {
                    failures += 1;
                    tracing::error!(path = %path.display(), error = %format!("{error:#}"), "failed to read");
                }
            }
        }
        if rows.is_empty() && failures > 0 {
            bail!("all {failures} input(s) failed to read");
        }

        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        write_stat(&rows, frame, &mut out)?;
        out.flush()?;
        Ok(())
    }
}

#[derive(Debug, Args)]
#[command(
    override_usage = "mdfwob bars [CONFIG.toml] [PATHS_OR_SYMBOLS...] [INTERVAL] [START..END] [FORMAT] [OPTIONS]"
)]
#[command(after_help = "Tokens (case-sensitive, any order):
  paths/symbols: FILE.fwob, a DIR, or a bare SYMBOL (resolved under output_dir)
  interval: e.g. 30s, 5m, 1h, 1d, 1w, 1mo, 1y (default 1d)
  formats: table (default), csv, md, jsonl, raw, hex, fwob
  session: rth (keep only regular-trading-hours ticks)
  fill: forward-fill empty intervals within a session
  time range: START..END (either side optional), e.g. 2024-01-01..2026-01-01 or ..2026-01-01
              bare dates/times use the exchange tz; add Z or +/-HH for an absolute instant")]
struct BarsArgs {
    /// Window start. Bare dates/times use the exchange tz; add Z or +/-HH for an absolute
    /// instant. Overrides the start side of a START..END token.
    #[arg(long)]
    start: Option<String>,
    /// Window end (a bare date is inclusive of the whole local day). Overrides a START..END token.
    #[arg(long)]
    end: Option<String>,
    /// Keep only regular-trading-hours ticks.
    #[arg(long = "use-rth")]
    use_rth: bool,
    /// Override the session window (HH:MM-HH:MM). Default 09:30-16:00 (rth) / 04:00-20:00.
    #[arg(long)]
    session: Option<String>,
    /// Override the session timezone (IANA name). Default America/New_York.
    #[arg(long)]
    tz: Option<String>,
    /// Output directory for `fwob` format (one `<symbol>.fwob` per symbol).
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(value_name = "ITEM", num_args = 0..)]
    items: Vec<String>,
}

impl BarsArgs {
    fn run(self) -> Result<()> {
        let (config_path, tokens) = split_config_target(self.items)?;
        let acfg = load_analysis_config(config_path.as_deref())?;
        let BarsTokens {
            paths,
            interval: interval_token,
            format,
            use_rth,
            fill,
            start: range_start,
            end: range_end,
        } = classify_with_interval(&tokens)?;
        let use_rth = self.use_rth || use_rth;
        let interval = resolve_interval(interval_token, acfg.bars.interval.as_deref())?;
        let fill = fill || acfg.bars.fill;
        // Resolve the session: its timezone anchors calendar bars, its open anchors intraday
        // buckets, and its window filters ticks only when --use-rth is set.
        let session = resolve_session(&acfg, use_rth, self.session.as_deref(), self.tz.as_deref())?;
        warn_uneven_interval(interval, &session, use_rth);
        let clock = BarClock::Session(session.clone());
        let start = self.start.clone().or(range_start);
        let end = self.end.clone().or(range_end);
        let (start, end) = parse_bounds(start.as_deref(), end.as_deref(), &session.time_zone())?;
        let files = resolve_files(&paths, &acfg)?;
        let query = TickQuery {
            start,
            end,
            session: use_rth.then(|| session.clone()),
        };

        // Group input files by the symbol they report, preserving discovery order so a symbol's
        // files feed one resampler as a single ascending stream. Only the header of each file is
        // read here; the heavy tick scan happens during streaming.
        let mut by_symbol: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for path in &files {
            match tick_symbol(path) {
                Ok(symbol) => by_symbol.entry(symbol).or_default().push(path.clone()),
                Err(error) => {
                    tracing::error!(path = %path.display(), error = %format!("{error:#}"), "failed to read")
                }
            }
        }

        let out_dir = self.output.clone().or_else(|| acfg.output_dir.clone());
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());

        match format {
            AnalysisFormat::Fwob => {
                let dir = out_dir
                    .as_deref()
                    .context("bars --format fwob requires --output DIR")?;
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("failed to create {}", dir.display()))?;
                for (symbol, paths) in &by_symbol {
                    let mut bars = Vec::new();
                    stream_bars(paths, interval, &clock, &query, fill, |bar| {
                        bars.push(bar);
                        Ok(())
                    })?;
                    write_bars_fwob(symbol, &bars, dir)?;
                }
            }
            AnalysisFormat::Frame(frame) => {
                // Each completed bucket streams a row straight to stdout, so the table fills in
                // bar by bar instead of appearing all at once after the whole file is processed.
                let include_symbol = by_symbol.len() > 1;
                guard_symbol_count(include_symbol, by_symbol.len())?;
                let base = bar_schema();
                let schema = if include_symbol {
                    with_symbol_column(&base)
                } else {
                    base
                };
                let symbols: Vec<String> = by_symbol.keys().cloned().collect();
                let strings: &[String] = if include_symbol { &symbols } else { &[] };
                // On an interactive terminal flush each row so bars appear as their buckets close;
                // when redirected, stay buffered for throughput.
                let autoflush = std::io::stdout().is_terminal();
                let mut stream = BarStream::new(&schema, strings, frame, autoflush, &mut out)?;
                for (index, (_symbol, paths)) in by_symbol.iter().enumerate() {
                    stream_bars(paths, interval, &clock, &query, fill, |bar| {
                        stream.emit(index, &bar)
                    })?;
                }
            }
        }
        out.flush()?;
        Ok(())
    }
}

/// Streams a symbol's bars to `sink` as each bucket closes, feeding all of `paths` through one
/// resampler (so multiple files of the same symbol form a single ascending stream) and applying
/// forward-fill when requested. Ticks are read in bulk chunks and never fully materialized.
fn stream_bars(
    paths: &[PathBuf],
    interval: Interval,
    clock: &BarClock,
    query: &TickQuery,
    fill: bool,
    sink: impl FnMut(Bar) -> Result<()>,
) -> Result<()> {
    let mut filler = ForwardFiller::new(interval, clock.clone(), fill, sink);
    let mut resampler = Resampler::new(interval, clock.clone());
    for path in paths {
        let (mut reader, _) = open_tick_reader(path)?;
        stream_ticks(&mut reader, query, |tick| {
            resampler.push(&tick, &mut |bar| filler.push(bar))
        })?;
    }
    resampler.finish(&mut |bar| filler.push(bar))
}

/// Warns when a sub-day interval does not evenly divide the active trading session (RTH or
/// extended hours). Since intraday buckets are anchored to the session open, an interval that
/// tiles the session yields equal-width bars; otherwise the session's last bar is shorter than the
/// rest. Day-and-larger intervals are calendar-anchored and never warn.
fn warn_uneven_interval(interval: Interval, session: &Session, use_rth: bool) {
    let Granularity::SubDay(width) = interval.granularity() else {
        return;
    };
    let length = session.length_seconds();
    if width == 0 || length.is_multiple_of(width) {
        return;
    }
    let kind = if use_rth { "RTH" } else { "extended-hours" };
    tracing::warn!(
        "interval {} does not evenly divide the {kind} session ({}); the last bar of each session will be shorter than the rest",
        interval.label(),
        fmt_session_len(length)
    );
}

/// Formats a session length in seconds as a compact `HhMm` label (e.g. `6h30m`, `16h`).
fn fmt_session_len(seconds: u32) -> String {
    let minutes = seconds / 60;
    let (hours, mins) = (minutes / 60, minutes % 60);
    match (hours, mins) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m}m"),
    }
}

#[derive(Debug, Args)]
#[command(
    override_usage = "mdfwob calc [CONFIG.toml] [PATHS_OR_SYMBOLS...] [INTERVAL] SPEC... [START..END] [FORMAT] [OPTIONS]"
)]
#[command(after_help = "Tokens (case-sensitive, any order):
  paths/symbols: FILE.fwob, a DIR, or a bare SYMBOL (resolved under output_dir)
  indicator specs: sma:N ema:N rsi:N ret:log ret:simple vol:N
  interval: e.g. 5m, 1h, 1d (resamples a tick file; default 1d; ignored for bar files)
  formats: table (default), csv, md, jsonl, raw, hex, fwob
  session: rth (keep only regular-trading-hours ticks)
  fill: forward-fill empty intervals within a session
  time range: START..END (either side optional), e.g. 2024-01-01..2026-01-01 or ..2026-01-01
              bare dates/times use the exchange tz; add Z or +/-HH for an absolute instant")]
struct CalcArgs {
    /// Window start. Bare dates/times use the exchange tz; add Z or +/-HH for an absolute
    /// instant. Overrides the start side of a START..END token.
    #[arg(long)]
    start: Option<String>,
    /// Window end (a bare date is inclusive of the whole local day). Overrides a START..END token.
    #[arg(long)]
    end: Option<String>,
    /// Keep only regular-trading-hours ticks.
    #[arg(long = "use-rth")]
    use_rth: bool,
    /// Override the session window (HH:MM-HH:MM). Default 09:30-16:00 (rth) / 04:00-20:00.
    #[arg(long)]
    session: Option<String>,
    /// Override the session timezone (IANA name). Default America/New_York.
    #[arg(long)]
    tz: Option<String>,
    #[arg(long)]
    output: Option<PathBuf>,
    /// Return method for the --summary scalars (log|simple). Default log.
    #[arg(long)]
    method: Option<String>,
    /// Print a whole-series return/volatility summary footer.
    #[arg(long)]
    summary: bool,
    /// Annualize the summary realized volatility.
    #[arg(long)]
    annualize: bool,
    /// Annualization factor for --annualize (sqrt scaling). Default 252.
    #[arg(long = "periods-per-year")]
    periods_per_year: Option<f64>,
    #[arg(value_name = "ITEM", num_args = 0..)]
    items: Vec<String>,
}

impl CalcArgs {
    fn run(self) -> Result<()> {
        let (config_path, tokens) = split_config_target(self.items)?;
        let acfg = load_analysis_config(config_path.as_deref())?;
        let CalcTokens {
            paths,
            interval: interval_token,
            specs: spec_tokens,
            format,
            use_rth,
            fill,
            start: range_start,
            end: range_end,
        } = classify_calc(&tokens)?;
        let use_rth = self.use_rth || use_rth;
        if spec_tokens.is_empty() {
            bail!("calc requires at least one indicator spec (e.g. sma:20, ret:log, vol:20)");
        }
        let interval = match interval_token {
            Some(interval) => Some(interval),
            None => match acfg.calc.interval.as_deref() {
                Some(text) => Some(
                    Interval::parse(text)
                        .with_context(|| format!("invalid interval in config: {text:?}"))?
                        .with_context(|| format!("invalid interval in config: {text:?}"))?,
                ),
                None => None,
            },
        };
        let method = match self.method.as_deref() {
            Some(token) => ReturnMethod::from_token(token)
                .ok_or_else(|| anyhow::anyhow!("--method must be log or simple"))?,
            None => acfg.calc.method,
        };
        let annualize = self.annualize || acfg.calc.annualize;
        let periods_per_year = self.periods_per_year.unwrap_or(acfg.calc.periods_per_year);
        let fill = fill || acfg.calc.fill;
        let session = resolve_session(&acfg, use_rth, self.session.as_deref(), self.tz.as_deref())?;
        if let Some(iv) = interval {
            warn_uneven_interval(iv, &session, use_rth);
        }
        let clock = BarClock::Session(session.clone());
        let filter = use_rth.then(|| session.clone());
        let start = self.start.clone().or(range_start);
        let end = self.end.clone().or(range_end);
        let (start, end) = parse_bounds(start.as_deref(), end.as_deref(), &session.time_zone())?;
        let files = resolve_files(&paths, &acfg)?;

        let mut groups: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
        for path in &files {
            let result = (|| -> Result<(String, Vec<Bar>)> {
                match input_kind(path)? {
                    InputKind::Bar => read_bars(path),
                    InputKind::Tick => {
                        let interval = interval.context(
                            "an interval token is required to resample a tick file (e.g. 5m, 1h, 1d)",
                        )?;
                        let symbol = tick_symbol(path)?;
                        let query = TickQuery {
                            start,
                            end,
                            session: filter.clone(),
                        };
                        let mut bars = Vec::new();
                        stream_bars(
                            std::slice::from_ref(path),
                            interval,
                            &clock,
                            &query,
                            fill,
                            |bar| {
                                bars.push(bar);
                                Ok(())
                            },
                        )?;
                        Ok((symbol, bars))
                    }
                }
            })();
            match result {
                Ok((symbol, mut bars)) => groups.entry(symbol).or_default().append(&mut bars),
                Err(error) => {
                    tracing::error!(path = %path.display(), error = %format!("{error:#}"), "failed to read");
                }
            }
        }

        let mut series = Vec::new();
        for (symbol, mut bars) in groups {
            bars.sort_by_key(|bar| bar.time);
            let columns = {
                let mut calc = Calc::new(&bars);
                for spec in &spec_tokens {
                    let indicator =
                        parse_spec(spec).expect("spec token validated during parsing")?;
                    calc = calc.with_boxed(indicator);
                }
                calc.run().columns
            };
            let summary = if self.summary {
                summarize(&bars, method, annualize, periods_per_year)
            } else {
                None
            };
            series.push(CalcSeries {
                symbol,
                bars,
                columns,
                summary,
            });
        }

        let out_dir = self.output.clone().or_else(|| acfg.output_dir.clone());
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        write_calc(&series, format, out_dir.as_deref(), &mut out)?;
        out.flush()?;
        Ok(())
    }
}

// ---- analysis CLI helpers ----------------------------------------------------

fn load_analysis_config(path: Option<&Path>) -> Result<AnalysisConfig> {
    match path {
        Some(path) => Ok(Config::read(path)
            .with_context(|| format!("failed to read config {}", path.display()))?
            .analysis),
        None => Ok(AnalysisConfig::default()),
    }
}

/// The positional `rth` token enables regular-trading-hours filtering, mirroring
/// the `--use-rth` flag (either turns it on).
const RTH_TOKEN: &str = "rth";

/// The positional `fill` token forward-fills empty intervals with flat bars (`bars`/`calc`).
const FILL_TOKEN: &str = "fill";

struct StatTokens {
    paths: Vec<String>,
    format: AnalysisFormat,
    use_rth: bool,
    start: Option<String>,
    end: Option<String>,
}

fn classify_paths_format(tokens: &[String]) -> Result<StatTokens> {
    let mut paths = Vec::new();
    let mut format = None;
    let mut use_rth = false;
    let mut range = None;
    for token in tokens {
        if token == RTH_TOKEN {
            use_rth = true;
        } else if let Some(parsed) = parse_range_token(token) {
            if range.replace(parsed).is_some() {
                bail!("multiple time-range tokens given");
            }
        } else if let Some(parsed) = AnalysisFormat::parse(token) {
            if format.replace(parsed).is_some() {
                bail!("multiple output format tokens given");
            }
        } else {
            paths.push(token.clone());
        }
    }
    let (start, end) = range.unwrap_or((None, None));
    Ok(StatTokens {
        paths,
        format: format.unwrap_or_default(),
        use_rth,
        start,
        end,
    })
}

struct BarsTokens {
    paths: Vec<String>,
    interval: Option<Interval>,
    format: AnalysisFormat,
    use_rth: bool,
    fill: bool,
    start: Option<String>,
    end: Option<String>,
}

fn classify_with_interval(tokens: &[String]) -> Result<BarsTokens> {
    let mut paths = Vec::new();
    let mut interval = None;
    let mut format = None;
    let mut use_rth = false;
    let mut fill = false;
    let mut range = None;
    for token in tokens {
        if token == RTH_TOKEN {
            use_rth = true;
        } else if token == FILL_TOKEN {
            fill = true;
        } else if let Some(parsed) = parse_range_token(token) {
            if range.replace(parsed).is_some() {
                bail!("multiple time-range tokens given");
            }
        } else if let Some(parsed) = Interval::parse(token) {
            if interval.replace(parsed?).is_some() {
                bail!("multiple interval tokens given");
            }
        } else if let Some(parsed) = AnalysisFormat::parse(token) {
            if format.replace(parsed).is_some() {
                bail!("multiple output format tokens given");
            }
        } else {
            paths.push(token.clone());
        }
    }
    let (start, end) = range.unwrap_or((None, None));
    Ok(BarsTokens {
        paths,
        interval,
        format: format.unwrap_or_default(),
        use_rth,
        fill,
        start,
        end,
    })
}

struct CalcTokens {
    paths: Vec<String>,
    interval: Option<Interval>,
    specs: Vec<String>,
    format: AnalysisFormat,
    use_rth: bool,
    fill: bool,
    start: Option<String>,
    end: Option<String>,
}

fn classify_calc(tokens: &[String]) -> Result<CalcTokens> {
    let mut paths = Vec::new();
    let mut interval = None;
    let mut specs = Vec::new();
    let mut format = None;
    let mut use_rth = false;
    let mut fill = false;
    let mut range = None;
    for token in tokens {
        if token == RTH_TOKEN {
            use_rth = true;
        } else if token == FILL_TOKEN {
            fill = true;
        } else if let Some(parsed) = parse_range_token(token) {
            if range.replace(parsed).is_some() {
                bail!("multiple time-range tokens given");
            }
        } else if let Some(parsed) = Interval::parse(token) {
            if interval.replace(parsed?).is_some() {
                bail!("multiple interval tokens given");
            }
        } else if let Some(parsed) = parse_spec(token) {
            parsed?; // validate now; rebuilt per symbol later
            specs.push(token.clone());
        } else if let Some(parsed) = AnalysisFormat::parse(token) {
            if format.replace(parsed).is_some() {
                bail!("multiple output format tokens given");
            }
        } else {
            paths.push(token.clone());
        }
    }
    let (start, end) = range.unwrap_or((None, None));
    Ok(CalcTokens {
        paths,
        interval,
        specs,
        format: format.unwrap_or_default(),
        use_rth,
        fill,
        start,
        end,
    })
}

fn resolve_interval(token: Option<Interval>, config: Option<&str>) -> Result<Interval> {
    if let Some(interval) = token {
        return Ok(interval);
    }
    if let Some(text) = config {
        return Interval::parse(text)
            .with_context(|| format!("invalid interval in config: {text:?}"))?
            .with_context(|| format!("invalid interval in config: {text:?}"));
    }
    // Default period when neither a token nor a config value is given.
    Ok(Interval::parse("1d")
        .expect("\"1d\" is interval-shaped")
        .expect("\"1d\" is a valid interval"))
}

fn resolve_session(
    acfg: &AnalysisConfig,
    use_rth: bool,
    session_override: Option<&str>,
    tz_override: Option<&str>,
) -> Result<Session> {
    if session_override.is_none() && tz_override.is_none() {
        return acfg.session(use_rth);
    }
    let base = if use_rth { &acfg.rth } else { &acfg.extended };
    let tz = tz_override.unwrap_or(&base.tz);
    let default_hours = if use_rth {
        DEFAULT_RTH_HOURS
    } else {
        DEFAULT_EXTENDED_HOURS
    };
    let hours = match session_override {
        Some(hours) => hours,
        None if !base.hours.trim().is_empty() => base.hours.as_str(),
        None => default_hours,
    };
    Session::new(tz, hours)
}

fn parse_bounds(
    start: Option<&str>,
    end: Option<&str>,
    tz: &jiff::tz::TimeZone,
) -> Result<(Option<u32>, Option<u32>)> {
    let start = start.map(|s| parse_time_bound(s, false, tz)).transpose()?;
    let end = end.map(|s| parse_time_bound(s, true, tz)).transpose()?;
    Ok((start, end))
}

/// Parses a `--start`/`--end` (or range-token) bound into a UTC epoch second.
///
/// A value carrying an explicit UTC offset or `Z` is an absolute instant; a bare local date-time
/// (`2026-01-01T09:30:00`) or a bare date (`2026-01-01`) is interpreted in the exchange timezone
/// `tz`, which is far less surprising for exchange data than UTC. A bare *end* date is inclusive,
/// expanding to the very end of that local day.
fn parse_time_bound(value: &str, is_end: bool, tz: &jiff::tz::TimeZone) -> Result<u32> {
    use jiff::{Timestamp, civil};

    let to_u32 = |secs: i64| {
        u32::try_from(secs).map_err(|_| anyhow::anyhow!("time {value:?} is out of range"))
    };

    // Absolute instant: carries a `Z` or numeric UTC offset.
    if let Ok(ts) = value.parse::<Timestamp>() {
        return to_u32(ts.as_second());
    }
    // Bare date (no time component): exchange-tz midnight; an end date includes the whole local
    // day. Checked before DateTime because jiff's DateTime parser also accepts a bare date.
    if !value.contains(':')
        && let Ok(date) = value.parse::<civil::Date>()
    {
        let day = if is_end { date.tomorrow()? } else { date };
        let secs = day.to_zoned(tz.clone())?.timestamp().as_second() - i64::from(is_end);
        return to_u32(secs);
    }
    // Local date-time without an offset: interpret in the exchange timezone.
    if let Ok(dt) = value.parse::<civil::DateTime>() {
        return to_u32(dt.to_zoned(tz.clone())?.timestamp().as_second());
    }
    bail!("invalid date/time {value:?} (use YYYY-MM-DD, a local date-time, or an RFC3339 instant)")
}

/// True when `value` parses as some date/datetime/instant — used to tell a `START..END` range
/// token apart from a path like `..\AAPL.fwob`. Timezone-free; the actual conversion is deferred.
fn looks_like_bound(value: &str) -> bool {
    value.parse::<jiff::Timestamp>().is_ok()
        || value.parse::<jiff::civil::DateTime>().is_ok()
        || value.parse::<jiff::civil::Date>().is_ok()
}

/// Parses a `START..END` range token (either side optional, e.g. `2024-01-01T12:00:00Z..2026-01-01`
/// or `..2026-01-01`) into raw bound strings. Returns `None` when the token is not range-shaped, so
/// it falls through to path/symbol handling (e.g. a relative path `..\AAPL.fwob`). Each present
/// side must look like a date/time; conversion to epochs happens later in [`parse_bounds`].
fn parse_range_token(token: &str) -> Option<(Option<String>, Option<String>)> {
    let (lhs, rhs) = token.split_once("..")?;
    if lhs.is_empty() && rhs.is_empty() {
        return None;
    }
    if !lhs.is_empty() && !looks_like_bound(lhs) {
        return None;
    }
    if !rhs.is_empty() && !looks_like_bound(rhs) {
        return None;
    }
    Some((
        (!lhs.is_empty()).then(|| lhs.to_owned()),
        (!rhs.is_empty()).then(|| rhs.to_owned()),
    ))
}

fn format_label(path: &Path) -> Result<String> {
    Ok(match detect_format(path)? {
        FormatVersion::V1 => "fwob-v1".to_owned(),
        FormatVersion::V2 => "fwob-v2".to_owned(),
    })
}

fn analysis_output_dir(acfg: &AnalysisConfig) -> PathBuf {
    acfg.output_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."))
}

fn resolve_files(paths: &[String], acfg: &AnalysisConfig) -> Result<Vec<PathBuf>> {
    let tokens = if paths.is_empty() && !acfg.symbols.is_empty() {
        acfg.symbols.clone()
    } else {
        paths.to_vec()
    };
    discover_inputs(&tokens, &analysis_output_dir(acfg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderKind;

    fn ny() -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get("America/New_York").unwrap()
    }

    fn epoch(rfc3339: &str) -> u32 {
        rfc3339.parse::<jiff::Timestamp>().unwrap().as_second() as u32
    }

    #[test]
    fn bare_dates_and_times_use_the_exchange_timezone() {
        let tz = ny();
        // Bare date -> exchange-tz midnight (winter ET = UTC-5).
        assert_eq!(
            parse_time_bound("2026-01-01", false, &tz).unwrap(),
            epoch("2026-01-01T05:00:00Z")
        );
        // Bare end date is inclusive of the whole local day (next ET midnight minus one second).
        assert_eq!(
            parse_time_bound("2026-01-01", true, &tz).unwrap(),
            epoch("2026-01-02T05:00:00Z") - 1
        );
        // Bare local date-time -> interpreted in the exchange tz.
        assert_eq!(
            parse_time_bound("2026-01-01T09:30:00", false, &tz).unwrap(),
            epoch("2026-01-01T14:30:00Z")
        );
        // An explicit Z / offset is absolute, regardless of the exchange tz.
        assert_eq!(
            parse_time_bound("2026-01-01T05:00:00Z", false, &tz).unwrap(),
            epoch("2026-01-01T05:00:00Z")
        );
        assert_eq!(
            parse_time_bound("2026-01-01T00:00:00-05:00", false, &tz).unwrap(),
            epoch("2026-01-01T05:00:00Z")
        );
    }

    #[test]
    fn range_token_parses_either_side_and_ignores_paths() {
        assert_eq!(
            parse_range_token("2024-01-01T12:00:00Z..2026-01-01"),
            Some((
                Some("2024-01-01T12:00:00Z".to_owned()),
                Some("2026-01-01".to_owned())
            ))
        );
        assert_eq!(
            parse_range_token("..2026-01-01"),
            Some((None, Some("2026-01-01".to_owned())))
        );
        assert_eq!(
            parse_range_token("2024-01-01.."),
            Some((Some("2024-01-01".to_owned()), None))
        );
        // Not range-shaped: a relative path, a plain symbol, or a bare `..`.
        assert_eq!(parse_range_token(r"..\AAPL.fwob"), None);
        assert_eq!(parse_range_token("../data/AAPL.fwob"), None);
        assert_eq!(parse_range_token("AAPL"), None);
        assert_eq!(parse_range_token(".."), None);
    }

    #[test]
    fn interval_defaults_to_one_day() {
        assert_eq!(resolve_interval(None, None).unwrap().label(), "1d");
        // A config value still wins over the default.
        assert_eq!(resolve_interval(None, Some("5m")).unwrap().label(), "5m");
    }

    #[test]
    fn provider_override_is_a_positional_token_only() {
        assert!(Cli::try_parse_from(["mdfwob", "download", "--provider", "databento"]).is_err());

        let cli = Cli::try_parse_from(["mdfwob", "download", "databento", "AAPL"]).unwrap();
        let Command::Download(args) = cli.command else {
            panic!("expected download command");
        };
        let (_, items) = split_config_target(args.items).unwrap();
        let parsed = parse_tokens(&items).unwrap();
        assert_eq!(parsed.provider, Some(ProviderKind::Databento));
        assert_eq!(parsed.symbols, ["AAPL"]);
    }

    #[test]
    fn request_interval_is_accepted_as_an_ad_hoc_override() {
        let cli = Cli::try_parse_from([
            "mdfwob",
            "download",
            "SPCX",
            "--request-interval-ms",
            "1000",
        ])
        .unwrap();
        let Command::Download(args) = cli.command else {
            panic!("expected download command");
        };

        assert_eq!(args.request_interval_ms, Some(1_000));
        assert_eq!(args.items, ["SPCX"]);
    }

    #[test]
    fn retry_interval_is_accepted_as_an_ad_hoc_override() {
        let cli =
            Cli::try_parse_from(["mdfwob", "download", "SPCX", "--retry-interval-ms", "5000"])
                .unwrap();
        let Command::Download(args) = cli.command else {
            panic!("expected download command");
        };

        assert_eq!(args.retry_interval_ms, Some(5_000));
        assert_eq!(args.items, ["SPCX"]);
    }

    #[test]
    fn reconnect_timeout_is_accepted_as_an_ibkr_override() {
        for (value, expected) in [("-1", -1), ("0", 0), ("1234", 1234)] {
            let cli = Cli::try_parse_from([
                "mdfwob",
                "download",
                "SPCX",
                "--reconnect-timeout-seconds",
                value,
            ])
            .unwrap();
            let Command::Download(args) = cli.command else {
                panic!("expected download command");
            };
            assert_eq!(args.reconnect_timeout_seconds, Some(expected));
        }
    }

    #[test]
    fn stall_timeout_is_accepted_as_an_ibkr_override() {
        for (value, expected) in [("0", 0u64), ("45", 45)] {
            let cli =
                Cli::try_parse_from(["mdfwob", "download", "SPCX", "--stall-timeout-seconds", value])
                    .unwrap();
            let Command::Download(args) = cli.command else {
                panic!("expected download command");
            };
            assert_eq!(args.stall_timeout_seconds, Some(expected));
        }
    }
}
