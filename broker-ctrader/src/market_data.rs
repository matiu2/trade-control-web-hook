//! Read-only views reuse the account binding and shared cache used by trading.
use super::*;
use std::sync::Arc;

mod forex;
mod identity;
pub use identity::AccountIdentity;

pub use ctrader_data_source::{InstrumentSummary, NativeBar, NativePeriod};

pub struct ChartData {
    broker: OpenApiBroker,
}
impl ChartData {
    /// Build a cache reader without logging in. A miss performs the same
    /// authenticated account binding as the trading facade before reading data.
    pub fn configured_feed(name: &str) -> Result<candle_cache::ctrader::CachedFeed> {
        Self::configured_profile_feed(name, access::Profile::load(name)?)
    }
    pub fn configured_profile_feed(
        name: &str,
        profile: access::Profile,
    ) -> Result<candle_cache::ctrader::CachedFeed> {
        let config = match &profile {
            access::Profile::OpenApi(config)
            | access::Profile::Hybrid {
                open_api: config, ..
            } => config.clone(),
            _ => color_eyre::eyre::bail!(
                "cTrader historical candles require Open API OAuth; FIX supplies quotes only"
            ),
        };
        config.validate()?;
        profile.validate(
            matches!(config.environment, ctrader_api::Environment::Live),
            None,
        )?;
        let name = name.to_string();
        let source = ctrader_data_source::CtraderDataSource::with_connector(config, move || {
            let name = name.clone();
            let profile = profile.clone();
            async move {
                Ok(Self::connect_profile(&name, profile)
                    .await?
                    .broker
                    .shared
                    .client
                    .clone())
            }
        });
        Ok(candle_cache::ctrader::CachedFeed::new(source))
    }
    pub async fn connect(name: &str) -> Result<Self> {
        Self::connect_profile(name, access::Profile::load(name)?).await
    }
    pub async fn connect_config(name: &str, config: Config) -> Result<Self> {
        Self::connect_profile(name, access::Profile::OpenApi(config)).await
    }
    /// Binding checks run before either native or paired history is exposed.
    pub async fn connect_profile(name: &str, profile: access::Profile) -> Result<Self> {
        let broker = match profile {
            access::Profile::OpenApi(config) => OpenApiBroker::connect_config(name, config).await?,
            access::Profile::Hybrid {
                open_api,
                quote,
                trade,
                binding,
            } => {
                hybrid::Hybrid::connect(name, open_api, quote, trade, binding)
                    .await?
                    .data
            }
            access::Profile::Fix(_) | access::Profile::FixTrading { .. } => {
                color_eyre::eyre::bail!(
                    "cTrader historical candles require Open API OAuth; FIX supplies quotes only"
                )
            }
        };
        Ok(Self { broker })
    }
    pub fn account_id(&self) -> i64 {
        self.broker.account_id()
    }
    pub async fn account_identity(&self) -> Result<AccountIdentity> {
        identity::decode(
            &metadata::trader(&self.broker).await?,
            self.account_id(),
            self.broker.shared.client.environment(),
        )
    }
    pub fn is_closed(&self) -> bool {
        self.broker.shared.client.is_closed()
    }
    pub async fn cached_feed(&self) -> Result<Arc<candle_cache::ctrader::CachedFeed>> {
        self.broker.shared.candles().await
    }
    pub async fn catalogue(&self) -> Result<Vec<InstrumentSummary>> {
        self.cached_feed().await?.source().catalogue().await
    }
    pub async fn instrument_metadata(&self, instrument: &str) -> Result<InstrumentMetadata> {
        self.broker.instrument_metadata(instrument).await
    }
    /// Match the recognized broker currency legs supported by strategy sizing.
    pub async fn forex_instrument_metadata(&self, instrument: &str) -> Result<InstrumentMetadata> {
        let symbol = self.instrument_metadata(instrument).await?;
        let assets = metadata::assets(&self.broker).await?;
        forex::validate(symbol.base_asset, symbol.quote_asset, &assets)?;
        Ok(symbol)
    }
    pub async fn quote(&self, instrument: &str) -> Result<Quote> {
        let id = self
            .cached_feed()
            .await?
            .source()
            .instrument_metadata(instrument)
            .await?
            .id;
        self.broker.shared.quote(id).await
    }
    /// Raw reads remain available to import tools; chart/engine use the cache.
    pub async fn bid_bars(
        &self,
        instrument: &str,
        period: NativePeriod,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<NativeBar>> {
        self.cached_feed()
            .await?
            .source()
            .bid_bars(instrument, period, since, until)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_cache_does_not_authenticate_until_a_cache_miss() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
        let config = ctrader_api::Config {
            client_id: "not-active".into(),
            client_secret: "private".into(),
            access_token: "not-granted".into(),
            account_id: 42,
            environment: ctrader_api::Environment::Demo,
        };
        let feed =
            ChartData::configured_profile_feed("personal-demo", access::Profile::OpenApi(config))
                .unwrap();
        assert_eq!(feed.source().account_id(), 42);
    }
    #[tokio::test]
    async fn fix_profiles_cannot_open_historical_readers() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
        let config = ctrader_fix::Config {
            host: "unused".into(),
            port: 5211,
            sender_comp_id: "demo.example.1".into(),
            target_comp_id: "cServer".into(),
            sender_sub_id: "QUOTE".into(),
            password: "private".into(),
        };
        assert!(
            ChartData::connect_profile("demo", access::Profile::Fix(config))
                .await
                .is_err()
        );
    }
}
