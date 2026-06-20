use std::{
    error::Error as StdError,
    fmt,
    sync::{Arc, Mutex, RwLock},
};

use anyhow::{Context, Result};
use ibapi::subscriptions::SubscriptionItem;
use time::OffsetDateTime;
use tracing::warn;

use super::{MarketDataProvider, RecoveryAction};
use crate::{
    config::IbkrConfig,
    downloader::{ProviderTick, StockContract, ibkr_contract, trading_hours},
};

type Client = ibapi::client::blocking::Client;

struct Versioned<T> {
    value: Arc<T>,
    generation: u64,
}

struct ClientSlot<T> {
    state: RwLock<Versioned<T>>,
    reconnect_gate: Mutex<()>,
}

impl<T> ClientSlot<T> {
    fn new(value: T) -> Self {
        Self {
            state: RwLock::new(Versioned {
                value: Arc::new(value),
                generation: 0,
            }),
            reconnect_gate: Mutex::new(()),
        }
    }

    fn snapshot(&self) -> (Arc<T>, u64) {
        let state = self.state.read().expect("IBKR client slot poisoned");
        (Arc::clone(&state.value), state.generation)
    }

    fn replace_if_generation(
        &self,
        observed_generation: u64,
        connect: impl FnOnce() -> Result<T>,
    ) -> Result<()> {
        let _gate = self
            .reconnect_gate
            .lock()
            .expect("IBKR reconnect gate poisoned");
        if self
            .state
            .read()
            .expect("IBKR client slot poisoned")
            .generation
            != observed_generation
        {
            return Ok(());
        }

        let replacement = connect()?;
        let mut state = self.state.write().expect("IBKR client slot poisoned");
        if state.generation == observed_generation {
            state.value = Arc::new(replacement);
            state.generation = state.generation.wrapping_add(1);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct IbkrRequestError {
    source: ibapi::Error,
    generation: u64,
}

impl fmt::Display for IbkrRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.source)
    }
}

impl StdError for IbkrRequestError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&self.source)
    }
}

pub struct IbkrProvider {
    config: IbkrConfig,
    clients: ClientSlot<Client>,
}

impl IbkrProvider {
    pub fn connect(config: &IbkrConfig) -> Result<Self> {
        let client = connect_client(config)?;
        Ok(Self {
            config: config.clone(),
            clients: ClientSlot::new(client),
        })
    }

    fn request_error(error: ibapi::Error, generation: u64) -> anyhow::Error {
        IbkrRequestError {
            source: error,
            generation,
        }
        .into()
    }
}

fn connect_client(config: &IbkrConfig) -> Result<Client> {
    let connection_url = format!("{}:{}", config.host, config.port);
    Client::connect(&connection_url, config.client_id)
        .with_context(|| format!("failed to connect to IBKR at {connection_url}"))
}

impl MarketDataProvider for IbkrProvider {
    fn head_timestamp(&self, contract: &StockContract, use_rth: bool) -> Result<OffsetDateTime> {
        use ibapi::market_data::historical::WhatToShow;

        let (client, generation) = self.clients.snapshot();
        client
            .head_timestamp(
                &ibkr_contract(contract),
                WhatToShow::Trades,
                trading_hours(use_rth),
            )
            .map_err(|error| Self::request_error(error, generation))
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
        let (client, generation) = self.clients.snapshot();
        let ib_contract = ibkr_contract(contract);
        let subscription = client
            .historical_ticks(&ib_contract, max_ticks)
            .starting(start)
            .trading_hours(trading_hours(use_rth))
            .trade()
            .map_err(|error| Self::request_error(error, generation))
            .with_context(|| format!("historical tick request failed for {}", contract.symbol))?;

        let mut out = Vec::new();
        for item in subscription {
            match item.map_err(|error| Self::request_error(error, generation))? {
                SubscriptionItem::Data(tick) => out.push(ProviderTick {
                    timestamp: tick.timestamp,
                    price: tick.price,
                    size: tick.size,
                }),
                SubscriptionItem::Notice(notice) => {
                    warn!(symbol = %contract.symbol, %notice, "IBKR notice during historical tick request");
                }
            }
        }
        Ok(out)
    }

    fn recovery_action(&self, error: &anyhow::Error) -> Option<RecoveryAction> {
        let request = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<IbkrRequestError>())?;
        match request.source {
            ibapi::Error::ConnectionReset => Some(RecoveryAction::Retry),
            ibapi::Error::ConnectionFailed | ibapi::Error::Shutdown => {
                Some(RecoveryAction::Reconnect {
                    generation: request.generation,
                })
            }
            _ => None,
        }
    }

    fn reconnect(&self, generation: u64) -> Result<()> {
        self.clients
            .replace_if_generation(generation, || connect_client(&self.config))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    #[test]
    fn concurrent_reconnect_replaces_one_generation_once() {
        let slot = Arc::new(ClientSlot::new(0usize));
        let connects = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|scope| {
            for _ in 0..8 {
                let slot = Arc::clone(&slot);
                let connects = Arc::clone(&connects);
                scope.spawn(move || {
                    slot.replace_if_generation(0, || {
                        connects.fetch_add(1, Ordering::SeqCst);
                        Ok(1)
                    })
                    .unwrap();
                });
            }
        });

        let (client, generation) = slot.snapshot();
        assert_eq!(*client, 1);
        assert_eq!(generation, 1);
        assert_eq!(connects.load(Ordering::SeqCst), 1);
    }
}
