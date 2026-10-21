//! Read-only chart data from the authenticated Open API companion.
//!
//! Native trendbars are BID prices, not synthetic MID or bid/ask execution bars.
//! See <https://help.ctrader.com/open-api/messages/#protoaogettrendbarsreq>.
use super::*;

mod catalogue;
mod decode;
mod periods;
mod ranges;
#[cfg(test)]
mod tests;

pub use periods::NativePeriod;

/// A native broker BID bar. Volume is the actual broker tick count.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeBar {
    pub time: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InstrumentSummary {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub enabled: Option<bool>,
}

/// Shared authenticated session exposing only reads to chart consumers.
pub struct ChartData {
    broker: OpenApiBroker,
    catalogue: tokio::sync::Mutex<CatalogueCache>,
    metadata: tokio::sync::Mutex<std::collections::HashMap<String, InstrumentMetadata>>,
}

type CatalogueCache = Option<(std::time::Instant, Vec<InstrumentSummary>)>;

fn catalogue_is_fresh(created: std::time::Instant, now: std::time::Instant) -> bool {
    now.saturating_duration_since(created) < std::time::Duration::from_secs(24 * 60 * 60)
}

impl ChartData {
    pub async fn connect(name: &str) -> Result<Self> {
        Self::connect_profile(name, access::Profile::load(name)?).await
    }

    pub async fn connect_config(name: &str, config: Config) -> Result<Self> {
        Self::connect_profile(name, access::Profile::OpenApi(config)).await
    }

    /// Hybrid profiles verify the same broker/login/account binding as trading.
    /// FIX-only profiles are rejected before any network request.
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
                    "cTrader candle history and instrument metadata require Open API OAuth; FIX supplies quotes only"
                )
            }
        };
        Ok(Self {
            broker,
            catalogue: tokio::sync::Mutex::new(None),
            metadata: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    pub fn account_id(&self) -> i64 {
        self.broker.account_id()
    }

    /// Allows callers to retire cached readers after a transport disconnect.
    /// Reconnecting a reader never retries or submits a trading mutation.
    pub fn is_closed(&self) -> bool {
        self.broker.shared.client.is_closed()
    }

    pub async fn catalogue(&self) -> Result<Vec<InstrumentSummary>> {
        let mut cache = self.catalogue.lock().await;
        if let Some((created, symbols)) = &*cache
            && catalogue_is_fresh(*created, std::time::Instant::now())
        {
            return Ok(symbols.clone());
        }
        let symbols = catalogue::read(&self.broker).await?;
        self.metadata.lock().await.clear();
        *cache = Some((std::time::Instant::now(), symbols.clone()));
        Ok(symbols)
    }

    /// Cached until the chart reader reconnects or its catalogue refreshes.
    /// Trading fetches its own current sizing metadata; this cache must not
    /// authorize order volume.
    pub async fn instrument_metadata(&self, instrument: &str) -> Result<InstrumentMetadata> {
        let mut cache = self.metadata.lock().await;
        if let Some(metadata) = cache.get(instrument) {
            return Ok(metadata.clone());
        }
        let metadata = self.broker.instrument_metadata(instrument).await?;
        cache.insert(instrument.into(), metadata.clone());
        Ok(metadata)
    }

    pub async fn quote(&self, instrument: &str) -> Result<Quote> {
        let metadata = self.instrument_metadata(instrument).await?;
        self.broker.shared.quote(metadata.id).await
    }

    /// Native BID bars whose broker opening timestamps fall in `[since, until)`.
    /// Partial current bars may be included; callers decide cache finalization.
    pub async fn bid_bars(
        &self,
        instrument: &str,
        period: NativePeriod,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<NativeBar>> {
        let windows = ranges::timestamp_windows(period, since, until)?;
        if windows.is_empty() {
            return Ok(Vec::new());
        }
        let symbol = self.instrument_metadata(instrument).await?;
        ranges::read(&self.broker, &symbol, period, windows).await
    }
}
