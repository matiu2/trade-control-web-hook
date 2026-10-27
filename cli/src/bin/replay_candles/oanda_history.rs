//! Account-specific historical books for live OANDA replay and sub-bar zoom.
use super::granularity::ReplayGranularity;
use candle_cache::{CacheClient, CacheConfig};
use chrono::{DateTime, Utc};
use color_eyre::{Result, eyre::eyre};
use oanda_client::data_source::OandaDataSource;
use tokio::sync::OnceCell;
use trade_control_engine::BidAskCandle;

#[derive(Default)]
pub struct Live {
    cache: OnceCell<CacheClient<OandaDataSource>>,
    worker_url: Option<String>,
    key_file: Option<std::path::PathBuf>,
}
impl Live {
    pub fn with_destination(mut self, url: Option<&str>, key: Option<&std::path::Path>) -> Self {
        self.worker_url = url.map(str::to_owned);
        self.key_file = key.map(std::path::Path::to_path_buf);
        self
    }
    pub async fn pull(
        &self,
        symbol: &str,
        granularity: ReplayGranularity,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<BidAskCandle>> {
        let cache = self
            .cache
            .get_or_try_init(|| async {
                let (default_url, default_key) = crate::oanda_live_profile::destination()?;
                let url = self.worker_url.clone().unwrap_or(default_url);
                let key = self.key_file.clone().unwrap_or(default_key);
                let profile = crate::oanda_live_profile::load(&url, &key).await?;
                let namespace = oanda_client::account_candle_namespace(&profile.account_id);
                let home = std::env::var_os("HOME")
                    .ok_or_else(|| eyre!("HOME required for the live account cache"))?;
                let config = CacheConfig::default().with_cache_dir(
                    std::path::PathBuf::from(home)
                        .join(".cache")
                        .join(namespace),
                );
                CacheClient::new(
                    config,
                    OandaDataSource::with_shared_client(profile.client, profile.account_id),
                )
                .await
                .map_err(color_eyre::Report::from)
            })
            .await?;
        let candles = cache
            .get_candles_range_bid_ask(
                symbol,
                from.fixed_offset(),
                to.fixed_offset(),
                granularity.candle_model(),
            )
            .await?;
        Ok(candles
            .candles
            .iter()
            .map(super::candles::to_engine_candle)
            .collect())
    }
}
