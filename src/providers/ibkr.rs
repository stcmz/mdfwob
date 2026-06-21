use std::{
    error::Error as StdError,
    fmt,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use ibapi::subscriptions::SubscriptionItem;
use time::OffsetDateTime;
use tracing::warn;

use super::{MarketDataProvider, RecoveryAction};
use crate::{
    config::IbkrConfig,
    downloader::{CancellationToken, ProviderTick, StockContract, ibkr_contract, trading_hours},
};

type Client = ibapi::client::blocking::Client;

/// How long each blocking wait inside a historical request lasts before we wake to re-check
/// cancellation and the connectivity epoch. Short enough that Ctrl+C is honored promptly; the
/// wait is asleep the whole time (no busy polling) and returns immediately when data arrives.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// True for IBKR "connectivity restored" system codes (1101 = restored, data lost; 1102 =
/// restored, data maintained). A request in flight across one of these was orphaned by TWS.
fn is_connectivity_restore(code: i32) -> bool {
    matches!(code, 1101 | 1102)
}

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

/// Why a historical-ticks request failed, paired with the client generation it ran against.
#[derive(Debug)]
enum IbkrFailure {
    /// An error surfaced by ibapi (e.g. a socket-level `ConnectionReset`).
    Ibapi(ibapi::Error),
    /// The request was in flight across a TWS<->IBKR connectivity blip and was silently
    /// dropped by TWS; it must be re-issued from the unchanged cursor.
    Orphaned,
    /// Cancellation was requested while the request was waiting.
    Canceled,
}

#[derive(Debug)]
struct IbkrRequestError {
    failure: IbkrFailure,
    generation: u64,
}

impl fmt::Display for IbkrRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.failure {
            IbkrFailure::Ibapi(source) => write!(formatter, "{source}"),
            IbkrFailure::Orphaned => {
                write!(formatter, "request orphaned by an IBKR connectivity loss")
            }
            IbkrFailure::Canceled => write!(formatter, "request canceled"),
        }
    }
}

impl StdError for IbkrRequestError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match &self.failure {
            IbkrFailure::Ibapi(source) => Some(source),
            _ => None,
        }
    }
}

/// Maps a request failure to the downloader's recovery action. Extracted as a free function so
/// it can be unit-tested without a live IBKR client.
fn classify_recovery(error: &anyhow::Error) -> Option<RecoveryAction> {
    let request = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<IbkrRequestError>())?;
    match &request.failure {
        IbkrFailure::Ibapi(ibapi::Error::ConnectionReset) | IbkrFailure::Orphaned => {
            Some(RecoveryAction::Retry)
        }
        IbkrFailure::Ibapi(ibapi::Error::ConnectionFailed | ibapi::Error::Shutdown) => {
            Some(RecoveryAction::Reconnect {
                generation: request.generation,
            })
        }
        _ => None,
    }
}

pub struct IbkrProvider {
    config: IbkrConfig,
    clients: ClientSlot<Client>,
    /// Bumped by the background connectivity watcher on every restore (1101/1102). A request
    /// records this at start; if it advances while the request waits, the request was orphaned.
    connectivity: Arc<AtomicU64>,
}

impl IbkrProvider {
    pub fn connect(config: &IbkrConfig) -> Result<Self> {
        let connectivity = Arc::new(AtomicU64::new(0));
        let client = connect_client(config, &connectivity)?;
        Ok(Self {
            config: config.clone(),
            clients: ClientSlot::new(client),
            connectivity,
        })
    }

    fn request_error(failure: IbkrFailure, generation: u64) -> anyhow::Error {
        IbkrRequestError {
            failure,
            generation,
        }
        .into()
    }
}

