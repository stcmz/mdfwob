use anyhow::Result;
use clap::Parser;
use tracing::error;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::{EnvFilter, fmt};

/// Formats log timestamps in the machine's local time zone (with UTC offset), e.g.
/// `2026-07-05T09:12:44.217-04:00`, instead of tracing's default UTC. Uses jiff so the local zone
/// and DST are resolved from the system tz database without the `time` crate's multi-thread
/// local-offset caveat.
struct LocalTimer;

impl FormatTime for LocalTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(
            w,
            "{}",
            jiff::Zoned::now().strftime("%Y-%m-%dT%H:%M:%S%.3f%:z")
        )
    }
}

fn main() -> Result<()> {
    fmt()
        .with_timer(LocalTimer)
        .with_env_filter(EnvFilter::from_default_env().add_directive("mdfwob=info".parse()?))
        .init();

    if let Err(error) = mdfwob::cli::Cli::parse().run() {
        // Log the full error chain at error level, then exit non-zero without letting `main`
        // print the error a second time via the default `Termination` impl.
        error!(error = format!("{error:#}"), "mdfwob exited with an error");
        std::process::exit(1);
    }
    Ok(())
}
