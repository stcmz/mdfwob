mod databento;
mod ibkr;

use anyhow::Result;
use time::OffsetDateTime;

use crate::downloader::{CancellationToken, ProviderTick, StockContract};

pub use databento::DatabentoProvider;
pub use ibkr::IbkrProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    Retry,
    Reconnect { generation: u64 },
}

pub trait MarketDataProvider: Sync {
    fn head_timestamp(&self, contract: &StockContract, use_rth: bool) -> Result<OffsetDateTime>;

    /// Returns trades in nondecreasing timestamp order, all at or after `start`.
    ///
    /// When a provider returns a partial result, it must include every trade through the final
    /// returned UTC second. The downloader resumes at the following whole second because the tick
    /// schema stores second-resolution timestamps.
    ///
    /// `cancel` lets a long/blocked request abort promptly so Ctrl+C is honored without waiting
    /// for the request to complete. A provider that observes cancellation should return an error
    /// (the downloader treats any error as a clean stop while `cancel` is set).
    fn historical_trade_ticks(
        &self,
        contract: &StockContract,
        start: OffsetDateTime,
        end: OffsetDateTime,
        max_ticks: i32,
        use_rth: bool,
        cancel: &CancellationToken,
    ) -> Result<Vec<ProviderTick>>;

    fn recovery_action(&self, _error: &anyhow::Error) -> Option<RecoveryAction> {
        None
    }

    fn reconnect(&self, _generation: u64) -> Result<()> {
        anyhow::bail!("provider does not support reconnecting")
    }
}
