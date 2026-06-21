//! mdfwob library surface.
//!
//! In addition to the `mdfwob` command-line tool, the market-data **analysis**
//! engine is exposed here as a public API so programs can compute summaries,
//! resample ticks into bars, and run indicator pipelines (including custom
//! user-supplied functions) directly. See [`analysis`].

pub mod analysis;
pub mod cli;
pub mod config;
pub mod fwob_options;
pub mod tick;

mod downloader;
mod providers;
mod storage;