/// Connect a fresh client and spawn a background thread that watches the global notice stream,
/// bumping `connectivity` on each connectivity restore. The watcher exits on its own when the
/// client (and its notice broadcaster) is dropped — i.e. on provider drop or client replacement.
fn connect_client(config: &IbkrConfig, connectivity: &Arc<AtomicU64>) -> Result<Client> {
    let connection_url = format!("{}:{}", config.host, config.port);
    let (client, notices) = Client::builder()
        .address(connection_url.clone())
        .client_id(config.client_id)
        .connect_with_notice_stream()
        .with_context(|| format!("failed to connect to IBKR at {connection_url}"))?;

    let epoch = Arc::clone(connectivity);
    std::thread::Builder::new()
        .name("ibkr-connectivity-watch".to_owned())
        .spawn(move || {
            while let Some(notice) = notices.next() {
                if is_connectivity_restore(notice.code) {
                    epoch.fetch_add(1, Ordering::SeqCst);
                }
            }
        })
        .context("failed to spawn IBKR connectivity watcher")?;

    Ok(client)
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
            .map_err(|error| Self::request_error(IbkrFailure::Ibapi(error), generation))
            .context("head timestamp request failed")
    }

    fn historical_trade_ticks(
        &self,
        contract: &StockContract,
        start: OffsetDateTime,
        _end: OffsetDateTime,
        max_ticks: i32,
        use_rth: bool,
        cancel: &CancellationToken,
    ) -> Result<Vec<ProviderTick>> {
        let (client, generation) = self.clients.snapshot();
        let start_epoch = self.connectivity.load(Ordering::SeqCst);
        let ib_contract = ibkr_contract(contract);
        let subscription = client
            .historical_ticks(&ib_contract, max_ticks)
            .starting(start)
            .trading_hours(trading_hours(use_rth))
            .trade()
            .map_err(|error| Self::request_error(IbkrFailure::Ibapi(error), generation))
            .with_context(|| format!("historical tick request failed for {}", contract.symbol))?;

        let mut out = Vec::new();
        loop {
            let started = Instant::now();
            match subscription.next_timeout(POLL_INTERVAL) {
                Some(Ok(SubscriptionItem::Data(tick))) => out.push(ProviderTick {
                    timestamp: tick.timestamp,
                    price: tick.price,
                    size: tick.size,
                }),
                Some(Ok(SubscriptionItem::Notice(notice))) => {
                    warn!(symbol = %contract.symbol, %notice, "IBKR notice during historical tick request");
                }
                Some(Err(error)) => {
                    return Err(Self::request_error(IbkrFailure::Ibapi(error), generation));
                }
                None => {
                    // ibapi returns `None` immediately once the stream is done/ended (its
                    // `next_helper` short-circuits before blocking), so a fast `None` means the
                    // request completed; a `None` after the full interval means no message
                    // arrived this tick.
                    if started.elapsed() < POLL_INTERVAL / 2 {
                        break;
                    }
                    if cancel.is_canceled() {
                        return Err(Self::request_error(IbkrFailure::Canceled, generation));
                    }
                    if self.connectivity.load(Ordering::SeqCst) != start_epoch {
                        return Err(Self::request_error(IbkrFailure::Orphaned, generation));
                    }
                    // Otherwise the connection is healthy and the request is merely slow: keep
                    // waiting rather than re-issuing a still-valid request.
                }
            }
        }
        Ok(out)
    }

    fn recovery_action(&self, error: &anyhow::Error) -> Option<RecoveryAction> {
        classify_recovery(error)
    }

    fn reconnect(&self, generation: u64) -> Result<()> {
        self.clients.replace_if_generation(generation, || {
            connect_client(&self.config, &self.connectivity)
        })
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

    #[test]
    fn only_connectivity_restore_codes_bump_the_epoch() {
        assert!(is_connectivity_restore(1101));
        assert!(is_connectivity_restore(1102));
        // Loss (1100), socket reset (1300), and farm-status notices must not bump.
        assert!(!is_connectivity_restore(1100));
        assert!(!is_connectivity_restore(1300));
        assert!(!is_connectivity_restore(2104));
    }

    #[test]
    fn classifies_orphaned_reset_reconnect_and_canceled_failures() {
        let orphaned = IbkrProvider::request_error(IbkrFailure::Orphaned, 3);
        assert_eq!(classify_recovery(&orphaned), Some(RecoveryAction::Retry));

        let reset =
            IbkrProvider::request_error(IbkrFailure::Ibapi(ibapi::Error::ConnectionReset), 0);
        assert_eq!(classify_recovery(&reset), Some(RecoveryAction::Retry));

        let failed =
            IbkrProvider::request_error(IbkrFailure::Ibapi(ibapi::Error::ConnectionFailed), 7);
        assert_eq!(
            classify_recovery(&failed),
            Some(RecoveryAction::Reconnect { generation: 7 })
        );

        let shutdown = IbkrProvider::request_error(IbkrFailure::Ibapi(ibapi::Error::Shutdown), 9);
        assert_eq!(
            classify_recovery(&shutdown),
            Some(RecoveryAction::Reconnect { generation: 9 })
        );

        // Cancellation is not retryable; the downloader stops cleanly via its own cancel check.
        let canceled = IbkrProvider::request_error(IbkrFailure::Canceled, 0);
        assert_eq!(classify_recovery(&canceled), None);

        // Unrelated errors are not recoverable.
        assert_eq!(classify_recovery(&anyhow::anyhow!("nope")), None);
    }
}
