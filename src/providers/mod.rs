mod databento;
mod ibkr;

use anyhow::Result;
use time::OffsetDateTime;

use crate::downloader::{ProviderTick, StockContract};

pub use databento::DatabentoProvider;
pub use ibkr::IbkrProvider;

pub trait MarketDataProvider: Sync {
    fn head_timestamp(&self, contract: &StockContract, use_rth: bool) -> Result<OffsetDateTime>;

    /// Returns trades in nondecreasing timestamp order, all at or after `start`.
    ///
    /// When a provider returns a partial result, it must include every trade through the final
    /// returned UTC second. The downloader resumes at the following whole second because FWOB's
    /// legacy `ShortTick` schema stores second-resolution timestamps.
    fn historical_trade_ticks(
        &self,
        contract: &StockContract,
        start: OffsetDateTime,
        end: OffsetDateTime,
        max_ticks: i32,
        use_rth: bool,
    ) -> Result<Vec<ProviderTick>>;
}
