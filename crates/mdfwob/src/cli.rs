use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use fwob::formatting::FrameFormat;
use fwob::toml::TomlWriter;
use fwob::{FormatVersion, Reader};
use fwob_core::Key;

use std::collections::BTreeMap;

use crate::{
    analysis::{
        calc::{StreamingIndicator, parse_spec, parse_streaming_spec},
        config::{AnalysisConfig, DEFAULT_EXTENDED_HOURS, DEFAULT_RTH_HOURS},
        inspect::{
            classify_hours, detect_bar_granularity, field_semantic_label, field_type_label,
            preview_bars, preview_rows, preview_ticks, sample_windows,
        },
        interval::{Granularity, Interval},
        ls::{LsFormat, ls_file, write_ls},
        model::{Bar, Tick},
        output::{
            AnalysisFormat, FrameStream, FrameWriter, format_epoch_tz, guard_symbol_count,
            write_stat,
        },
        plot::{Canvas, PlotOptions, Series, render},
        read::{
            InputKind, TickQuery, decode_tick, detect_kind, discover_inputs, file_symbol,
            input_kind, open_tick_reader, stream_bars_file, stream_ticks,
        },
        resample::{BarClock, BarResampler, ForwardFiller, Resampler},
        schema::{
            bar_schema, calc_schema, decode_bar, encode_bar, encode_calc_row, with_symbol_column,
        },
        session::Session,
        stat::stat_file,
        summary::{SummaryCollector, SummaryColumn},
    },
    config::{Config, StockContractConfig},
    downloader::{DownloadPlan, Downloader, parse_epoch_seconds},
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
            Command::Ls(args) => args.run(),
            Command::Inspect(args) => args.run(),
            Command::Verify(args) => args.run(),
            Command::Stat(args) => args.run(),
            Command::Bars(args) => args.run(),
            Command::Calc(args) => args.run(),
            Command::Plot(args) => args.run(),
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Download configured or ad hoc historical market data.
    Download(DownloadArgs),
    /// List tick/bar files: one tz-aware row per file (table/md/csv/jsonl; no full scan).
    Ls(LsArgs),
    /// Quick metadata overview of a tick or bar file (colored, tz-aware TOML; no full scan).
    Inspect(InspectArgs),
    /// Verify a tick or bar file's integrity and schema, with a scanned market summary.
    Verify(VerifyArgs),
    /// Summarize tick or bar files: one row per file with price/volume stats.
    Stat(StatArgs),
    /// Resample ticks (or re-resample bars) into OHLCV bars (table/csv/md/jsonl/raw/hex/fwob).
    Bars(BarsArgs),
    /// Compute per-bar indicator series (sma/ema/rsi/ret/vol) over bars or ticks.
    Calc(CalcArgs),
    /// Render OHLC bars as a candlestick chart (Sixel to the console, or a PNG file).
    Plot(PlotArgs),
}

