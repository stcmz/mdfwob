use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use jiff::tz::TimeZone;
use time::OffsetDateTime;
use tracing::{error, info, warn};

use crate::{
    config::{Config, OptionContractConfig, OptionRight, ProviderKind, StockContractConfig},
    fwob_options::FwobOptions,
    providers::{DatabentoProvider, IbkrProvider, MarketDataProvider, RecoveryAction},
    storage::{TickStore, TickWriter},
    tick::ShortTick,
};

const MAX_TICKS_PER_REQUEST: i32 = 1000;

#[derive(Debug, Clone)]
pub struct DownloadPlan {
    config: Config,
    fwob: FwobOptions,
    contracts: Vec<StockContract>,
    configured_start: Option<OffsetDateTime>,
    configured_end: Option<OffsetDateTime>,
    timezone: TimeZone,
}

impl DownloadPlan {
    pub fn new(config: Config, fwob: FwobOptions) -> Result<Self> {
        for option in &config.options {
            validate_option(option)?;
        }
        if config.download.parallelism == 0 {
            bail!("download.parallelism must be at least 1");
        }
        let configured_start = parse_optional_time(config.download.start.as_deref())?;
        let configured_end = parse_optional_time(config.download.end.as_deref())?;
        if let (Some(start), Some(end)) = (configured_start, configured_end)
            && start >= end
        {
            bail!("download.start must be earlier than download.end");
        }
        let mut contracts = config
            .stocks
            .iter()
            .flat_map(|group| {
                group
                    .symbols
                    .iter()
                    .map(|symbol| stock_contract(group, symbol))
            })
            .collect::<Vec<_>>();
        contracts.extend(config.options.iter().map(option_contract));
        if contracts.is_empty() {
            bail!("no stock or option contracts selected");
        }
        validate_output_titles(&contracts)?;
        let timezone = TimeZone::get(&config.download.timezone)
            .with_context(|| format!("invalid download.timezone {:?}", config.download.timezone))?;
        Ok(Self {
            config,
            fwob,
            contracts,
            configured_start,
            configured_end,
            timezone,
        })
    }
}

#[derive(Clone)]
pub struct CancellationToken {
    canceled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            canceled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.canceled.store(true, Ordering::SeqCst);
    }

    pub fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::SeqCst)
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Downloader {
    plan: DownloadPlan,
}

impl Downloader {
    pub fn new(plan: DownloadPlan) -> Self {
        Self { plan }
    }

    pub fn run(self) -> Result<()> {
        let cancel = CancellationToken::new();
        install_ctrlc_handler(cancel.clone())?;

        let pacer = RequestPacer::new(Duration::from_millis(
            self.plan.config.download.request_interval_ms,
        ));
        let retry_pacer = RequestPacer::new(Duration::from_millis(
            self.plan.config.download.retry_interval_ms,
        ));
        match self.plan.config.download.provider {
            ProviderKind::Ibkr => {
                let provider = IbkrProvider::connect(&self.plan.config.ibkr)?;
                run_with_provider(self.plan, &provider, &cancel, &pacer, &retry_pacer)
            }
            ProviderKind::Databento => {
                if self.plan.config.download.start.is_none() {
                    bail!("download.start or --start is required for provider databento");
                }
                let provider = DatabentoProvider::connect(&self.plan.config.databento)?;
                run_with_provider(self.plan, &provider, &cancel, &pacer, &retry_pacer)
            }
            provider => bail!("provider {provider} is not implemented yet"),
        }
    }
}

fn install_ctrlc_handler(cancel: CancellationToken) -> Result<()> {
    let presses = Arc::new(AtomicUsize::new(0));
    let handler_presses = Arc::clone(&presses);
    ctrlc::set_handler(move || {
        if handler_presses.fetch_add(1, Ordering::SeqCst) == 0 {
            cancel.cancel();
            warn!(
                "Ctrl+C received; stopping after the current request/append completes. Press Ctrl+C again to force exit."
            );
        } else {
            warn!("second Ctrl+C received; forcing exit");
            std::process::exit(130);
        }
    })
    .context("failed to install Ctrl+C handler")
}

#[derive(Debug, Clone)]
pub struct StockContract {
    pub symbol: String,
    pub currency: String,
    pub exchange: String,
    pub primary_exchange: Option<String>,
    pub option: Option<OptionSpec>,
}

#[derive(Debug, Clone)]
pub struct OptionSpec {
    pub expiration: String,
    pub strike: f64,
    pub right: OptionRight,
    pub multiplier: String,
    pub trading_class: Option<String>,
    pub local_symbol: Option<String>,
}

impl StockContract {
    fn output_title(&self) -> String {
        match &self.option {
            Some(option) => format!(
                "{}_{}_{}_{}",
                self.symbol,
                option.expiration,
                option.right.code(),
                option.strike
            ),
            None => self.symbol.clone(),
        }
    }
}

struct RequestPacer {
    next: Mutex<Instant>,
    interval: Duration,
}

impl RequestPacer {
    fn new(interval: Duration) -> Self {
        Self {
            next: Mutex::new(Instant::now()),
            interval,
        }
    }

    fn wait(&self, cancel: &CancellationToken) -> bool {
        loop {
            if cancel.is_canceled() {
                return false;
            }
            let mut next = self.next.lock().expect("request pacer poisoned");
            let now = Instant::now();
            if now >= *next {
                *next = now + self.interval;
                return true;
            }
            let sleep_for = (*next - now).min(Duration::from_millis(100));
            drop(next);
            std::thread::sleep(sleep_for);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProviderTick {
    pub timestamp: OffsetDateTime,
    pub price: f64,
    pub size: i32,
}

fn run_with_provider(
    plan: DownloadPlan,
    provider: &impl MarketDataProvider,
    cancel: &CancellationToken,
    pacer: &RequestPacer,
    retry_pacer: &RequestPacer,
) -> Result<()> {
    let downloader = SymbolDownloader {
        provider,
        configured_start: plan.configured_start,
        configured_end: plan.configured_end,
        use_rth: plan.config.download.use_rth,
        reconnect_timeout_seconds: plan.config.ibkr.reconnect_timeout_seconds,
        timezone: &plan.timezone,
        cancel,
        pacer,
        retry_pacer,
    };

    let jobs = VecDeque::from(plan.contracts.clone());
    let worker_count = plan.config.download.parallelism.max(1).min(jobs.len());
    let jobs = Mutex::new(jobs);
    let first_error = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    if cancel.is_canceled() {
                        break;
                    }
                    let contract = jobs.lock().expect("job queue poisoned").pop_front();
                    let Some(contract) = contract else {
                        break;
                    };
                    let store = TickStore::new(
                        &plan.config.download.output_dir,
                        contract.output_title(),
                        plan.fwob,
                    );
                    let result = (|| {
                        let _lock = store.try_lock()?;
                        store.ensure_compatible_format()?;
                        store.verify_existing()?;
                        downloader.download_symbol(&contract, &store)
                    })();
                    if let Err(error) = result {
                        // A single symbol failing must not abort the whole run; record the first
                        // error (so the process still exits non-zero) and move on to the next
                        // contract. Only Ctrl+C cancels remaining work.
                        error!(
                            symbol = %contract.symbol,
                            error = %format!("{error:#}"),
                            "symbol download failed; continuing with remaining symbols"
                        );
                        let mut slot = first_error.lock().expect("error slot poisoned");
                        if slot.is_none() {
                            *slot = Some(error);
                        }
                    }
                }
            });
        }
    });

    if let Some(error) = first_error.lock().expect("error slot poisoned").take() {
        Err(error)
    } else {
        Ok(())
    }
}

