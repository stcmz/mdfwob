use anyhow::Result;
use clap::Parser;
use tracing::error;
use tracing_subscriber::{EnvFilter, fmt};

fn main() -> Result<()> {
    fmt()
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