/// Shared help text describing every built-in indicator spec (what it is good for and how it is
/// computed). Appended to both the `calc` and `plot` usage via `concat!` so they stay in sync.
macro_rules! indicator_guide {
    () => {
        "\n\nIndicators (N = number of bars; computed on close unless noted):
  sma:N       Simple moving average: the mean of the last N closes. A smoothed
              trend baseline; lags price by ~N/2 bars. Warms up over N-1 bars.
  ema:N       Exponential moving average: alpha = 2/(N+1), seeded with the N-bar
              SMA. Weights recent closes more, so it turns faster than sma:N.
  dema:N      Double exponential moving average, 2*EMA - EMA(EMA). Cancels most of
              the EMA's lag, hugging price more tightly (at the cost of some noise).
  vsma:N      Simple moving average of volume over N bars (drawn on the volume
              panel). Smooths volume to reveal participation trends.
  vema:N      Exponential moving average of volume (alpha = 2/(N+1)); reacts to a
              volume spike faster than vsma:N.
  vdema:N     Double exponential moving average of volume; low-lag volume trend.
  rsi:N       Wilder's Relative Strength Index (0-100): 100 - 100/(1+RS), where
              RS = Wilder-smoothed avg gain / avg loss over N bars. A momentum
              oscillator; >70 is often overbought, <30 oversold.
  vol:N       Rolling realized volatility: the sample stdev of the last N log
              returns. Gauges how much price is swinging (risk); rises when choppy.
  ret:log     Per-bar log return, ln(close/prev_close). Time-additive, the right
              input for volatility and multi-period return math.
  ret:simple  Per-bar simple return, (close-prev_close)/prev_close: the plain
              percentage change between bars."
    };
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

    /// Seconds a request may stall with no data before it is aborted and retried. 0 disables
    /// stall detection. Defaults to 60.
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

    /// Exchange timezone (IANA name) for day-advance alignment and the download-progress
    /// timestamps shown in log fields (the log-line prefix uses the machine's local time).
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

    /// Minimum interval between provider requests in milliseconds. Defaults to 1000.
    #[arg(long)]
    request_interval_ms: Option<u64>,

    /// Minimum interval between retry attempts after a recoverable error, in milliseconds.
    /// Independent of --request-interval-ms. Defaults to 10000.
    #[arg(long)]
    retry_interval_ms: Option<u64>,

    /// How often (seconds) to commit an in-progress download to disk. -1 commits only at the end;
    /// 0 commits after every batch; positive commits at most that often. Defaults to 60.
    #[arg(long, allow_hyphen_values = true)]
    commit_interval_seconds: Option<i64>,

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
        if let Some(commit_interval_seconds) = self.commit_interval_seconds {
            config.download.commit_interval_seconds = commit_interval_seconds;
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

/// Whether colored TOML should be emitted: a terminal, and `NO_COLOR` unset (matching `fwob`).
fn color_enabled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn kind_label(kind: InputKind) -> &'static str {
    match kind {
        InputKind::Tick => "tick",
        InputKind::Bar => "bar",
    }
}

/// Extracts the `time` epoch second from a boundary [`Key`] (the tick/bar key is a `u32` second).
fn key_epoch(key: Key) -> Option<u32> {
    match key {
        Key::U32(value) => Some(value),
        Key::I64(value) => u32::try_from(value).ok(),
        _ => None,
    }
}

/// Frames sampled per end by `inspect` for granularity/hours detection and the head/tail preview.
const INSPECT_SAMPLE: u64 = 1_024;

#[derive(Debug, Args)]
#[command(override_usage = "mdfwob ls [CONFIG.toml] [PATHS_OR_SYMBOLS...] [FORMAT] [OPTIONS]")]
#[command(
    after_help = "Tokens (any order): tick/bar FILE.fwob, a DIR (its immediate *.fwob), or a \
bare SYMBOL (resolved under output_dir); and one output format: table (default), md, csv, jsonl. \
With no path, lists the current directory's *.fwob files."
)]
struct LsArgs {
    /// Override the session window (HH:MM-HH:MM) used to classify regular vs extended hours.
    #[arg(long)]
    session: Option<String>,
    /// Timezone (IANA name) for rendered timestamps and hours classification. Default
    /// America/New_York.
    #[arg(long)]
    tz: Option<String>,
    /// Optional CONFIG.toml, then files/dirs/symbols and an optional format token.
    #[arg(value_name = "ITEM", num_args = 0..)]
    items: Vec<String>,
}

