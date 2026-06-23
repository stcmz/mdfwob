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
#[command(override_usage = "mdfwob stat [CONFIG.toml] [PATHS_OR_SYMBOLS...] [FORMAT] [OPTIONS]")]
struct StatArgs {
    /// Start of the time window (YYYY-MM-DD or RFC3339, UTC).
    #[arg(long)]
    start: Option<String>,
    /// End of the time window (YYYY-MM-DD or RFC3339, UTC). A bare date is inclusive.
    #[arg(long)]
    end: Option<String>,
    /// Keep only regular-trading-hours ticks.
    #[arg(long = "use-rth")]
    use_rth: bool,
    /// Override the session window (HH:MM-HH:MM).
    #[arg(long)]
    session: Option<String>,
    /// Override the session timezone (IANA name).
    #[arg(long)]
    tz: Option<String>,
    /// Intra-day tick spacing (seconds) above which a gap is counted.
    #[arg(long = "max-gap")]
    max_gap: Option<u32>,
    /// Optional CONFIG.toml, then files/dirs/symbols and an output format token.
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
        } = classify_paths_format(&tokens)?;
        let use_rth = self.use_rth || use_rth;
        let frame = match format {
            AnalysisFormat::Fwob => bail!("stat does not support fwob output"),
            AnalysisFormat::Frame(frame) => frame,
        };
        let session = resolve_session(&acfg, use_rth, self.session.as_deref(), self.tz.as_deref())?;
        let (start, end) = parse_bounds(self.start.as_deref(), self.end.as_deref())?;
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
    override_usage = "mdfwob bars [CONFIG.toml] [PATHS_OR_SYMBOLS...] INTERVAL [FORMAT] [OPTIONS]"
)]
struct BarsArgs {
    #[arg(long)]
    start: Option<String>,
    #[arg(long)]
    end: Option<String>,
    #[arg(long = "use-rth")]
    use_rth: bool,
    #[arg(long)]
    session: Option<String>,
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
        } = classify_with_interval(&tokens)?;
        let use_rth = self.use_rth || use_rth;
        let interval = resolve_interval(interval_token, acfg.bars.interval.as_deref())?;
        let fill = fill || acfg.bars.fill;
        // Resolve the session: its timezone anchors calendar bars, its open anchors intraday
        // buckets, and its window filters ticks only when --use-rth is set.
        let session = resolve_session(&acfg, use_rth, self.session.as_deref(), self.tz.as_deref())?;
        warn_uneven_interval(interval, &session, use_rth);
        let clock = BarClock::Session(session.clone());
        let (start, end) = parse_bounds(self.start.as_deref(), self.end.as_deref())?;
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
    override_usage = "mdfwob calc [CONFIG.toml] [PATHS_OR_SYMBOLS...] [INTERVAL] SPEC... [FORMAT] [OPTIONS]"
)]
#[command(
    after_help = "Indicator specs: sma:N ema:N rsi:N ret:log ret:simple vol:N
INTERVAL (e.g. 5m, 1h, 1d) is required when an input is a tick file."
)]
struct CalcArgs {
    #[arg(long)]
    start: Option<String>,
    #[arg(long)]
    end: Option<String>,
    #[arg(long = "use-rth")]
    use_rth: bool,
    #[arg(long)]
    session: Option<String>,
    #[arg(long)]
    tz: Option<String>,
    #[arg(long)]
    output: Option<PathBuf>,
    /// Return method for the --summary scalars (log|simple).
    #[arg(long)]
    method: Option<String>,
    /// Print a whole-series return/volatility summary footer.
    #[arg(long)]
    summary: bool,
    /// Annualize the summary realized volatility.
    #[arg(long)]
    annualize: bool,
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
        let (start, end) = parse_bounds(self.start.as_deref(), self.end.as_deref())?;
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
}

fn classify_paths_format(tokens: &[String]) -> Result<StatTokens> {
    let mut paths = Vec::new();
    let mut format = None;
    let mut use_rth = false;
    for token in tokens {
        if token == RTH_TOKEN {
            use_rth = true;
        } else if let Some(parsed) = AnalysisFormat::parse(token) {
            if format.replace(parsed).is_some() {
                bail!("multiple output format tokens given");
            }
        } else {
            paths.push(token.clone());
        }
    }
    Ok(StatTokens {
        paths,
        format: format.unwrap_or_default(),
        use_rth,
    })
}

struct BarsTokens {
    paths: Vec<String>,
    interval: Option<Interval>,
    format: AnalysisFormat,
    use_rth: bool,
    fill: bool,
}

fn classify_with_interval(tokens: &[String]) -> Result<BarsTokens> {
    let mut paths = Vec::new();
    let mut interval = None;
    let mut format = None;
    let mut use_rth = false;
    let mut fill = false;
    for token in tokens {
        if token == RTH_TOKEN {
            use_rth = true;
        } else if token == FILL_TOKEN {
            fill = true;
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
    Ok(BarsTokens {
        paths,
        interval,
        format: format.unwrap_or_default(),
        use_rth,
        fill,
    })
}

struct CalcTokens {
    paths: Vec<String>,
    interval: Option<Interval>,
    specs: Vec<String>,
    format: AnalysisFormat,
    use_rth: bool,
    fill: bool,
}

fn classify_calc(tokens: &[String]) -> Result<CalcTokens> {
    let mut paths = Vec::new();
    let mut interval = None;
    let mut specs = Vec::new();
    let mut format = None;
    let mut use_rth = false;
    let mut fill = false;
    for token in tokens {
        if token == RTH_TOKEN {
            use_rth = true;
        } else if token == FILL_TOKEN {
            fill = true;
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
    Ok(CalcTokens {
        paths,
        interval,
        specs,
        format: format.unwrap_or_default(),
        use_rth,
        fill,
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
    bail!("an interval is required (e.g. 5m, 1h, 1d)")
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

fn parse_bounds(start: Option<&str>, end: Option<&str>) -> Result<(Option<u32>, Option<u32>)> {
    let start = start.map(|s| parse_time_bound(s, false)).transpose()?;
    let end = end.map(|s| parse_time_bound(s, true)).transpose()?;
    Ok((start, end))
}

fn parse_time_bound(value: &str, is_end: bool) -> Result<u32> {
    use jiff::{Timestamp, civil::Date, tz::TimeZone};

    if let Ok(date) = value.parse::<Date>() {
        let start = date.to_zoned(TimeZone::UTC)?.timestamp().as_second();
        let secs = if is_end { start + 86_400 - 1 } else { start };
        return u32::try_from(secs).map_err(|_| anyhow::anyhow!("time {value:?} is out of range"));
    }
    let ts: Timestamp = value
        .parse()
        .with_context(|| format!("invalid date/time {value:?} (use YYYY-MM-DD or RFC3339)"))?;
    u32::try_from(ts.as_second()).map_err(|_| anyhow::anyhow!("time {value:?} is out of range"))
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
}
