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

/// True for IBKR notice codes that report a transient competing-session takeover rather than a
/// fatal request error. 10187 is "Trading TWS session is connected from a different IP address",
/// returned when the same account signs in elsewhere; the condition clears on its own once the
/// other session ends, so the request must be retried from the unchanged cursor rather than the
/// symbol abandoned. Kept as a list so sibling codes (e.g. a future 10197) are easy to add.
fn is_competing_session_notice(code: i32) -> bool {
    matches!(code, 10187)
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
    /// The request made no progress for the configured stall window (carried here, in seconds):
    /// no data, no notice, no completion, and no connectivity-restore signal. TWS's link to the
    /// IBKR servers is wedged (e.g. a competing session opened elsewhere). The request is re-issued
    /// on the existing client (the socket is usually still alive; a genuinely dead one surfaces an
    /// `Io` error on the retry, which routes to Reconnect).
    Stalled(u64),
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
            IbkrFailure::Stalled(seconds) => write!(
                formatter,
                "request stalled with no response from IBKR for {seconds}s"
            ),
            IbkrFailure::Canceled => write!(formatter, "request canceled"),
        }
    }
}

impl StdError for IbkrRequestError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        // Display already renders the underlying ibapi message in full, so exposing that same
        // error as a `source` would make anyhow's `{:#}` chain print the message twice. The
        // typed failure is still available via `downcast_ref::<IbkrRequestError>` for recovery
        // classification, which is the only consumer that needs it.
        None
    }
}

/// True for I/O error kinds that mean the TWS/Gateway socket was lost and a fresh client
/// connection is required: e.g. Windows `os error 10053` (`ConnectionAborted`) or an
/// `UnexpectedEof` ("failed to fill whole buffer") while reading the next message. ibapi
/// surfaces these as [`ibapi::Error::Io`]. Its own transparent reconnect can fail (for
/// instance when TWS demands the paper-trading disclaimer be re-accepted), so we treat them
/// as recoverable and re-issue from the unchanged cursor after rebuilding the client.
fn is_connection_loss(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        error.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof
            | ErrorKind::NotConnected
            | ErrorKind::ConnectionRefused
    )
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
        // A competing-session takeover (10187) is transient and clears when the other session
        // ends. Retry from the unchanged cursor — without rebuilding the client — instead of
        // abandoning the symbol, so a brief login elsewhere no longer fails the whole run.
        IbkrFailure::Ibapi(ibapi::Error::Notice(notice))
            if is_competing_session_notice(notice.code) =>
        {
            Some(RecoveryAction::Retry)
        }
        IbkrFailure::Ibapi(ibapi::Error::ConnectionFailed | ibapi::Error::Shutdown) => {
            Some(RecoveryAction::Reconnect {
                generation: request.generation,
            })
        }
        // A stalled request is re-issued on the *existing* client, not reconnected. The socket is
        // typically still alive (it is serving other workers) and the request was orphaned
        // upstream, so a reconnect can't help and — because the live client still holds the
        // configured client id — would only collide with itself (IBKR error 326). If the socket is
        // genuinely dead, the re-issued request surfaces an `Io` connection-loss error that then
        // routes to Reconnect, by which point the id is free.
        IbkrFailure::Stalled(_) => Some(RecoveryAction::Retry),
        // A dropped API socket (os error 10053 / UnexpectedEof) arrives as an I/O error. Rebuild
        // the client and retry rather than abandoning the symbol, so a connection that recovers
        // (or a disclaimer that gets accepted) resumes the download instead of failing the run.
        IbkrFailure::Ibapi(ibapi::Error::Io(io)) if is_connection_loss(io) => {
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
    /// How long a request may make zero progress before it is declared stalled and reconnected.
    /// `None` disables stall detection (config `stall_timeout_seconds = 0`).
    stall_timeout: Option<Duration>,
}