struct SymbolDownloader<'a, P> {
    provider: &'a P,
    configured_start: Option<OffsetDateTime>,
    configured_end: Option<OffsetDateTime>,
    use_rth: bool,
    /// IBKR reconnect retry budget: -1 retries forever, 0 disables retries, and a
    /// positive value caps the wall-clock seconds spent retrying a single request.
    reconnect_timeout_seconds: i64,
    /// Exchange timezone that anchors the day-advance and log timestamps.
    timezone: &'a TimeZone,
    cancel: &'a CancellationToken,
    /// Paces the first attempt of each request (normal data-fetch rate).
    pacer: &'a RequestPacer,
    /// Paces retry attempts after a recoverable failure, independently of `pacer`.
    retry_pacer: &'a RequestPacer,
}

impl<P: MarketDataProvider> SymbolDownloader<'_, P> {
    fn download_symbol(&self, contract: &StockContract, store: &TickStore) -> Result<()> {
        let cursor = match store.last_timestamp()? {
            Some(last) => OffsetDateTime::from_unix_timestamp(i64::from(last) + 1)?,
            None => match self.configured_start {
                Some(start) => start,
                None => {
                    if !self.pacer.wait(self.cancel) {
                        return Ok(());
                    }
                    self.provider.head_timestamp(contract, self.use_rth)?
                }
            },
        };

        info!(
            symbol = %contract.symbol,
            output = %store.path().display(),
            start = %fmt_in_tz(cursor, self.timezone),
            end = %self.configured_end.map_or_else(|| "dynamic-now".to_string(), |end| fmt_in_tz(end, self.timezone)),
            "downloading symbol"
        );

        // Keep one writer open for the whole symbol instead of reopening/finishing per batch, then
        // finish exactly once. `finish` runs on every exit path (normal completion, cancellation, and
        // the error from a rejected batch) so all previously appended batches are committed.
        let mut writer = store.writer();
        let appended = match self.download_symbol_batches(contract, &mut writer, cursor) {
            Ok(appended) => appended,
            Err(download_error) => {
                // Commit whatever was appended before this batch failed. A finalization failure is
                // a separate durability problem and must not be hidden by the original error.
                if let Err(finish_error) = writer.finish() {
                    bail!(
                        "download failed for {}: {download_error:#}; additionally failed to finalize {}: {finish_error:#}",
                        contract.symbol,
                        store.path().display()
                    );
                }
                return Err(download_error);
            }
        };
        writer
            .finish()
            .with_context(|| format!("failed to finalize {}", store.path().display()))?;

        if self.cancel.is_canceled() {
            info!(symbol = %contract.symbol, appended, "stopped after cancellation");
        } else if appended == 0 {
            info!(symbol = %contract.symbol, "nothing to download; already up to date");
        } else {
            info!(symbol = %contract.symbol, appended, "finished downloading symbol");
        }
        Ok(())
    }

    fn download_symbol_batches(
        &self,
        contract: &StockContract,
        writer: &mut TickWriter<'_>,
        mut cursor: OffsetDateTime,
    ) -> Result<u64> {
        let mut appended = 0u64;
        loop {
            let end = resolve_end(self.configured_end);
            if cursor >= end {
                break;
            }
            if self.cancel.is_canceled() {
                warn!(symbol = %contract.symbol, "cancellation requested; stopping before next request");
                break;
            }

            let Some(mut ticks) = self.fetch_with_recovery(contract, cursor, end)? else {
                // Cancellation requested while pacing/retrying; stop this symbol cleanly.
                break;
            };
            validate_provider_batch(&ticks, cursor, &contract.symbol)?;
            let provider_returned_ticks = !ticks.is_empty();
            ticks.retain(|tick| tick.timestamp < end);
            if ticks.is_empty() {
                if provider_returned_ticks {
                    break;
                }
                // Advance to the start of the next calendar day in the EXCHANGE timezone. A
                // trading day (pre-market through after-hours) lives within one local day, so its
                // local midnight always precedes the next session's pre-market -- the advance can
                // never step over a session. A UTC-day jump cannot guarantee this because US
                // after-hours crosses UTC midnight (more so in winter, ending 01:00 UTC).
                cursor = next_day_start(cursor, self.timezone)?;
                info!(symbol = %contract.symbol, next = %fmt_in_tz(cursor, self.timezone), "empty response; advanced to next day");
                continue;
            }

            let mut frames = Vec::with_capacity(ticks.len());
            for tick in ticks {
                frames.push(provider_tick_to_short_tick(tick, &contract.symbol)?);
            }

            let count = frames.len();
            let last_second = frames.last().expect("frames is non-empty").time;
            writer.append_ticks(&frames)?;
            appended += count as u64;
            cursor = OffsetDateTime::from_unix_timestamp(i64::from(last_second) + 1)?;
            info!(symbol = %contract.symbol, count, next = %fmt_in_tz(cursor, self.timezone), "appended ticks");
        }
        Ok(appended)
    }

    /// Fetch one batch for `cursor`, transparently retrying retryable provider failures
    /// (connection resets / reconnects) from the **unchanged** cursor so a dropped gateway
    /// never advances past unfetched data. Returns `Ok(None)` when cancellation interrupts
    /// the wait. The retry budget is per logical request: it starts at the first retryable
    /// failure and resets once a request succeeds.
    fn fetch_with_recovery(
        &self,
        contract: &StockContract,
        cursor: OffsetDateTime,
        end: OffsetDateTime,
    ) -> Result<Option<Vec<ProviderTick>>> {
        let mut retry_deadline: Option<Instant> = None;
        let mut first_attempt = true;
        loop {
            // The first attempt paces on the normal data-fetch rate; retries pace on the
            // separate retry interval so a slow request cadence never throttles recovery.
            let pacer = if first_attempt {
                self.pacer
            } else {
                self.retry_pacer
            };
            if !pacer.wait(self.cancel) {
                return Ok(None);
            }
            first_attempt = false;
            let error = match self.provider.historical_trade_ticks(
                contract,
                cursor,
                end,
                MAX_TICKS_PER_REQUEST,
                self.use_rth,
                self.cancel,
            ) {
                Ok(ticks) => return Ok(Some(ticks)),
                Err(error) => error,
            };

            // A request interrupted by Ctrl+C surfaces as an error; while cancellation is in
            // effect, treat any failure as a clean stop instead of retrying or abandoning.
            if self.cancel.is_canceled() {
                return Ok(None);
            }

            let Some(action) = self.provider.recovery_action(&error) else {
                return Err(error);
            };
            // 0 disables retries entirely; surface the error like any other.
            if self.reconnect_timeout_seconds == 0 {
                return Err(error);
            }
            // A positive budget caps wall-clock retry time from the first retryable failure.
            if self.reconnect_timeout_seconds > 0 {
                let deadline = *retry_deadline.get_or_insert_with(|| {
                    Instant::now() + Duration::from_secs(self.reconnect_timeout_seconds as u64)
                });
                if Instant::now() >= deadline {
                    return Err(error).with_context(|| {
                        format!(
                            "exhausted {}s reconnect budget for {}",
                            self.reconnect_timeout_seconds, contract.symbol
                        )
                    });
                }
            }

            warn!(
                symbol = %contract.symbol,
                error = %format!("{error:#}"),
                "retryable provider error; retrying from the unchanged cursor"
            );
            if let RecoveryAction::Reconnect { generation } = action {
                // A failed reconnect is itself retryable while within budget; keep looping.
                if let Err(reconnect_error) = self.provider.reconnect(generation) {
                    warn!(
                        symbol = %contract.symbol,
                        error = %format!("{reconnect_error:#}"),
                        "reconnect attempt failed; will retry"
                    );
                }
            }
        }
    }
}

