//! mdfwob library surface.
//!
//! The market-data **analysis** engine lives in the [`mdfwob_core`] crate and is
//! re-exported here (as [`analysis`] and [`tick`]) so existing `mdfwob::analysis::…`
//! paths keep working. This crate adds the download stack and the command-line tool.

pub use mdfwob_core::{analysis, tick};

pub mod cli;
pub mod config;
pub mod fwob_options;

mod downloader;
mod providers;
mod storage;