impl IbkrProvider {
    pub fn connect(config: &IbkrConfig) -> Result<Self> {
        let connectivity = Arc::new(AtomicU64::new(0));
        let client = connect_client(config, &connectivity)?;
        Ok(Self {
            config: config.clone(),
            clients: ClientSlot::new(client),
            connectivity,
            stall_timeout: match config.stall_timeout_seconds {
                0 => None,
                seconds => Some(Duration::from_secs(seconds)),
            },
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
        .with_context(|| {
            format!(
                "failed to connect to IBKR at {connection_url} (client_id {})",
                config.client_id
            )
        })?;

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
        // Last time the request made progress (received any message). A request that produces
        // nothing for `self.stall_timeout` is wedged and must be reconnected rather than waited
        // on forever; the timer resets on every data tick or notice.
        let mut last_progress = Instant::now();
        loop {
            let started = Instant::now();
            match subscription.next_timeout(POLL_INTERVAL) {
                Some(Ok(SubscriptionItem::Data(tick))) => {
                    last_progress = Instant::now();
                    out.push(ProviderTick {
                        timestamp: tick.timestamp,
                        price: tick.price,
                        size: tick.size,
                    });
                }
                Some(Ok(SubscriptionItem::Notice(notice))) => {
                    last_progress = Instant::now();
                    warn!(client_id = self.config.client_id, generation, symbol = %contract.symbol, %notice, "IBKR notice during historical tick request");
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
                    // A request that has produced nothing for the whole stall window is wedged
                    // (e.g. TWS lost its IBKR-server link to a competing session) without any
                    // socket error or restore notice to wake us. Force a reconnect instead of
                    // hanging until the process is restarted. Skipped when stall detection is
                    // disabled (`stall_timeout` is `None`).
                    if let Some(stall_timeout) = self.stall_timeout
                        && last_progress.elapsed() >= stall_timeout
                    {
                        warn!(
                            client_id = self.config.client_id,
                            generation,
                            symbol = %contract.symbol,
                            stall_seconds = stall_timeout.as_secs(),
                            "historical request stalled; recovering on the existing client"
                        );
                        return Err(Self::request_error(
                            IbkrFailure::Stalled(stall_timeout.as_secs()),
                            generation,
                        ));
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

        // A stalled request retries on the existing client (no reconnect → no 326 self-collision).
        let stalled = IbkrProvider::request_error(IbkrFailure::Stalled(30), 5);
        assert_eq!(classify_recovery(&stalled), Some(RecoveryAction::Retry));

        // A competing-session notice (10187) is transient: retry from the unchanged cursor.
        let competing = IbkrProvider::request_error(
            IbkrFailure::Ibapi(ibapi::Error::Notice(notice(
                10187,
                "Trading TWS session is connected from a different IP address",
            ))),
            2,
        );
        assert_eq!(classify_recovery(&competing), Some(RecoveryAction::Retry));

        // An unrelated notice (e.g. 200 no security definition) stays fatal.
        let no_contract = IbkrProvider::request_error(
            IbkrFailure::Ibapi(ibapi::Error::Notice(notice(
                200,
                "No security definition found",
            ))),
            0,
        );
        assert_eq!(classify_recovery(&no_contract), None);

        // Cancellation is not retryable; the downloader stops cleanly via its own cancel check.
        let canceled = IbkrProvider::request_error(IbkrFailure::Canceled, 0);
        assert_eq!(classify_recovery(&canceled), None);

        // Unrelated errors are not recoverable.
        assert_eq!(classify_recovery(&anyhow::anyhow!("nope")), None);
    }

    /// Builds a minimal TWS notice for classification tests (no wire timestamp or reject JSON).
    fn notice(code: i32, message: &str) -> ibapi::Notice {
        ibapi::Notice {
            code,
            message: message.to_owned(),
            error_time: None,
            advanced_order_reject_json: String::new(),
        }
    }

    /// The doubled-message regression: `IbkrRequestError` renders the ibapi message once, so
    /// anyhow's `{:#}` chain must not repeat it.
    #[test]
    fn ibapi_failure_display_is_not_duplicated_in_the_anyhow_chain() {
        let error = IbkrProvider::request_error(
            IbkrFailure::Ibapi(ibapi::Error::Notice(notice(
                10187,
                "Trading TWS session is connected from a different IP address",
            ))),
            0,
        );
        let rendered = format!("{error:#}");
        let occurrences = rendered.matches("different IP address").count();
        assert_eq!(occurrences, 1, "message duplicated: {rendered}");
    }

    #[test]
    fn socket_loss_io_errors_reconnect_and_resume() {
        use std::io::{Error as IoError, ErrorKind};

        // os error 10053 is ConnectionAborted; "failed to fill whole buffer" is UnexpectedEof.
        for kind in [
            ErrorKind::ConnectionAborted,
            ErrorKind::UnexpectedEof,
            ErrorKind::ConnectionReset,
            ErrorKind::BrokenPipe,
            ErrorKind::NotConnected,
            ErrorKind::ConnectionRefused,
        ] {
            let error = IbkrProvider::request_error(
                IbkrFailure::Ibapi(ibapi::Error::Io(IoError::from(kind))),
                4,
            );
            assert_eq!(
                classify_recovery(&error),
                Some(RecoveryAction::Reconnect { generation: 4 }),
                "kind {kind:?} should reconnect"
            );
        }

        // A non-connection I/O error is not a transport loss and must stay non-retryable.
        let unrelated = IbkrProvider::request_error(
            IbkrFailure::Ibapi(ibapi::Error::Io(IoError::from(ErrorKind::InvalidData))),
            1,
        );
        assert_eq!(classify_recovery(&unrelated), None);
    }
}
