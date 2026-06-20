use std::{collections::HashSet, fs, path::PathBuf};

use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub ibkr: IbkrConfig,
    pub databento: DatabentoConfig,
    pub download: DownloadConfig,
    pub stocks: Vec<StockContractConfig>,
    pub options: Vec<OptionContractConfig>,
}

impl Config {
    pub fn read(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let text = fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn filter_symbols(&mut self, symbols: &[String]) {
        let wanted: HashSet<String> = symbols.iter().map(|s| normalize_symbol(s)).collect();
        for group in &mut self.stocks {
            group
                .symbols
                .retain(|symbol| wanted.contains(&normalize_symbol(symbol)));
        }
        self.stocks.retain(|group| !group.symbols.is_empty());
        self.options
            .retain(|option| wanted.contains(&normalize_symbol(&option.symbol)));
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabentoConfig {
    pub api_key_env: String,
    pub stock_dataset: String,
    pub option_dataset: String,
}

impl Default for DatabentoConfig {
    fn default() -> Self {
        Self {
            api_key_env: "DATABENTO_API_KEY".into(),
            stock_dataset: "EQUS.MINI".into(),
            option_dataset: "OPRA.PILLAR".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IbkrConfig {
    pub host: String,
    pub port: u16,
    pub client_id: i32,
    pub reconnect_timeout_seconds: i64,
}

impl Default for IbkrConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 4002,
            client_id: 0,
            reconnect_timeout_seconds: -1,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DownloadConfig {
    pub provider: ProviderKind,
    pub output_dir: PathBuf,
    pub start: Option<String>,
    pub end: Option<String>,
    pub use_rth: bool,
    pub parallelism: usize,
    pub request_interval_ms: u64,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            provider: ProviderKind::Ibkr,
            output_dir: PathBuf::from("."),
            start: None,
            end: None,
            use_rth: false,
            parallelism: 4,
            request_interval_ms: 3_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Ibkr,
    Databento,
    Polygon,
    Thetadata,
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Ibkr => "ibkr",
            Self::Databento => "databento",
            Self::Polygon => "polygon",
            Self::Thetadata => "thetadata",
        };
        f.write_str(name)
    }
}

impl ProviderKind {
    pub fn from_token(value: &str) -> Option<Self> {
        match value {
            "ibkr" => Some(Self::Ibkr),
            "databento" => Some(Self::Databento),
            "polygon" => Some(Self::Polygon),
            "thetadata" => Some(Self::Thetadata),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StockContractConfig {
    pub symbols: Vec<String>,
    pub currency: String,
    pub exchange: String,
    pub primary_exchange: Option<String>,
}

impl Default for StockContractConfig {
    fn default() -> Self {
        Self {
            symbols: Vec::new(),
            currency: "USD".into(),
            exchange: "SMART".into(),
            primary_exchange: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum OptionRight {
    #[serde(rename = "call", alias = "CALL", alias = "Call", alias = "C")]
    Call,
    #[serde(rename = "put", alias = "PUT", alias = "Put", alias = "P")]
    Put,
}

impl OptionRight {
    pub fn code(self) -> &'static str {
        match self {
            Self::Call => "C",
            Self::Put => "P",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionContractConfig {
    pub symbol: String,
    pub expiration: String,
    pub strike: f64,
    pub right: OptionRight,
    #[serde(default = "default_currency")]
    pub currency: String,
    #[serde(default = "default_exchange")]
    pub exchange: String,
    #[serde(default = "default_option_multiplier")]
    pub multiplier: String,
    #[serde(default)]
    pub trading_class: Option<String>,
    #[serde(default)]
    pub local_symbol: Option<String>,
}

fn default_currency() -> String {
    "USD".into()
}

fn default_exchange() -> String {
    "SMART".into()
}

fn default_option_multiplier() -> String {
    "100".into()
}

pub fn normalize_symbol(symbol: &str) -> String {
    symbol.trim().to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_stays_valid() {
        let config: Config = toml::from_str(include_str!("../contracts.example.toml")).unwrap();

        assert_eq!(config.download.provider, ProviderKind::Ibkr);
        assert!(!config.stocks.is_empty());
        assert!(!config.options.is_empty());
    }

    #[test]
    fn download_defaults_use_three_second_request_interval() {
        let download = DownloadConfig::default();
        assert_eq!(download.provider, ProviderKind::Ibkr);
        assert_eq!(download.request_interval_ms, 3_000);
    }

    #[test]
    fn download_provider_is_deserialized() {
        let config: Config = toml::from_str(
            r#"
                [download]
                provider = "databento"
            "#,
        )
        .unwrap();

        assert_eq!(config.download.provider, ProviderKind::Databento);
    }

    #[test]
    fn ibkr_connection_settings_are_provider_scoped() {
        let config: Config = toml::from_str(
            r#"
                [ibkr]
                host = "gateway.internal"
                port = 7496
                client_id = 42
            "#,
        )
        .unwrap();

        assert_eq!(config.ibkr.host, "gateway.internal");
        assert_eq!(config.ibkr.port, 7496);
        assert_eq!(config.ibkr.client_id, 42);
        assert_eq!(config.ibkr.reconnect_timeout_seconds, -1);
    }

    #[test]
    fn legacy_connection_and_ibkr_only_download_fields_are_rejected() {
        assert!(
            toml::from_str::<Config>(
                r#"
                    [connection]
                    host = "127.0.0.1"
                "#,
            )
            .is_err()
        );
        assert!(
            toml::from_str::<Config>(
                r#"
                    [download]
                    what_to_show = "TRADES"
                "#,
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_config_fields_are_rejected() {
        let error = toml::from_str::<Config>(
            r#"
                [download]
                paralellism = 4
            "#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn databento_defaults_and_overrides_are_deserialized() {
        let defaults = Config::default().databento;
        assert_eq!(defaults.api_key_env, "DATABENTO_API_KEY");
        assert_eq!(defaults.stock_dataset, "EQUS.MINI");
        assert_eq!(defaults.option_dataset, "OPRA.PILLAR");

        let config: Config = toml::from_str(
            r#"
                [databento]
                api_key_env = "MY_DATABENTO_KEY"
                stock_dataset = "EQUS.ALL"
            "#,
        )
        .unwrap();
        assert_eq!(config.databento.api_key_env, "MY_DATABENTO_KEY");
        assert_eq!(config.databento.stock_dataset, "EQUS.ALL");
    }

    #[test]
    fn stock_collection_fields_are_deserialized() {
        let config: Config = toml::from_str(
            r#"
                [[stocks]]
                symbols = ["AAPL"]
                currency = "USD"
                exchange = "SMART"
                primary_exchange = "NASDAQ"
            "#,
        )
        .unwrap();

        let stock = &config.stocks[0];
        assert_eq!(stock.symbols, ["AAPL"]);
        assert_eq!(stock.currency, "USD");
        assert_eq!(stock.exchange, "SMART");
        assert_eq!(stock.primary_exchange.as_deref(), Some("NASDAQ"));
    }

    #[test]
    fn option_contract_fields_and_defaults_are_deserialized() {
        let config: Config = toml::from_str(
            r#"
                [[options]]
                symbol = "MSFT"
                expiration = "20260717"
                strike = 450
                right = "C"
                trading_class = "MSFT"
            "#,
        )
        .unwrap();

        let option = &config.options[0];
        assert_eq!(option.symbol, "MSFT");
        assert_eq!(option.expiration, "20260717");
        assert_eq!(option.strike, 450.0);
        assert_eq!(option.right, OptionRight::Call);
        assert_eq!(option.currency, "USD");
        assert_eq!(option.exchange, "SMART");
        assert_eq!(option.multiplier, "100");
        assert_eq!(option.trading_class.as_deref(), Some("MSFT"));
    }

    #[test]
    fn symbol_filter_includes_options() {
        let mut config: Config = toml::from_str(
            r#"
                [[stocks]]
                symbols = ["AAPL", "MSFT"]

                [[options]]
                symbol = "MSFT"
                expiration = "20260717"
                strike = 450
                right = "put"
            "#,
        )
        .unwrap();

        config.filter_symbols(&["msft".into()]);
        assert_eq!(config.stocks[0].symbols, ["MSFT"]);
        assert_eq!(config.options.len(), 1);
    }
}
