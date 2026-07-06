//! mdfwob-core — the market-data analysis engine.
//!
//! Reads tick / bar FWOB files, resamples them into OHLCV bars, computes per-bar
//! indicator series (including user-supplied custom functions), summarizes, and
//! renders charts. Shared by the `mdfwob` CLI (download + analysis) and by
//! downstream tools such as backtesting engines. Carries no network/download
//! dependencies.

pub mod analysis;
pub mod tick;

/// Normalizes a symbol for case-insensitive matching (trim + uppercase).
pub fn normalize_symbol(symbol: &str) -> String {
    symbol.trim().to_ascii_uppercase()
}