fn validate_provider_batch(
    ticks: &[ProviderTick],
    cursor: OffsetDateTime,
    symbol: &str,
) -> Result<()> {
    let mut previous = None;
    for tick in ticks {
        if tick.timestamp < cursor {
            bail!(
                "provider returned a tick before the requested cursor for {symbol}: {} < {cursor}",
                tick.timestamp
            );
        }
        if previous.is_some_and(|timestamp| tick.timestamp < timestamp) {
            bail!("provider returned out-of-order ticks for {symbol}");
        }
        previous = Some(tick.timestamp);
    }
    Ok(())
}

fn resolve_end(configured_end: Option<OffsetDateTime>) -> OffsetDateTime {
    configured_end.unwrap_or_else(OffsetDateTime::now_utc)
}

/// UTC instant of the start of the calendar day *after* `cursor`'s local day in `tz`. Always
/// strictly after `cursor` (so the download loop makes progress), and — because a trading day
/// lives within one local day — never past the next session's first tick.
fn next_day_start(cursor: OffsetDateTime, tz: &TimeZone) -> Result<OffsetDateTime> {
    let timestamp = jiff::Timestamp::from_second(cursor.unix_timestamp())
        .context("cursor timestamp is out of range")?;
    let next_date = timestamp
        .to_zoned(tz.clone())
        .date()
        .tomorrow()
        .context("date overflow while advancing past an empty day")?;
    let start = next_date
        .to_zoned(tz.clone())
        .context("next local day has no valid start instant")?;
    OffsetDateTime::from_unix_timestamp(start.timestamp().as_second())
        .context("next day start is out of range")
}

/// Renders a UTC instant as a local wall-clock string in `tz` (with the zone abbreviation), for
/// human-readable download logs. The stored FWOB value is unaffected — only logging.
fn fmt_in_tz(instant: OffsetDateTime, tz: &TimeZone) -> String {
    match jiff::Timestamp::from_second(instant.unix_timestamp()) {
        Ok(timestamp) => timestamp
            .to_zoned(tz.clone())
            .strftime("%Y-%m-%d %H:%M:%S %Z")
            .to_string(),
        Err(_) => instant.to_string(),
    }
}

fn provider_tick_to_short_tick(tick: ProviderTick, symbol: &str) -> Result<ShortTick> {
    // Unix seconds identify an absolute UTC instant. The source offset or
    // exchange timezone must never be encoded as local wall-clock time.
    let utc_seconds = tick.timestamp.unix_timestamp();
    if utc_seconds < 0 || utc_seconds > u32::MAX as i64 {
        bail!("tick timestamp is outside u32 range for {symbol}: {utc_seconds}");
    }
    ShortTick::new(utc_seconds as u32, tick.price, tick.size)
}

fn stock_contract(group: &StockContractConfig, symbol: &str) -> StockContract {
    StockContract {
        symbol: symbol.to_string(),
        currency: group.currency.clone(),
        exchange: group.exchange.clone(),
        primary_exchange: group.primary_exchange.clone(),
        option: None,
    }
}

fn option_contract(config: &OptionContractConfig) -> StockContract {
    StockContract {
        symbol: config.symbol.clone(),
        currency: config.currency.clone(),
        exchange: config.exchange.clone(),
        primary_exchange: None,
        option: Some(OptionSpec {
            expiration: config.expiration.clone(),
            strike: config.strike,
            right: config.right,
            multiplier: config.multiplier.clone(),
            trading_class: config.trading_class.clone(),
            local_symbol: config.local_symbol.clone(),
        }),
    }
}

fn validate_output_titles(contracts: &[StockContract]) -> Result<()> {
    let mut outputs = HashMap::<String, String>::new();
    for contract in contracts {
        let title = contract.output_title();
        validate_file_stem(&title)?;
        let key = title.to_ascii_lowercase();
        if let Some(existing) = outputs.insert(key, contract.symbol.clone()) {
            bail!(
                "contracts {existing} and {} both write to {title}.fwob",
                contract.symbol
            );
        }
    }
    Ok(())
}