impl LsArgs {
    fn run(self) -> Result<()> {
        let (config_path, tokens) = split_config_target(self.items)?;
        let acfg = load_analysis_config(config_path.as_deref())?;
        let mut format = None;
        let mut paths = Vec::new();
        for token in &tokens {
            if let Some(parsed) = LsFormat::parse(token) {
                if format.replace(parsed).is_some() {
                    bail!("multiple output format tokens given");
                }
            } else {
                paths.push(token.clone());
            }
        }
        let format = format.unwrap_or_default();
        let display = resolve_session(&acfg, false, self.session.as_deref(), self.tz.as_deref())?;
        let rth = resolve_session(&acfg, true, self.session.as_deref(), self.tz.as_deref())?;
        let tz = display.time_zone();

        // Default to the current directory when no path/symbol is given (like `fwob ls`).
        let files = if paths.is_empty() && acfg.symbols.is_empty() {
            resolve_files(&[".".to_string()], &acfg)?
        } else {
            resolve_files(&paths, &acfg)?
        };

        let mut rows = Vec::new();
        let mut failures = 0u32;
        for path in &files {
            match ls_file(path.display().to_string(), path, &rth, INSPECT_SAMPLE) {
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
        write_ls(&rows, format, &tz, &mut out)?;
        out.flush()?;
        Ok(())
    }
}

#[derive(Debug, Args)]
struct InspectArgs {
    /// A tick or bar FILE.fwob.
    path: PathBuf,
    /// Override the session window (HH:MM-HH:MM) used to classify regular vs extended hours.
    #[arg(long)]
    session: Option<String>,
    /// Timezone (IANA name) for rendered timestamps and hours classification. Default
    /// America/New_York.
    #[arg(long)]
    tz: Option<String>,
}

impl InspectArgs {
    fn run(self) -> Result<()> {
        let acfg = AnalysisConfig::default();
        let display = resolve_session(&acfg, false, self.session.as_deref(), self.tz.as_deref())?;
        let rth = resolve_session(&acfg, true, self.session.as_deref(), self.tz.as_deref())?;
        let tz = display.time_zone();

        let mut reader = Reader::open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        let kind = detect_kind(&reader)
            .with_context(|| format!("failed to inspect {}", self.path.display()))?;
        let format = match reader.format_version() {
            FormatVersion::V1 => "fwob-v1",
            FormatVersion::V2 => "fwob-v2",
        };
        let title = reader.title().to_string();
        let schema = reader.schema().clone();
        let frame_count = reader.frame_count();
        let physical_bytes = std::fs::metadata(&self.path)
            .with_context(|| format!("failed to stat {}", self.path.display()))?
            .len();

        let first = reader.first_key()?.and_then(key_epoch);
        let last = reader.last_key()?.and_then(key_epoch);

        // Sample the same leading + trailing windows `ls` uses (shared `sample_windows`), so their
        // granularity/hours classification is identical. Bounded — never a full scan. The leading
        // window feeds the preview head; the trailing window the preview tail.
        let (lead, tail) = sample_windows(frame_count, INSPECT_SAMPLE);
        let mut times: Vec<u32> = Vec::new();
        let mut lead_ticks: Vec<Tick> = Vec::new();
        let mut lead_bars: Vec<Bar> = Vec::new();
        let mut tail_ticks: Vec<Tick> = Vec::new();
        let mut tail_bars: Vec<Bar> = Vec::new();
        for (range, is_tail) in std::iter::once((lead, false)).chain(tail.map(|t| (t, true))) {
            for frame in reader.frames(range)? {
                let frame = frame?;
                match kind {
                    InputKind::Tick => {
                        let tick = decode_tick(frame.bytes());
                        times.push(tick.time);
                        if is_tail {
                            tail_ticks.push(tick);
                        } else {
                            lead_ticks.push(tick);
                        }
                    }
                    InputKind::Bar => {
                        let bar = decode_bar(frame.bytes())?;
                        times.push(bar.time);
                        if is_tail {
                            tail_bars.push(bar);
                        } else {
                            lead_bars.push(bar);
                        }
                    }
                }
            }
        }

        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        let mut w = TomlWriter::new(&mut out, color_enabled());

        w.section("file")?;
        w.kv_str("format", format)?;
        w.kv_str("title", &title)?;
        w.kv_str("kind", kind_label(kind))?;
        w.kv_str("frame_type", &schema.frame_type)?;
        w.kv_num("key_field_index", schema.key_field_index)?;

        w.blank()?;
        w.section("storage")?;
        w.kv_num("physical_bytes", physical_bytes)?;
        w.kv_num("frame_count", frame_count)?;

        w.blank()?;
        w.section("range")?;
        w.kv_str("timezone", tz.iana_name().unwrap_or("UTC"))?;
        if let Some(first) = first {
            w.kv_str("first", &format_epoch_tz(first, &tz))?;
        }
        if let Some(last) = last {
            w.kv_str("last", &format_epoch_tz(last, &tz))?;
        }
        if kind == InputKind::Bar
            && let Some(granularity) = detect_bar_granularity(&times)
        {
            w.kv_str("granularity", &granularity)?;
        }
        if !times.is_empty() {
            w.kv_str("hours", classify_hours(&times, &rth))?;
        }

        w.blank()?;
        w.section("schema")?;
        w.kv_num("field_count", schema.fields.len())?;
        for field in &schema.fields {
            w.blank()?;
            w.array_section("schema.fields")?;
            w.kv_str("name", &field.name)?;
            w.kv_str("type", field_type_label(field.field_type))?;
            w.kv_num("length", field.length)?;
            w.kv_num("offset", field.offset)?;
            if field.semantic != fwob_core::FieldSemantic::None {
                w.kv_str("semantic", &field_semantic_label(field.semantic))?;
            }
        }

        let preview = match kind {
            InputKind::Tick => {
                preview_ticks(&preview_rows(frame_count, &lead_ticks, &tail_ticks), &tz)
            }
            InputKind::Bar => preview_bars(&preview_rows(frame_count, &lead_bars, &tail_bars), &tz),
        };
        if !preview.is_empty() {
            w.blank()?;
            w.section("frames")?;
            w.kv_multiline("preview", &preview)?;
        }
        out.flush()?;
        Ok(())
    }
}

#[derive(Debug, Args)]
struct VerifyArgs {
    /// A tick or bar FILE.fwob.
    path: PathBuf,
    /// Override the session window (HH:MM-HH:MM); only affects the timezone of rendered timestamps.
    #[arg(long)]
    session: Option<String>,
    /// Timezone (IANA name) for rendered timestamps. Default America/New_York.
    #[arg(long)]
    tz: Option<String>,
}

impl VerifyArgs {
    fn run(self) -> Result<()> {
        let acfg = AnalysisConfig::default();
        let tz =
            resolve_session(&acfg, false, self.session.as_deref(), self.tz.as_deref())?.time_zone();

        // Structural integrity: walk every page/frame (header, ordering, string table).
        let report = fwob::Maintenance::verify(&self.path, fwob::ReaderOptions::default())
            .with_context(|| format!("failed to verify {}", self.path.display()))?;
        // Identity: confirm it is a canonical Tick/Bar file matching our structs.
        let reader = Reader::open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        let kind = detect_kind(&reader)
            .with_context(|| format!("failed to verify {}", self.path.display()))?;
        let title = reader.title().to_string();
        drop(reader);
        // Decoded market summary (the scan reads every frame's fields).
        let row = stat_file(&self.path, &TickQuery::default())?;

        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        let mut w = TomlWriter::new(&mut out, color_enabled());

        w.section("verify")?;
        w.kv_str("status", "ok")?;
        w.kv_str("title", &title)?;
        w.kv_str("kind", kind_label(kind))?;
        w.kv_num("frame_count", report.frame_count)?;
        w.kv_num("string_count", report.string_count)?;
        w.kv_num("file_length", report.file_length)?;

        w.blank()?;
        w.section("data")?;
        w.kv_str("timezone", tz.iana_name().unwrap_or("UTC"))?;
        if let Some(first) = row.first {
            w.kv_str("first", &format_epoch_tz(first, &tz))?;
        }
        if let Some(last) = row.last {
            w.kv_str("last", &format_epoch_tz(last, &tz))?;
        }
        w.kv_num("trades", row.trades)?;
        if row.min.is_finite() {
            w.kv_float("min", row.min, 4)?;
        }
        if row.max.is_finite() {
            w.kv_float("max", row.max, 4)?;
        }
        if row.vwap.is_finite() {
            w.kv_float("vwap", row.vwap, 4)?;
        }
        w.kv_num("volume", row.volume)?;
        out.flush()?;
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
        let files = resolve_files(&paths, &acfg)?;
        let query = TickQuery {
            start,
            end,
            session: use_rth.then(|| session.clone()),
        };

        let mut rows = Vec::new();
        let mut failures = 0u32;
        for path in &files {
            match stat_file(path, &query) {
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
  paths/symbols: a tick or bar FILE.fwob, a DIR, or a bare SYMBOL (resolved under output_dir)
  interval: e.g. 30s, 5m, 1h, 1d, 1w, 1mo, 1y (default 1d; bar files re-resample to it, e.g. 1s->1m)
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

        // Group input files (tick or bar) by the symbol they report, preserving discovery order so
        // a symbol's files feed one resampler as a single ascending stream. Only the header of each
        // file is read here; the heavy scan happens during streaming.
        let mut by_symbol: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for path in &files {
            match file_symbol(path) {
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
                // Stream each bucket straight into the bar file so a fine-interval conversion of a
                // whole tick history (e.g. 1s over billions of ticks) keeps bounded memory instead
                // of buffering the entire bar series first.
                for (symbol, paths) in &by_symbol {
                    let path = dir.join(format!("{symbol}.fwob"));
                    let mut writer = FrameWriter::create(&path, bar_schema(), symbol)?;
                    stream_bars(paths, interval, &clock, &query, fill, |bar| {
                        writer.push(|buf| encode_bar(&bar, buf))
                    })?;
                    writer.finish()?;
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
                let mut stream = FrameStream::new(&schema, strings, frame, autoflush, &mut out)?;
                for (index, (_symbol, paths)) in by_symbol.iter().enumerate() {
                    stream_bars(paths, interval, &clock, &query, fill, |bar| {
                        stream.emit(index, |buf| encode_bar(&bar, buf))
                    })?;
                }
            }
        }
        out.flush()?;
        Ok(())
    }
}

#[derive(Debug, Args)]
#[command(
    override_usage = "mdfwob plot [CONFIG.toml] [PATHS_OR_SYMBOLS...] [INTERVAL] [START..END] [OPTIONS]"
)]
#[command(after_help = concat!("Tokens (case-sensitive, any order):
  paths/symbols: a tick or bar FILE.fwob, a DIR, or a bare SYMBOL (resolved under output_dir)
  interval: e.g. 30s, 5m, 1h, 1d, 1w, 1mo, 1y (default 1d; tick files resample to it, bar files re-resample to it)
  indicator specs: sma:N ema:N dema:N (on price); vsma:N vema:N vdema:N (on volume); rsi:N ret:log ret:simple vol:N (own panel)
  volume: add a volume panel below the candles (same as --volume; implied by vsma/vema/vdema)
  session: rth (keep only regular-trading-hours ticks)
  fill: forward-fill empty intervals within a session
  time range: START..END (either side optional), e.g. 2024-01-01..2026-01-01 or ..2026-01-01
              bare dates/times use the exchange tz; add Z or +/-HH for an absolute instant",
    indicator_guide!(),
    "\n\nWith no indicator specs, a default sma:20 overlay is drawn. By default the chart is written
to the console as a Sixel image (renders inline in Windows Terminal, WezTerm, xterm, iTerm2, ...).
Pass --output to write a file instead: a .png (default) or a .six/.sixel raw Sixel dump."))]
struct PlotArgs {
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
    /// Write the chart to a file (.png, or .six/.sixel for a raw Sixel dump) instead of the
    /// console. Requires a single resolved symbol.
    #[arg(long, short = 'o')]
    output: Option<PathBuf>,
    /// Image width in pixels.
    #[arg(long, default_value_t = crate::analysis::plot::DEFAULT_WIDTH)]
    width: u32,
    /// Image height in pixels.
    #[arg(long, default_value_t = crate::analysis::plot::DEFAULT_HEIGHT)]
    height: u32,
    /// Add a volume panel below the candles (same as the positional `volume` token).
    #[arg(long)]
    volume: bool,
    #[arg(value_name = "ITEM", num_args = 0..)]
    items: Vec<String>,
}

impl PlotArgs {
    fn run(self) -> Result<()> {
        let (config_path, tokens) = split_config_target(self.items)?;
        let acfg = load_analysis_config(config_path.as_deref())?;
        // A bare `volume` token toggles the volume panel (like `rth`/`fill`); pull it out before
        // token classification so it is not mistaken for a symbol.
        let mut volume = self.volume;
        let tokens: Vec<String> = tokens
            .into_iter()
            .filter(|token| {
                if token == VOLUME_TOKEN {
                    volume = true;
                    false
                } else {
                    true
                }
            })
            .collect();
        // `plot` shares `calc`'s token grammar so it accepts the same indicator specs.
        let CalcTokens {
            paths,
            interval: interval_token,
            specs,
            format,
            use_rth,
            fill,
            start: range_start,
            end: range_end,
        } = classify_calc(&tokens)?;
        // `plot` renders an image; a table/csv/fwob format token is meaningless here.
        if format != AnalysisFormat::default() {
            bail!("plot does not take an output format token; use --output to write a file");
        }
        let use_rth = self.use_rth || use_rth;
        let interval = resolve_interval(interval_token, acfg.bars.interval.as_deref())?;
        let fill = fill || acfg.bars.fill;
        let session = resolve_session(&acfg, use_rth, self.session.as_deref(), self.tz.as_deref())?;
        warn_uneven_interval(interval, &session, use_rth);
        let clock = BarClock::Session(session.clone());
        let start = self.start.clone().or(range_start);
        let end = self.end.clone().or(range_end);
        let (start, end) = parse_bounds(start.as_deref(), end.as_deref(), &session.time_zone())?;
        let files = resolve_files(&paths, &acfg)?;
        let filter = use_rth.then(|| session.clone());

        // Accept both tick files and pre-aggregated bar files: both stream through the resampler at
        // the requested interval, so a bar file honors the interval (e.g. plotting a 1s bar file at
        // 1m aggregates it into 1m candles instead of drawing degenerate one-second dots). Group the
        // resulting bars by symbol.
        let query = TickQuery {
            start,
            end,
            session: filter.clone(),
        };
        let mut groups: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
        for path in &files {
            let result = (|| -> Result<(String, Vec<Bar>)> {
                let symbol = file_symbol(path)?;
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
            })();
            match result {
                Ok((symbol, mut bars)) => groups.entry(symbol).or_default().append(&mut bars),
                Err(error) => {
                    tracing::error!(path = %path.display(), error = %format!("{error:#}"), "failed to read")
                }
            }
        }
        if groups.is_empty() {
            bail!("no plottable inputs resolved");
        }
        if self.output.is_some() && groups.len() > 1 {
            bail!(
                "plot --output writes a single chart, but {} symbols resolved; narrow the selection",
                groups.len()
            );
        }

        let stdout = std::io::stdout();
        for (symbol, mut bars) in groups {
            // A symbol may span several files; keep the merged bars ascending for rendering and
            // for the indicator computations that assume ordered input.
            bars.sort_by_key(|bar| bar.time);
            if bars.is_empty() {
                tracing::warn!(symbol = %symbol, "no bars in range; nothing to plot");
                continue;
            }

            // Compute each indicator spec against this symbol's bars and route it by kind: sma/ema
            // overlay the price panel, vsma/vema overlay the volume panel, and the rest (rsi/ret/vol)
            // get their own stacked panel.
            let mut overlays: Vec<Series> = Vec::new();
            let mut panels: Vec<Series> = Vec::new();
            let mut volume_overlays: Vec<Series> = Vec::new();
            for spec in &specs {
                let indicator = parse_spec(spec).expect("spec token validated during parsing")?;
                let series = Series {
                    label: indicator.name(),
                    values: indicator.compute(&bars),
                };
                match spec.split(':').next() {
                    Some("sma") | Some("ema") | Some("dema") => overlays.push(series),
                    Some("vsma") | Some("vema") | Some("vdema") => volume_overlays.push(series),
                    _ => panels.push(series),
                }
            }
            // A volume MA implies the volume panel.
            let volume = volume || !volume_overlays.is_empty();
            // With no overlays/panels at all, keep the previous default: a single SMA(20) overlay.
            if overlays.is_empty() && panels.is_empty() && volume_overlays.is_empty() {
                let sma = parse_spec("sma:20")
                    .expect("valid spec")
                    .expect("valid period");
                overlays.push(Series {
                    label: sma.name(),
                    values: sma.compute(&bars),
                });
            }

            let mut label_parts: Vec<String> = overlays
                .iter()
                .chain(panels.iter())
                .chain(volume_overlays.iter())
                .map(|s| s.label.clone())
                .collect();
            if volume && volume_overlays.is_empty() {
                label_parts.push("vol".to_string());
            }
            let title = format!("{symbol}  {}   {}", interval.label(), label_parts.join(" "));

            let opts = PlotOptions {
                width: self.width,
                height: self.height,
                title,
                // Label the axis in the same session tz the bars were anchored to.
                tz: session.time_zone(),
                overlays,
                panels,
                volume,
                volume_overlays,
            };
            let canvas = render(&bars, &opts);
            match &self.output {
                Some(path) => {
                    write_chart_file(&canvas, path)?;
                    tracing::info!(path = %path.display(), candles = bars.len(), "wrote chart");
                }
                None => {
                    let mut lock = stdout.lock();
                    lock.write_all(canvas.to_sixel().as_bytes())?;
                    lock.flush()?;
                }
            }
        }
        Ok(())
    }
}

/// Writes a rendered [`Canvas`] to `path`, choosing the encoding from the extension: `.six`/`.sixel`
/// dumps the raw Sixel escape sequence, anything else (including `.png`) writes an indexed PNG.
fn write_chart_file(canvas: &Canvas, path: &Path) -> Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("six") | Some("sixel") => std::fs::write(path, canvas.to_sixel())
            .with_context(|| format!("failed to write {}", path.display())),
        _ => canvas.write_png(path),
    }
}

/// Streams a symbol's bars to `sink` as each bucket closes, feeding all of `paths` through one
/// resampler (so multiple files of the same symbol form a single ascending stream) and applying
/// forward-fill when requested.
///
/// Accepts both tick files (resampled from ticks) and pre-aggregated bar files (re-resampled from
/// bars to the requested `interval`, e.g. 1s→1m), so every downstream command honors the interval
/// regardless of input format. A symbol's files must all be the same kind. Ticks are read in bulk
/// chunks and never fully materialized; bar files honor the `query` window by bar time.
fn stream_bars(
    paths: &[PathBuf],
    interval: Interval,
    clock: &BarClock,
    query: &TickQuery,
    fill: bool,
    sink: impl FnMut(Bar) -> Result<()>,
) -> Result<()> {
    let mut kind: Option<InputKind> = None;
    for path in paths {
        let this = input_kind(path)?;
        match kind {
            Some(existing) if existing != this => {
                bail!(
                    "cannot mix tick and bar files for one symbol ({})",
                    path.display()
                )
            }
            _ => kind = Some(this),
        }
    }

    let mut filler = ForwardFiller::new(interval, clock.clone(), fill, sink);
    match kind {
        Some(InputKind::Bar) => {
            let mut resampler = BarResampler::new(interval, clock.clone());
            for path in paths {
                // Seek to the window instead of reading the whole bar file, so a narrow time range
                // is proportionally fast.
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
#[command(after_help = concat!("Tokens (case-sensitive, any order):
  paths/symbols: a tick or bar FILE.fwob, a DIR, or a bare SYMBOL (resolved under output_dir)
  indicator specs: sma:N ema:N dema:N vsma:N vema:N vdema:N rsi:N ret:log ret:simple vol:N
  interval: e.g. 5m, 1h, 1d (default 1d; tick and bar inputs both resample/re-resample to it)
  formats: table (default), csv, md, jsonl, raw, hex, fwob
  session: rth (keep only regular-trading-hours ticks)
  fill: forward-fill empty intervals within a session
  time range: START..END (either side optional), e.g. 2024-01-01..2026-01-01 or ..2026-01-01
              bare dates/times use the exchange tz; add Z or +/-HH for an absolute instant",
    indicator_guide!()))]
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
    /// Print a per-column summary footer as colored TOML: a `[summary.price]` block (drawdown,
    /// CAGR), `n/mean/min/max/last` per indicator, and — for a `ret:log`/`ret:simple` column — a
    /// fitted-normal block (mean, stdev, skew, excess kurtosis, quartiles, Jarque-Bera, annualized
    /// return/vol, Sharpe) plus a `[.character]` read (trend/volatility/tails/regime/...).
    #[arg(long)]
    summary: bool,
    /// Override the summary's annualization factor (periods per year). Default: derived from the
    /// data's own bar frequency (returns / calendar-years), so daily/weekly/intraday all annualize
    /// correctly without assuming 252.
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
        // Like `bars`/`plot`, default to 1d when neither a token nor a config value is given; tick
        // and bar inputs alike resample/re-resample to it.
        let interval = resolve_interval(interval_token, acfg.calc.interval.as_deref())?;
        let fill = fill || acfg.calc.fill;
        let session = resolve_session(&acfg, use_rth, self.session.as_deref(), self.tz.as_deref())?;
        warn_uneven_interval(interval, &session, use_rth);
        let clock = BarClock::Session(session.clone());
        let filter = use_rth.then(|| session.clone());
        let start = self.start.clone().or(range_start);
        let end = self.end.clone().or(range_end);
        let (start, end) = parse_bounds(start.as_deref(), end.as_deref(), &session.time_zone())?;
        let files = resolve_files(&paths, &acfg)?;
        let query = TickQuery {
            start,
            end,
            session: filter.clone(),
        };

        // Group input files by the symbol they report (header only), preserving discovery order so a
        // symbol's files feed one resampler as a single ascending stream (mirrors `bars`). Each
        // symbol is then processed and emitted on its own, so rows appear as their bars close and
        // only one symbol's state is live at a time.
        let mut by_symbol: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for path in &files {
            match file_symbol(path) {
                Ok(symbol) => by_symbol.entry(symbol).or_default().push(path.clone()),
                Err(error) => {
                    tracing::error!(path = %path.display(), error = %format!("{error:#}"), "failed to read")
                }
            }
        }

        // Column names and precisions are identical across symbols (same specs), so derive the
        // shared schema once; the stateful streaming indicators are re-created per symbol.
        let meta = build_streaming_indicators(&spec_tokens)?;
        let names: Vec<String> = meta.iter().map(|i| i.name()).collect();
        let decimals: Vec<u8> = meta.iter().map(|i| i.decimals()).collect();
        drop(meta);

        let out_dir = self.output.clone().or_else(|| acfg.output_dir.clone());
        match format {
            AnalysisFormat::Fwob => {
                let dir = out_dir
                    .as_deref()
                    .context("calc --format fwob requires --output DIR")?;
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("failed to create {}", dir.display()))?;
                for (symbol, paths) in &by_symbol {
                    let mut indicators = build_streaming_indicators(&spec_tokens)?;
                    let path = dir.join(format!("{symbol}.fwob"));
                    let mut writer =
                        FrameWriter::create(&path, calc_schema(&names, &decimals)?, symbol)?;
                    stream_bars(paths, interval, &clock, &query, fill, |bar| {
                        let values: Vec<Option<f64>> =
                            indicators.iter_mut().map(|i| i.update(&bar)).collect();
                        writer.push(|buf| {
                            encode_calc_row(bar.time, bar.close, &values, &decimals, buf)
                        })
                    })?;
                    writer.finish()?;
                }
            }
            AnalysisFormat::Frame(frame) => {
                let include_symbol = by_symbol.len() > 1;
                guard_symbol_count(include_symbol, by_symbol.len())?;
                let base = calc_schema(&names, &decimals)?;
                let schema = if include_symbol {
                    with_symbol_column(&base)
                } else {
                    base
                };
                let symbols: Vec<String> = by_symbol.keys().cloned().collect();
                let strings: &[String] = if include_symbol { &symbols } else { &[] };
                let stdout = std::io::stdout();
                let mut out = std::io::BufWriter::new(stdout.lock());
                // On an interactive terminal flush each row so it appears the moment its bar closes;
                // when redirected, stay buffered for throughput.
                let autoflush = std::io::stdout().is_terminal();
                // The summary footer is a colored-TOML block (table/markdown only); color it only on
                // a terminal, honoring NO_COLOR — the same rule `fwob inspect` uses.
                let show_summary =
                    self.summary && matches!(frame, FrameFormat::Table | FrameFormat::Markdown);
                let color =
                    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
                let summary_columns: Vec<SummaryColumn> = names
                    .iter()
                    .zip(&decimals)
                    .map(|(name, decimals)| SummaryColumn {
                        name: name.clone(),
                        decimals: *decimals,
                    })
                    .collect();
                let mut collectors: Vec<(String, SummaryCollector)> = Vec::new();
                {
                    let mut stream =
                        FrameStream::new(&schema, strings, frame, autoflush, &mut out)?;
                    for (index, (symbol, paths)) in by_symbol.iter().enumerate() {
                        let mut indicators = build_streaming_indicators(&spec_tokens)?;
                        let mut collector =
                            show_summary.then(|| SummaryCollector::new(&summary_columns));
                        stream_bars(paths, interval, &clock, &query, fill, |bar| {
                            let values: Vec<Option<f64>> =
                                indicators.iter_mut().map(|i| i.update(&bar)).collect();
                            stream.emit(index, |buf| {
                                encode_calc_row(bar.time, bar.close, &values, &decimals, buf)
                            })?;
                            if let Some(collector) = collector.as_mut() {
                                collector.push_row(bar.time, bar.close, &values);
                            }
                            Ok(())
                        })?;
                        if let Some(collector) = collector {
                            collectors.push((symbol.clone(), collector));
                        }
                    }
                }
                // Colored-TOML summary footers after all rows, one `[summary]` block per symbol.
                for (symbol, collector) in &collectors {
                    let base = if include_symbol {
                        format!("{symbol}.summary")
                    } else {
                        "summary".to_string()
                    };
                    writeln!(out)?;
                    collector.render(&mut out, color, &base, self.periods_per_year)?;
                }
                out.flush()?;
            }
        }
        Ok(())
    }
}

