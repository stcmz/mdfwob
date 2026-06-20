use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::{
    config::{Config, StockContractConfig},
    downloader::{DownloadPlan, Downloader},
    fwob_options::{parse_tokens, validate_zstd_level},
};

#[derive(Debug, Parser)]
#[command(name = "mdfwob")]
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
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Download configured or ad hoc historical market data.
    Download(DownloadArgs),
    /// Fully verify a FWOB v1 or v2 output file.
    Verify(VerifyArgs),
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

    /// Default primary exchange for ad hoc stock symbols.
    #[arg(long)]
    primary_exchange: Option<String>,

    /// Start date/time. Accepts YYYY-MM-DD or RFC3339.
    #[arg(long)]
    start: Option<String>,

    /// End date/time. Accepts YYYY-MM-DD or RFC3339. Defaults to now.
    #[arg(long)]
    end: Option<String>,

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
}
