//! Market-data analysis engine.
//!
//! Reads tick FWOB files, resamples them into OHLCV bars, computes per-bar
//! indicator series (including user-supplied custom functions), and renders
//! summaries. Exposed as a library API in addition to the `mdfwob` CLI
//! subcommands `stat`, `bars`, and `calc`.

pub mod adjust;
pub mod calc;
pub mod config;
pub mod feed;
pub mod inspect;
pub mod interval;
pub mod ls;
pub mod model;
pub mod output;
pub mod plot;
pub mod read;
pub mod resample;
pub mod schema;
pub mod session;
pub mod sidecar;
pub mod stat;
pub mod summary;

pub use adjust::{
    ActionKind, ActionSpec, ActionTable, Adjuster, AdjustmentMode, CorporateAction, adjust_bars,
    detect_splits, load_actions,
};
pub use calc::{
    Calc, CalcColumn, CalcOutput, CalcSummary, Dema, Ema, Indicator, Returns, Rsi, Sma,
    StreamingIndicator, Volatility, VolumeDema, VolumeEma, VolumeSma, parse_spec,
    parse_streaming_spec, summarize,
};
pub use config::{AnalysisConfig, ReturnMethod};
pub use feed::{BarStream, SymbolBars, resolve_sidecar, stream_symbol_bars, symbol_bars};
pub use interval::Interval;
pub use ls::{LsFormat, LsRow, ls_file, write_ls};
pub use model::{Bar, Tick};
pub use plot::{Canvas, PlotOptions, render};
pub use read::{
    TickQuery, discover_inputs, file_symbol, open_tick_reader, read_bars, read_ticks,
    stream_bars_file, stream_ticks, tick_symbol,
};
pub use resample::{BarClock, ForwardFiller, Resampler, resample};
pub use session::Session;
pub use sidecar::{RefreshOutcome, RefreshSpec, refresh_sidecar, sidecar_name, sidecar_path};
pub use stat::{StatAccumulator, StatRow, compute_stat, stat_file, stat_file_adjusted};
pub use summary::{SummaryCollector, SummaryColumn};
