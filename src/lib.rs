//! mdfwob library surface.
//!
//! The download/verify functionality is exposed as a library in addition to the
//! `mdfwob` command-line tool.

pub mod cli;
pub mod config;
pub mod fwob_options;
pub mod tick;

mod downloader;
mod providers;
mod storage;
