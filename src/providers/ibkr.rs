use anyhow::{Context, Result};
use time::OffsetDateTime;

use super::MarketDataProvider;
use crate::{
    config::IbkrConfig,
    downloader::{ProviderTick, StockContract, ibkr_contract, trading_hours},
};

pub struct IbkrProvider {
    client: ibapi::client::blocking::Client,
}

impl IbkrProvider {
    pub fn connect(config: &IbkrConfig) -> Result<Self> {
        let connection_url = format!("{}:{}", config.host, config.port);
        let client = ibapi::client::blocking::Client::connect(&connection_url, config.client_id)
            .with_context(|| format!("failed to connect to IBKR at {connection_url}"))?;
        Ok(Self { client })
    }
}

impl MarketDataProvider for IbkrProvider {
    fn head_timestamp(&self, contract: &StockContract, use_rth: bool) -> Result<OffsetDateTime> {
        use ibapi::market_data::historical::WhatToShow;

        self.client
            .head_timestamp(
                &ibkr_contract(contract),
                WhatToShow::Trades,
                trading_hours(use_rth),
            )
            .context("head timestamp request failed")
    }

    fn historical_trade_ticks(
        &self,
        contract: &StockContract,
        start: OffsetDateTime,
        _end: OffsetDateTime,
        max_ticks: i32,
        use_rth: bool,
    ) -> Result<Vec<ProviderTick>> {
        let ib_contract = ibkr_contract(contract);
        let subscription = self
            .client
            .historical_ticks(&ib_contract, max_ticks)
            .starting(start)
            .trading_hours(trading_hours(use_rth))
            .trade()
            .with_context(|| format!("historical tick request failed for {}", contract.symbol))?;

        let mut out = Vec::new();
        for tick in subscription {
            out.push(ProviderTick {
                timestamp: tick.timestamp,
                price: tick.price,
                size: tick.size,
            });
        }
        Ok(out)
    }
}