fn validate_file_stem(value: &str) -> Result<()> {
    if value.is_empty()
        || value.trim() != value
        || value.ends_with('.')
        || matches!(value, "." | "..")
    {
        bail!("unsafe FWOB output name '{value}'");
    }
    if value
        .chars()
        .any(|character| character.is_control() || r#"<>:"/\|?*"#.contains(character))
    {
        bail!("unsafe FWOB output name '{value}'");
    }
    let base = value
        .split('.')
        .next()
        .unwrap_or(value)
        .to_ascii_uppercase();
    let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (base.len() == 4
            && matches!(&base[..3], "COM" | "LPT")
            && matches!(base.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        bail!("unsafe FWOB output name '{value}'");
    }
    Ok(())
}

pub(crate) fn ibkr_contract(contract: &StockContract) -> ibapi::contracts::Contract {
    let mut ib_contract = match &contract.option {
        Some(option) => {
            let right = match option.right {
                OptionRight::Call => ibapi::contracts::OptionRight::Call,
                OptionRight::Put => ibapi::contracts::OptionRight::Put,
            };
            let mut contract = ibapi::contracts::Contract::option(
                contract.symbol.as_str(),
                option.expiration.as_str(),
                option.strike,
                right,
            );
            contract.multiplier = option.multiplier.clone();
            if let Some(trading_class) = &option.trading_class {
                contract.trading_class = trading_class.clone();
            }
            if let Some(local_symbol) = &option.local_symbol {
                contract.local_symbol = local_symbol.clone();
            }
            contract
        }
        None => ibapi::contracts::Contract::stock(contract.symbol.as_str()).build(),
    };
    ib_contract.currency = ibapi::contracts::Currency(contract.currency.clone());
    ib_contract.exchange = ibapi::contracts::Exchange(contract.exchange.clone());
    if let Some(primary_exchange) = &contract.primary_exchange {
        ib_contract.primary_exchange = ibapi::contracts::Exchange(primary_exchange.clone());
    }
    ib_contract
}

fn validate_option(option: &OptionContractConfig) -> Result<()> {
    if option.symbol.trim().is_empty() {
        bail!("option symbol must not be empty");
    }
    if option.expiration.len() != 8 || !option.expiration.bytes().all(|byte| byte.is_ascii_digit())
    {
        bail!("option expiration must use YYYYMMDD: {}", option.expiration);
    }
    if !option.strike.is_finite() || option.strike <= 0.0 {
        bail!(
            "option strike must be a positive finite number for {}",
            option.symbol
        );
    }
    if option.multiplier.trim().is_empty() {
        bail!("option multiplier must not be empty for {}", option.symbol);
    }
    Ok(())
}

pub(crate) fn trading_hours(use_rth: bool) -> ibapi::market_data::TradingHours {
    if use_rth {
        ibapi::market_data::TradingHours::Regular
    } else {
        ibapi::market_data::TradingHours::Extended
    }
}

fn parse_optional_time(value: Option<&str>) -> Result<Option<OffsetDateTime>> {
    value.map(parse_time).transpose()
}

fn parse_time(value: &str) -> Result<OffsetDateTime> {
    if let Ok(value) = OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
    {
        return Ok(value);
    }
    let format = time::macros::format_description!("[year]-[month]-[day]");
    let date = time::Date::parse(value, format)
        .map_err(|_| anyhow::anyhow!("invalid date/time: {value}"))?;
    Ok(date.midnight().assume_utc())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    struct MockProvider {
        head: OffsetDateTime,
        batches: Mutex<VecDeque<Vec<ProviderTick>>>,
        starts: Mutex<Vec<OffsetDateTime>>,
    }

    impl MarketDataProvider for MockProvider {
        fn head_timestamp(
            &self,
            _contract: &StockContract,
            _use_rth: bool,
        ) -> Result<OffsetDateTime> {
            Ok(self.head)
        }

        fn historical_trade_ticks(
            &self,
            _contract: &StockContract,
            start: OffsetDateTime,
            _end: OffsetDateTime,
            _max_ticks: i32,
            _use_rth: bool,
            _cancel: &CancellationToken,
        ) -> Result<Vec<ProviderTick>> {
            self.starts.lock().unwrap().push(start);
            Ok(self.batches.lock().unwrap().pop_front().unwrap())
        }
    }

    /// Error type the recovery mock returns; `action` mirrors what a real provider's
    /// `recovery_action` would report (`None` = non-retryable).
    #[derive(Debug)]
    struct MockError {
        action: Option<RecoveryAction>,
    }

    impl std::fmt::Display for MockError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "mock provider error")
        }
    }

    impl std::error::Error for MockError {}

    enum Response {
        Data(Vec<ProviderTick>),
        /// A failure; `Some(action)` is retryable, `None` is fatal.
        Fail(Option<RecoveryAction>),
    }

    /// What the provider returns once its scripted `responses` queue drains.
    enum Drain {
        Empty,
        FailRetryable(RecoveryAction),
    }

    struct RecoveryProvider {
        responses: Mutex<VecDeque<Response>>,
        drain: Drain,
        starts: Mutex<Vec<OffsetDateTime>>,
        reconnects: Mutex<u64>,
        /// When `Some(n)`, cancel `cancel` once the n-th request has been issued.
        cancel_after: Option<usize>,
        cancel: CancellationToken,
    }

    impl RecoveryProvider {
        fn new(responses: Vec<Response>, drain: Drain) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                drain,
                starts: Mutex::new(Vec::new()),
                reconnects: Mutex::new(0),
                cancel_after: None,
                cancel: CancellationToken::new(),
            }
        }

        fn request_count(&self) -> usize {
            self.starts.lock().unwrap().len()
        }

        fn reconnect_count(&self) -> u64 {
            *self.reconnects.lock().unwrap()
        }
    }

    impl MarketDataProvider for RecoveryProvider {
        fn head_timestamp(
            &self,
            _contract: &StockContract,
            _use_rth: bool,
        ) -> Result<OffsetDateTime> {
            unreachable!("recovery tests configure an explicit start")
        }

        fn historical_trade_ticks(
            &self,
            _contract: &StockContract,
            start: OffsetDateTime,
            _end: OffsetDateTime,
            _max_ticks: i32,
            _use_rth: bool,
            _cancel: &CancellationToken,
        ) -> Result<Vec<ProviderTick>> {
            {
                let mut starts = self.starts.lock().unwrap();
                starts.push(start);
                if self.cancel_after.is_some_and(|n| starts.len() >= n) {
                    self.cancel.cancel();
                }
            }
            match self.responses.lock().unwrap().pop_front() {
                Some(Response::Data(ticks)) => Ok(ticks),
                Some(Response::Fail(action)) => Err(MockError { action }.into()),
                None => match self.drain {
                    Drain::Empty => Ok(Vec::new()),
                    Drain::FailRetryable(action) => Err(MockError {
                        action: Some(action),
                    }
                    .into()),
                },
            }
        }

        fn recovery_action(&self, error: &anyhow::Error) -> Option<RecoveryAction> {
            error
                .chain()
                .find_map(|cause| cause.downcast_ref::<MockError>())
                .and_then(|mock| mock.action)
        }

        fn reconnect(&self, _generation: u64) -> Result<()> {
            *self.reconnects.lock().unwrap() += 1;
            Ok(())
        }
    }

    fn download_with_recovery(
        provider: &RecoveryProvider,
        reconnect_timeout_seconds: i64,
        configured_start: OffsetDateTime,
        configured_end: Option<OffsetDateTime>,
        cancel: &CancellationToken,
        pacer: &RequestPacer,
        store: &TickStore,
    ) -> Result<()> {
        SymbolDownloader {
            provider,
            configured_start: Some(configured_start),
            configured_end,
            use_rth: false,
            reconnect_timeout_seconds,
            timezone: &TimeZone::UTC,
            cancel,
            pacer,
            // Tests reuse the same pacer for retries; production wires a separate one.
            retry_pacer: pacer,
        }
        .download_symbol(&test_stock("AAPL"), store)
    }

    #[test]
    fn reset_then_success_retries_same_cursor_without_losing_data() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_100_000).unwrap();
        let provider = RecoveryProvider::new(
            vec![
                Response::Fail(Some(RecoveryAction::Retry)),
                Response::Data(vec![
                    ProviderTick {
                        timestamp: base,
                        price: 10.0,
                        size: 1,
                    },
                    ProviderTick {
                        timestamp: base + time::Duration::seconds(1),
                        price: 10.1,
                        size: 2,
                    },
                ]),
            ],
            Drain::Empty,
        );
        let dir = temp_dir("mdfwob-reset-retry");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        let cancel = CancellationToken::new();
        let pacer = RequestPacer::new(Duration::ZERO);

        download_with_recovery(
            &provider,
            -1,
            base,
            Some(base + time::Duration::seconds(2)),
            &cancel,
            &pacer,
            &store,
        )
        .unwrap();

        // The reset re-requests the SAME cursor (no day-skip), then the data lands intact.
        assert_eq!(provider.starts.lock().unwrap().as_slice(), &[base, base]);
        assert_eq!(provider.reconnect_count(), 0);
        assert_eq!(
            store.last_timestamp().unwrap(),
            Some((base + time::Duration::seconds(1)).unix_timestamp() as u32)
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reconnect_action_recreates_client_once_then_resumes() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_200_000).unwrap();
        let provider = RecoveryProvider::new(
            vec![
                Response::Fail(Some(RecoveryAction::Reconnect { generation: 0 })),
                Response::Data(vec![ProviderTick {
                    timestamp: base,
                    price: 10.0,
                    size: 1,
                }]),
            ],
            Drain::Empty,
        );
        let dir = temp_dir("mdfwob-reconnect");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        let cancel = CancellationToken::new();
        let pacer = RequestPacer::new(Duration::ZERO);

        download_with_recovery(
            &provider,
            -1,
            base,
            Some(base + time::Duration::seconds(1)),
            &cancel,
            &pacer,
            &store,
        )
        .unwrap();

        assert_eq!(provider.reconnect_count(), 1);
        assert_eq!(provider.starts.lock().unwrap().as_slice(), &[base, base]);
        assert_eq!(
            store.last_timestamp().unwrap(),
            Some(base.unix_timestamp() as u32)
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn zero_timeout_fails_the_symbol_without_retrying() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_300_000).unwrap();
        let provider = RecoveryProvider::new(
            vec![Response::Fail(Some(RecoveryAction::Retry))],
            Drain::Empty,
        );
        let dir = temp_dir("mdfwob-zero-timeout");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        let cancel = CancellationToken::new();
        let pacer = RequestPacer::new(Duration::ZERO);

        let result = download_with_recovery(
            &provider,
            0,
            base,
            Some(base + time::Duration::seconds(10)),
            &cancel,
            &pacer,
            &store,
        );

        assert!(result.is_err());
        assert_eq!(provider.request_count(), 1);
        assert_eq!(provider.reconnect_count(), 0);
        fs::remove_dir_all(dir).unwrap_or(());
    }

    #[test]
    fn non_retryable_error_propagates_immediately() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_400_000).unwrap();
        let provider = RecoveryProvider::new(vec![Response::Fail(None)], Drain::Empty);
        let dir = temp_dir("mdfwob-fatal");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        let cancel = CancellationToken::new();
        let pacer = RequestPacer::new(Duration::ZERO);

        let result = download_with_recovery(
            &provider,
            -1,
            base,
            Some(base + time::Duration::seconds(10)),
            &cancel,
            &pacer,
            &store,
        );

        assert!(result.is_err());
        assert_eq!(provider.request_count(), 1);
        assert_eq!(provider.reconnect_count(), 0);
        fs::remove_dir_all(dir).unwrap_or(());
    }

    #[test]
    fn finite_timeout_exhaustion_fails_the_symbol() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_500_000).unwrap();
        let provider = RecoveryProvider::new(vec![], Drain::FailRetryable(RecoveryAction::Retry));
        let dir = temp_dir("mdfwob-finite-timeout");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        let cancel = CancellationToken::new();
        // Pace retries so a 1s budget exhausts in a bounded number of attempts.
        let pacer = RequestPacer::new(Duration::from_millis(100));

        let result = download_with_recovery(
            &provider,
            1,
            base,
            Some(base + time::Duration::seconds(10)),
            &cancel,
            &pacer,
            &store,
        );

        let error = result.unwrap_err();
        assert!(error.to_string().contains("reconnect budget"));
        assert!(provider.request_count() >= 1);
        fs::remove_dir_all(dir).unwrap_or(());
    }

    #[test]
    fn infinite_retry_is_interrupted_by_cancellation() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_600_000).unwrap();
        let cancel = CancellationToken::new();
        let mut provider =
            RecoveryProvider::new(vec![], Drain::FailRetryable(RecoveryAction::Retry));
        provider.cancel_after = Some(3);
        provider.cancel = cancel.clone();

        let dir = temp_dir("mdfwob-infinite-cancel");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        let pacer = RequestPacer::new(Duration::ZERO);

        // Infinite budget would loop forever; cancellation must stop it cleanly (Ok).
        download_with_recovery(
            &provider,
            -1,
            base,
            Some(base + time::Duration::seconds(10)),
            &cancel,
            &pacer,
            &store,
        )
        .unwrap();

        assert!(cancel.is_canceled());
        assert_eq!(provider.request_count(), 3);
        fs::remove_dir_all(dir).unwrap_or(());
    }

    #[test]
    fn cancel_during_request_stops_cleanly_even_on_non_retryable_error() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_700_000).unwrap();
        let cancel = CancellationToken::new();
        // Fatal (non-retryable) error, but cancellation fires during the same request: the
        // downloader must stop cleanly (Ok) rather than surface the error.
        let mut provider = RecoveryProvider::new(vec![Response::Fail(None)], Drain::Empty);
        provider.cancel_after = Some(1);
        provider.cancel = cancel.clone();

        let dir = temp_dir("mdfwob-cancel-fatal");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        let pacer = RequestPacer::new(Duration::ZERO);

        let result = download_with_recovery(
            &provider,
            -1,
            base,
            Some(base + time::Duration::seconds(10)),
            &cancel,
            &pacer,
            &store,
        );

        assert!(result.is_ok());
        assert!(cancel.is_canceled());
        assert_eq!(provider.request_count(), 1);
        fs::remove_dir_all(dir).unwrap_or(());
    }

    #[test]
    fn mock_download_appends_batches_and_advances_cursor() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let provider = MockProvider {
            head: base,
            batches: Mutex::new(VecDeque::from(vec![
                vec![
                    ProviderTick {
                        timestamp: base,
                        price: 10.0,
                        size: 100,
                    },
                    ProviderTick {
                        timestamp: base + time::Duration::seconds(1),
                        price: 10.1,
                        size: 200,
                    },
                ],
                vec![ProviderTick {
                    timestamp: base + time::Duration::seconds(2),
                    price: 10.2,
                    size: 300,
                }],
            ])),
            starts: Mutex::new(Vec::new()),
        };

        let dir = temp_dir("mdfwob-download");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        let contract = StockContract {
            symbol: "AAPL".into(),
            currency: "USD".into(),
            exchange: "SMART".into(),
            primary_exchange: Some("NASDAQ".into()),
            option: None,
        };
        let cancel = CancellationToken::new();
        let pacer = RequestPacer::new(Duration::ZERO);

        SymbolDownloader {
            provider: &provider,
            configured_start: None,
            configured_end: Some(base + time::Duration::seconds(3)),
            use_rth: false,
            reconnect_timeout_seconds: -1,
            timezone: &TimeZone::UTC,
            cancel: &cancel,
            pacer: &pacer,
            retry_pacer: &pacer,
        }
        .download_symbol(&contract, &store)
        .unwrap();

        assert_eq!(store.last_timestamp().unwrap(), Some(1_700_000_002));
        assert_eq!(
            provider.starts.lock().unwrap().as_slice(),
            &[base, base + time::Duration::seconds(2),]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn next_day_start_aligns_to_exchange_midnight_across_dst() {
        let tz = TimeZone::get("America/New_York").unwrap();
        let utc = |month, day, hour, minute| {
            time::Date::from_calendar_date(2026, month, day)
                .unwrap()
                .with_hms(hour, minute, 0)
                .unwrap()
                .assume_utc()
        };
        // Winter (EST, UTC-5): cursor at after-hours close 01:00 UTC Jan 7 (20:00 ET Jan 6) lands
        // on NY date Jan 6, so the next NY midnight is 00:00 EST Jan 7 == 05:00 UTC Jan 7.
        assert_eq!(
            next_day_start(utc(time::Month::January, 7, 1, 0), &tz).unwrap(),
            utc(time::Month::January, 7, 5, 0)
        );
        // Summer (EDT, UTC-4): cursor at 23:59 UTC Jun 16 (19:59 ET Jun 16) -> next NY midnight is
        // 00:00 EDT Jun 17 == 04:00 UTC Jun 17.
        assert_eq!(
            next_day_start(utc(time::Month::June, 16, 23, 59), &tz).unwrap(),
            utc(time::Month::June, 17, 4, 0)
        );
        // A cursor already at a local midnight advances a full local day (never zero).
        let midnight = utc(time::Month::June, 17, 4, 0);
        assert!(next_day_start(midnight, &tz).unwrap() > midnight);
    }

    #[test]
    fn empty_response_advances_to_next_exchange_day_without_skipping_data() {
        // Reproduces the real IBKR data-loss case in WINTER (EST): a US trading day's after-hours
        // runs to 20:00 ET = 01:00 UTC the next day, so after a session the cursor sits at ~01:00
        // UTC. The provider returns empty for that odd start, and the day's data only comes back
        // when the request begins at the exchange day's local midnight (05:00 UTC in EST). A UTC
        // day-jump would land at 00:00 UTC the *following* day, skipping that whole session;
        // advancing to the next New York midnight lands at 05:00 UTC and captures it.
        struct ExchangeDayProvider {
            magic_start: OffsetDateTime,
            data_tick: OffsetDateTime,
            starts: Mutex<Vec<OffsetDateTime>>,
        }

        impl MarketDataProvider for ExchangeDayProvider {
            fn head_timestamp(
                &self,
                _contract: &StockContract,
                _use_rth: bool,
            ) -> Result<OffsetDateTime> {
                unreachable!("configured start avoids head timestamp")
            }

            fn historical_trade_ticks(
                &self,
                _contract: &StockContract,
                start: OffsetDateTime,
                _end: OffsetDateTime,
                _max_ticks: i32,
                _use_rth: bool,
                _cancel: &CancellationToken,
            ) -> Result<Vec<ProviderTick>> {
                self.starts.lock().unwrap().push(start);
                if start == self.magic_start {
                    Ok(vec![ProviderTick {
                        timestamp: self.data_tick,
                        price: 10.0,
                        size: 1,
                    }])
                } else {
                    Ok(Vec::new())
                }
            }
        }

        let tz = TimeZone::get("America/New_York").unwrap();
        let utc = |month, day, hour, minute| {
            time::Date::from_calendar_date(2026, month, day)
                .unwrap()
                .with_hms(hour, minute, 0)
                .unwrap()
                .assume_utc()
        };
        // EST is UTC-5, so New York midnight 2026-01-07 == 05:00 UTC. The cursor sits at the prior
        // session's after-hours close (20:00 ET Jan 6 == 01:00 UTC Jan 7), on New York date Jan 6.
        let cursor = utc(time::Month::January, 7, 1, 0);
        let magic_start = utc(time::Month::January, 7, 5, 0); // NY midnight Jan 7
        let data_tick = utc(time::Month::January, 7, 14, 30); // 09:30 ET regular session
        let end = utc(time::Month::January, 8, 5, 0); // NY midnight Jan 8

        let provider = ExchangeDayProvider {
            magic_start,
            data_tick,
            starts: Mutex::new(Vec::new()),
        };
        let dir = temp_dir("mdfwob-exchange-day-advance");
        let store = TickStore::new(&dir, "MSFT", FwobOptions::default());
        let cancel = CancellationToken::new();
        let pacer = RequestPacer::new(Duration::ZERO);

        SymbolDownloader {
            provider: &provider,
            configured_start: Some(cursor),
            configured_end: Some(end),
            use_rth: false,
            reconnect_timeout_seconds: -1,
            timezone: &tz,
            cancel: &cancel,
            pacer: &pacer,
            retry_pacer: &pacer,
        }
        .download_symbol(&test_stock("MSFT"), &store)
        .unwrap();

        // The session's tick must be captured (not skipped); a UTC-day jump would have skipped it.
        assert_eq!(
            store.last_timestamp().unwrap(),
            Some(data_tick.unix_timestamp() as u32)
        );
        assert!(
            provider.starts.lock().unwrap().contains(&magic_start),
            "a request must begin at the exchange day's local midnight (05:00 UTC)"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mock_download_resumes_from_existing_file() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap();
        let dir = temp_dir("mdfwob-resume");
        let store = TickStore::new(&dir, "MSFT", FwobOptions::default());
        store
            .append_ticks(&[ShortTick::new(base.unix_timestamp() as u32, 100.0, 1).unwrap()])
            .unwrap();

        let provider = MockProvider {
            head: base - time::Duration::days(1),
            batches: Mutex::new(VecDeque::from(vec![vec![ProviderTick {
                timestamp: base + time::Duration::seconds(1),
                price: 100.1,
                size: 2,
            }]])),
            starts: Mutex::new(Vec::new()),
        };
        let contract = StockContract {
            symbol: "MSFT".into(),
            currency: "USD".into(),
            exchange: "SMART".into(),
            primary_exchange: Some("NASDAQ".into()),
            option: None,
        };
        let cancel = CancellationToken::new();
        let pacer = RequestPacer::new(Duration::ZERO);

        SymbolDownloader {
            provider: &provider,
            configured_start: None,
            configured_end: Some(base + time::Duration::seconds(2)),
            use_rth: false,
            reconnect_timeout_seconds: -1,
            timezone: &TimeZone::UTC,
            cancel: &cancel,
            pacer: &pacer,
            retry_pacer: &pacer,
        }
        .download_symbol(&contract, &store)
        .unwrap();

        assert_eq!(
            provider.starts.lock().unwrap().as_slice(),
            &[base + time::Duration::seconds(1)]
        );
        assert_eq!(
            store.last_timestamp().unwrap(),
            Some((base + time::Duration::seconds(1)).unix_timestamp() as u32)
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fixed_end_excludes_ticks_at_and_after_the_cutoff() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_000_150).unwrap();
        let end = base + time::Duration::seconds(1);
        let provider = MockProvider {
            head: base,
            batches: Mutex::new(VecDeque::from(vec![vec![
                ProviderTick {
                    timestamp: base,
                    price: 10.0,
                    size: 1,
                },
                ProviderTick {
                    timestamp: end,
                    price: 11.0,
                    size: 2,
                },
            ]])),
            starts: Mutex::new(Vec::new()),
        };
        let dir = temp_dir("mdfwob-fixed-end");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        let contract = test_stock("AAPL");
        let cancel = CancellationToken::new();
        let pacer = RequestPacer::new(Duration::ZERO);

        SymbolDownloader {
            provider: &provider,
            configured_start: Some(base),
            configured_end: Some(end),
            use_rth: false,
            reconnect_timeout_seconds: -1,
            timezone: &TimeZone::UTC,
            cancel: &cancel,
            pacer: &pacer,
            retry_pacer: &pacer,
        }
        .download_symbol(&contract, &store)
        .unwrap();

        assert_eq!(
            store.last_timestamp().unwrap(),
            Some(base.unix_timestamp() as u32)
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn subsecond_provider_ticks_resume_at_the_next_whole_second() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_000_175).unwrap();
        let provider = MockProvider {
            head: base,
            batches: Mutex::new(VecDeque::from(vec![
                vec![ProviderTick {
                    timestamp: base + time::Duration::milliseconds(500),
                    price: 10.0,
                    size: 1,
                }],
                vec![ProviderTick {
                    timestamp: base + time::Duration::seconds(1),
                    price: 10.1,
                    size: 2,
                }],
            ])),
            starts: Mutex::new(Vec::new()),
        };
        let dir = temp_dir("mdfwob-subsecond-cursor");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        let contract = test_stock("AAPL");
        let cancel = CancellationToken::new();
        let pacer = RequestPacer::new(Duration::ZERO);

        SymbolDownloader {
            provider: &provider,
            configured_start: Some(base),
            configured_end: Some(base + time::Duration::seconds(2)),
            use_rth: false,
            reconnect_timeout_seconds: -1,
            timezone: &TimeZone::UTC,
            cancel: &cancel,
            pacer: &pacer,
            retry_pacer: &pacer,
        }
        .download_symbol(&contract, &store)
        .unwrap();

        assert_eq!(
            provider.starts.lock().unwrap().as_slice(),
            &[base, base + time::Duration::seconds(1)]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancellation_stops_before_next_request() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_000_200).unwrap();
        let provider = MockProvider {
            head: base,
            batches: Mutex::new(VecDeque::from(vec![vec![ProviderTick {
                timestamp: base,
                price: 10.0,
                size: 1,
            }]])),
            starts: Mutex::new(Vec::new()),
        };
        let dir = temp_dir("mdfwob-cancel");
        let store = TickStore::new(&dir, "AAPL", FwobOptions::default());
        let contract = StockContract {
            symbol: "AAPL".into(),
            currency: "USD".into(),
            exchange: "SMART".into(),
            primary_exchange: Some("NASDAQ".into()),
            option: None,
        };
        let cancel = CancellationToken::new();
        cancel.cancel();
        let pacer = RequestPacer::new(Duration::ZERO);

        SymbolDownloader {
            provider: &provider,
            configured_start: Some(base),
            configured_end: Some(base + time::Duration::seconds(10)),
            use_rth: false,
            reconnect_timeout_seconds: -1,
            timezone: &TimeZone::UTC,
            cancel: &cancel,
            pacer: &pacer,
            retry_pacer: &pacer,
        }
        .download_symbol(&contract, &store)
        .unwrap();

        assert!(provider.starts.lock().unwrap().is_empty());
        assert_eq!(store.last_timestamp().unwrap(), None);
        fs::remove_dir_all(dir).unwrap_or(());
    }

    #[test]
    fn worker_pool_downloads_multiple_symbols() {
        struct ConcurrentProvider {
            symbols: Mutex<Vec<String>>,
        }

        impl MarketDataProvider for ConcurrentProvider {
            fn head_timestamp(
                &self,
                _contract: &StockContract,
                _use_rth: bool,
            ) -> Result<OffsetDateTime> {
                unreachable!("configured start avoids head timestamp")
            }

            fn historical_trade_ticks(
                &self,
                contract: &StockContract,
                start: OffsetDateTime,
                _end: OffsetDateTime,
                _max_ticks: i32,
                _use_rth: bool,
                _cancel: &CancellationToken,
            ) -> Result<Vec<ProviderTick>> {
                self.symbols.lock().unwrap().push(contract.symbol.clone());
                Ok(vec![ProviderTick {
                    timestamp: start,
                    price: 10.0,
                    size: 1,
                }])
            }
        }

        let base = OffsetDateTime::from_unix_timestamp(1_700_000_300).unwrap();
        let dir = temp_dir("mdfwob-workers");
        let mut config = Config::default();
        config.download.output_dir = dir.clone();
        config.download.start = Some(
            base.format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        );
        config.download.end = Some(
            (base + time::Duration::seconds(1))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        );
        config.download.parallelism = 2;
        config.download.request_interval_ms = 0;
        config.stocks.push(StockContractConfig {
            symbols: vec!["AAPL".into(), "MSFT".into()],
            currency: "USD".into(),
            exchange: "SMART".into(),
            primary_exchange: Some("NASDAQ".into()),
        });

        let provider = ConcurrentProvider {
            symbols: Mutex::new(Vec::new()),
        };
        let cancel = CancellationToken::new();
        let pacer = RequestPacer::new(Duration::ZERO);
        run_with_provider(
            DownloadPlan::new(config, FwobOptions::default()).unwrap(),
            &provider,
            &cancel,
            &pacer,
            &pacer,
        )
        .unwrap();

        let mut symbols = provider.symbols.lock().unwrap().clone();
        symbols.sort();
        assert_eq!(symbols, ["AAPL", "MSFT"]);
        assert_eq!(
            TickStore::new(&dir, "AAPL", FwobOptions::default())
                .last_timestamp()
                .unwrap(),
            Some(base.unix_timestamp() as u32)
        );
        assert_eq!(
            TickStore::new(&dir, "MSFT", FwobOptions::default())
                .last_timestamp()
                .unwrap(),
            Some(base.unix_timestamp() as u32)
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn one_symbol_failure_does_not_abort_remaining_symbols() {
        struct FlakyProvider;

        impl MarketDataProvider for FlakyProvider {
            fn head_timestamp(
                &self,
                _contract: &StockContract,
                _use_rth: bool,
            ) -> Result<OffsetDateTime> {
                unreachable!("configured start avoids head timestamp")
            }

            fn historical_trade_ticks(
                &self,
                contract: &StockContract,
                start: OffsetDateTime,
                _end: OffsetDateTime,
                _max_ticks: i32,
                _use_rth: bool,
                _cancel: &CancellationToken,
            ) -> Result<Vec<ProviderTick>> {
                if contract.symbol == "BAD" {
                    bail!("simulated provider failure for {}", contract.symbol);
                }
                Ok(vec![ProviderTick {
                    timestamp: start,
                    price: 10.0,
                    size: 1,
                }])
            }
        }

        let base = OffsetDateTime::from_unix_timestamp(1_700_000_400).unwrap();
        let dir = temp_dir("mdfwob-partial-failure");
        let mut config = Config::default();
        config.download.output_dir = dir.clone();
        config.download.start = Some(
            base.format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        );
        config.download.end = Some(
            (base + time::Duration::seconds(1))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        );
        // Single worker so the failing symbol is processed before the good one, proving the run
        // continues past the failure rather than aborting.
        config.download.parallelism = 1;
        config.download.request_interval_ms = 0;
        config.stocks.push(StockContractConfig {
            symbols: vec!["BAD".into(), "GOOD".into()],
            currency: "USD".into(),
            exchange: "SMART".into(),
            primary_exchange: Some("NASDAQ".into()),
        });

        let provider = FlakyProvider;
        let cancel = CancellationToken::new();
        let pacer = RequestPacer::new(Duration::ZERO);
        let result = run_with_provider(
            DownloadPlan::new(config, FwobOptions::default()).unwrap(),
            &provider,
            &cancel,
            &pacer,
            &pacer,
        );

        // The overall run reports the failure, but the good symbol was still downloaded.
        let error = result.unwrap_err();
        assert!(error.to_string().contains("simulated provider failure"));
        assert!(!cancel.is_canceled());
        assert_eq!(
            TickStore::new(&dir, "GOOD", FwobOptions::default())
                .last_timestamp()
                .unwrap(),
            Some(base.unix_timestamp() as u32)
        );
        assert!(!dir.join("BAD.fwob").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn option_contract_maps_to_ibkr_and_unique_output_title() {
        let config = OptionContractConfig {
            symbol: "MSFT".into(),
            expiration: "20260717".into(),
            strike: 450.0,
            right: OptionRight::Call,
            currency: "USD".into(),
            exchange: "SMART".into(),
            multiplier: "100".into(),
            trading_class: Some("MSFT".into()),
            local_symbol: None,
        };
        let contract = option_contract(&config);
        let ibkr = ibkr_contract(&contract);

        assert_eq!(contract.output_title(), "MSFT_20260717_C_450");
        assert_eq!(ibkr.security_type, ibapi::contracts::SecurityType::Option);
        assert_eq!(ibkr.last_trade_date_or_contract_month, "20260717");
        assert_eq!(ibkr.strike, 450.0);
        assert_eq!(ibkr.right, Some(ibapi::contracts::OptionRight::Call));
        assert_eq!(ibkr.multiplier, "100");
        assert_eq!(ibkr.trading_class, "MSFT");
    }

    #[test]
    fn invalid_option_contract_is_rejected() {
        let mut config = Config::default();
        config.options.push(OptionContractConfig {
            symbol: "MSFT".into(),
            expiration: "2026-07-17".into(),
            strike: 450.0,
            right: OptionRight::Put,
            currency: "USD".into(),
            exchange: "SMART".into(),
            multiplier: "100".into(),
            trading_class: None,
            local_symbol: None,
        });

        assert!(DownloadPlan::new(config, FwobOptions::default()).is_err());
    }

    #[test]
    fn duplicate_and_unsafe_output_names_are_rejected() {
        let mut duplicate = Config::default();
        duplicate.stocks.push(StockContractConfig {
            symbols: vec!["AAPL".into(), "aapl".into()],
            ..StockContractConfig::default()
        });
        assert!(
            DownloadPlan::new(duplicate, FwobOptions::default())
                .unwrap_err()
                .to_string()
                .contains("both write")
        );

        let mut unsafe_name = Config::default();
        unsafe_name.stocks.push(StockContractConfig {
            symbols: vec!["../AAPL".into()],
            ..StockContractConfig::default()
        });
        assert!(
            DownloadPlan::new(unsafe_name, FwobOptions::default())
                .unwrap_err()
                .to_string()
                .contains("unsafe FWOB output name")
        );
    }

    #[test]
    fn invalid_plan_settings_are_rejected_before_provider_connection() {
        let mut config = Config::default();
        config.download.parallelism = 0;
        config.stocks.push(StockContractConfig {
            symbols: vec!["AAPL".into()],
            ..StockContractConfig::default()
        });
        assert!(DownloadPlan::new(config, FwobOptions::default()).is_err());

        let mut config = Config::default();
        config.download.start = Some("2025-01-02".into());
        config.download.end = Some("2025-01-01".into());
        config.stocks.push(StockContractConfig {
            symbols: vec!["AAPL".into()],
            ..StockContractConfig::default()
        });
        assert!(DownloadPlan::new(config, FwobOptions::default()).is_err());
    }

    #[test]
    fn malformed_provider_batches_are_rejected() {
        let base = OffsetDateTime::from_unix_timestamp(1_700_000_500).unwrap();
        let before_cursor = [ProviderTick {
            timestamp: base - time::Duration::seconds(1),
            price: 10.0,
            size: 1,
        }];
        assert!(validate_provider_batch(&before_cursor, base, "AAPL").is_err());

        let out_of_order = [
            ProviderTick {
                timestamp: base + time::Duration::seconds(1),
                price: 10.0,
                size: 1,
            },
            ProviderTick {
                timestamp: base,
                price: 10.1,
                size: 1,
            },
        ];
        assert!(validate_provider_batch(&out_of_order, base, "AAPL").is_err());
    }

    #[test]
    fn explicit_offsets_resolve_to_the_same_utc_instant() {
        let utc = parse_time("2024-03-10T14:30:00Z").unwrap();
        let eastern = parse_time("2024-03-10T10:30:00-04:00").unwrap();
        assert_eq!(utc.unix_timestamp(), eastern.unix_timestamp());
    }

    #[test]
    fn repeated_dst_hour_requires_an_explicit_offset() {
        let daylight = parse_time("2024-11-03T01:30:00-04:00").unwrap();
        let standard = parse_time("2024-11-03T01:30:00-05:00").unwrap();
        assert_eq!(standard.unix_timestamp() - daylight.unix_timestamp(), 3600);
        assert!(parse_time("2024-11-03T01:30:00").is_err());
    }

    #[test]
    fn date_only_input_is_midnight_utc() {
        let parsed = parse_time("2024-03-10").unwrap();
        let expected = parse_time("2024-03-10T00:00:00Z").unwrap();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn omitted_end_resolves_to_current_time_dynamically() {
        let before = OffsetDateTime::now_utc();
        let resolved = resolve_end(None);
        let after = OffsetDateTime::now_utc();
        assert!(resolved >= before);
        assert!(resolved <= after);

        let fixed = parse_time("2024-06-03T13:30:00Z").unwrap();
        assert_eq!(resolve_end(Some(fixed)), fixed);
    }

    #[test]
    fn market_timezones_store_the_same_utc_epoch_second() {
        let cases = [
            parse_time("2024-06-03T09:30:00-04:00").unwrap(), // New York
            parse_time("2024-06-03T14:30:00+01:00").unwrap(), // London
            parse_time("2024-06-03T21:30:00+08:00").unwrap(), // Hong Kong
            parse_time("2024-06-03T13:30:00Z").unwrap(),
        ];

        let stored = cases.map(|timestamp| {
            provider_tick_to_short_tick(
                ProviderTick {
                    timestamp,
                    price: 100.0,
                    size: 1,
                },
                "TEST",
            )
            .unwrap()
            .time
        });

        assert!(stored.iter().all(|timestamp| *timestamp == stored[0]));
        assert_eq!(
            stored[0],
            parse_time("2024-06-03T13:30:00Z").unwrap().unix_timestamp() as u32
        );
    }

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nonce}"))
    }

    fn test_stock(symbol: &str) -> StockContract {
        StockContract {
            symbol: symbol.into(),
            currency: "USD".into(),
            exchange: "SMART".into(),
            primary_exchange: Some("NASDAQ".into()),
            option: None,
        }
    }
}
