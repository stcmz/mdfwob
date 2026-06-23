//! Market-data analysis engine.
//!
//! Reads tick FWOB files, resamples them into OHLCV bars, computes per-bar
//! indicator series (including user-supplied custom functions), and renders
//! summaries. Exposed as a library API in addition to the `mdfwob` CLI
//! subcommands `stat`, `bars`, and `calc`.

pub mod calc;
pub mod config;
pub mod interval;
pub mod model;
pub mod output;
pub mod read;
pub mod resample;
pub mod schema;
pub mod session;
pub mod stat;

pub use calc::{
    Calc, CalcColumn, CalcOutput, CalcSummary, Ema, Indicator, Returns, Rsi, Sma, Volatility,
    parse_spec, summarize,
};
pub use config::{AnalysisConfig, ReturnMethod};
pub use interval::Interval;
pub use model::{Bar, Tick};
pub use read::{
    TickQuery, discover_inputs, open_tick_reader, read_bars, read_ticks, stream_ticks, tick_symbol,
};
pub use resample::{BarClock, ForwardFiller, Resampler, resample};
pub use session::Session;
pub use stat::{StatAccumulator, StatRow, compute_stat};
