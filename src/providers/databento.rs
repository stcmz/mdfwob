use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use databento::{
    HistoricalClient,
    dbn::{FIXED_PRICE_SCALE, SType, Schema, TradeMsg},
    historical::timeseries::GetRangeParams,
};
use time::OffsetDateTime;
use tokio::runtime::Runtime;

use super::MarketDataProvider;
use crate::{
    config::DatabentoConfig,
    downloader::{ProviderTick, StockContract},
};

pub struct DatabentoProvider {
    client: Mutex<HistoricalClient>,
    runtime: Runtime,
    stock_dataset: String,
    option_dataset: String,
}

impl DatabentoProvider {
    pub fn connect(config: &DatabentoConfig) -> Result<Self> {
        let api_key = std::env::var(&config.api_key_env).with_context(|| {
            format!(
                "Databento API key environment variable {} is not set",
                config.api_key_env
            )
        })?;
        let client = HistoricalClient::builder().key(api_key)?.build()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to create Databento runtime")?;
        Ok(Self {
            client: Mutex::new(client),
            runtime,
            stock_dataset: config.stock_dataset.clone(),
            option_dataset: config.option_dataset.clone(),
        })
    }

    fn symbol_and_dataset<'a>(&'a self, contract: &'a StockContract) -> Result<(&'a str, &'a str)> {
        match &contract.option {
            Some(option) => {
                let symbol = option.local_symbol.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Databento option {} requires local_symbol in [[options]]",
                        contract.symbol
                    )
                })?;
                Ok((symbol, &self.option_dataset))
            }
            None => Ok((&contract.symbol, &self.stock_dataset)),
        }
    }
}

impl MarketDataProvider for DatabentoProvider {
    fn head_timestamp(&self, _contract: &StockContract, _use_rth: bool) -> Result<OffsetDateTime> {
        bail!("download.start or --start is required for provider databento")
    }

    fn historical_trade_ticks(
        &self,
        contract: &StockContract,
        start: OffsetDateTime,
        end: OffsetDateTime,
        _max_ticks: i32,
        use_rth: bool,
    ) -> Result<Vec<ProviderTick>> {
        if use_rth {
            bail!("provider databento does not yet support regular-hours filtering");
        }
        let (symbol, dataset) = self.symbol_and_dataset(contract)?;
        let request_end = end.min(start + time::Duration::days(1));
        let mut client = self.client.lock().expect("Databento client poisoned");

        self.runtime.block_on(async {
            let mut decoder = client
                .timeseries()
                .get_range(
                    &GetRangeParams::builder()
                        .dataset(dataset)
                        .date_time_range(start..request_end)
                        .symbols(symbol)
                        .stype_in(SType::RawSymbol)
                        .schema(Schema::Trades)
                        .build(),
                )
                .await
                .with_context(|| {
                    format!(
                        "Databento historical trade request failed for {}",
                        contract.symbol
                    )
                })?;

            let mut out = Vec::new();
            while let Some(trade) = decoder.decode_record::<TradeMsg>().await? {
                out.push(databento_trade_to_provider(trade, &contract.symbol)?);
            }
            Ok(out)
        })
    }
}

fn databento_trade_to_provider(trade: &TradeMsg, symbol: &str) -> Result<ProviderTick> {
    let timestamp = OffsetDateTime::from_unix_timestamp_nanos(trade.hd.ts_event as i128)?;
    let size = i32::try_from(trade.size)
        .with_context(|| format!("Databento trade size exceeds i32 for {symbol}"))?;
    Ok(ProviderTick {
        timestamp,
        price: trade.price as f64 / FIXED_PRICE_SCALE as f64,
        size,
    })
}

#[cfg(test)]
mod tests {
    use databento::dbn::{RecordHeader, rtype};

    use super::*;

    #[test]
    fn converts_databento_trade_to_utc_provider_tick() {
        let timestamp = OffsetDateTime::from_unix_timestamp(1_717_421_401).unwrap();
        let trade = TradeMsg {
            hd: RecordHeader::new::<TradeMsg>(
                rtype::MBP_0,
                1,
                42,
                timestamp.unix_timestamp_nanos() as u64,
            ),
            price: 123_456_700_000,
            size: 250,
            ..TradeMsg::default()
        };

        let converted = databento_trade_to_provider(&trade, "AAPL").unwrap();
        assert_eq!(converted.timestamp, timestamp);
        assert_eq!(converted.price, 123.4567);
        assert_eq!(converted.size, 250);
    }
}