/// Builds a fresh set of stateful [`StreamingIndicator`]s from validated spec tokens.
fn build_streaming_indicators(specs: &[String]) -> Result<Vec<Box<dyn StreamingIndicator>>> {
    specs
        .iter()
        .map(|spec| parse_streaming_spec(spec).expect("spec token validated during parsing"))
        .collect()
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

/// The positional `volume` token toggles the volume panel (`plot`).
const VOLUME_TOKEN: &str = "volume";

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
    // A bare integer (optionally comma-grouped, e.g. `1,778,078,433`) is a raw Unix epoch second /
    // FWOB key — an exact instant, so an end value is not expanded like a bare date.
    if let Some(epoch) = parse_epoch_seconds(value) {
        return to_u32(epoch);
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
    bail!(
        "invalid date/time {value:?} (use YYYY-MM-DD, a local date-time, an RFC3339 instant, or a \
         Unix epoch second like 1778078433)"
    )
}

/// True when `value` parses as some date/datetime/instant or a bare epoch second — used to tell a
/// `START..END` range token apart from a path like `..\AAPL.fwob`. Timezone-free; the actual
/// conversion is deferred.
fn looks_like_bound(value: &str) -> bool {
    parse_epoch_seconds(value).is_some()
        || value.parse::<jiff::Timestamp>().is_ok()
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
    fn bare_epoch_bounds_accept_optional_commas() {
        let tz = ny();
        let plain = parse_time_bound("1778078433", false, &tz).unwrap();
        assert_eq!(plain, 1_778_078_433);
        // Thousands separators are accepted and yield the same exact key.
        assert_eq!(
            parse_time_bound("1,778,078,433", false, &tz).unwrap(),
            plain
        );
        assert_eq!(plain, epoch("2026-05-06T14:40:33Z"));
        // An epoch is exact: the `is_end` flag does not expand it like a bare date.
        assert_eq!(parse_time_bound("1778078433", true, &tz).unwrap(), plain);
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
        // Epoch-second ranges, with and without thousands separators.
        assert_eq!(
            parse_range_token("1778078433..1778086276"),
            Some((Some("1778078433".to_owned()), Some("1778086276".to_owned())))
        );
        assert_eq!(
            parse_range_token("1,778,078,433..1,778,086,276"),
            Some((
                Some("1,778,078,433".to_owned()),
                Some("1,778,086,276".to_owned())
            ))
        );
        assert_eq!(
            parse_range_token("1778078433.."),
            Some((Some("1778078433".to_owned()), None))
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
    fn commit_interval_is_accepted_as_an_ad_hoc_override() {
        for (value, expected) in [("-1", -1), ("0", 0), ("120", 120)] {
            let cli = Cli::try_parse_from([
                "mdfwob",
                "download",
                "SPCX",
                "--commit-interval-seconds",
                value,
            ])
            .unwrap();
            let Command::Download(args) = cli.command else {
                panic!("expected download command");
            };
            assert_eq!(args.commit_interval_seconds, Some(expected));
        }
    }

    #[test]
    fn plot_defaults_to_console_hd_and_accepts_overrides() {
        // Defaults: 1920x1080, no volume, no output file (console Sixel).
        let cli = Cli::try_parse_from(["mdfwob", "plot", "AAPL"]).unwrap();
        let Command::Plot(args) = cli.command else {
            panic!("expected plot command");
        };
        assert_eq!(args.width, 1920);
        assert_eq!(args.height, 1080);
        assert!(!args.volume);
        assert!(args.output.is_none());
        assert_eq!(args.items, ["AAPL"]);

        // An explicit file, dimensions, indicator specs, and volume flag override the defaults.
        let cli = Cli::try_parse_from([
            "mdfwob",
            "plot",
            "AAPL",
            "1d",
            "sma:50",
            "rsi:14",
            "-o",
            "chart.png",
            "--width",
            "3840",
            "--height",
            "2160",
            "--volume",
        ])
        .unwrap();
        let Command::Plot(args) = cli.command else {
            panic!("expected plot command");
        };
        assert_eq!(args.width, 3840);
        assert_eq!(args.height, 2160);
        assert!(args.volume);
        assert_eq!(args.output.as_deref(), Some(Path::new("chart.png")));
        assert_eq!(args.items, ["AAPL", "1d", "sma:50", "rsi:14"]);
    }

    #[test]
    fn plot_accepts_volume_as_a_positional_token() {
        let cli = Cli::try_parse_from(["mdfwob", "plot", "AAPL", "volume"]).unwrap();
        let Command::Plot(args) = cli.command else {
            panic!("expected plot command");
        };
        // The token stays in `items`; run() strips it. The flag itself defaults off here.
        assert!(!args.volume);
        assert_eq!(args.items, ["AAPL", "volume"]);
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
            let cli = Cli::try_parse_from([
                "mdfwob",
                "download",
                "SPCX",
                "--stall-timeout-seconds",
                value,
            ])
            .unwrap();
            let Command::Download(args) = cli.command else {
                panic!("expected download command");
            };
            assert_eq!(args.stall_timeout_seconds, Some(expected));
        }
    }
}
