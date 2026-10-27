//! Select one account's configured backend without inventing missing FIX data.
use super::*;

pub struct CtraderBroker {
    backend: Backend,
}

enum Backend {
    OpenApi(OpenApiBroker),
    Fix(Box<fix::FixBroker>),
    Hybrid(Box<hybrid::Hybrid>),
}

impl CtraderBroker {
    pub(crate) fn hybrid_backend(&self) -> Result<&hybrid::Hybrid> {
        match &self.backend {
            Backend::Hybrid(broker) => Ok(broker),
            _ => color_eyre::eyre::bail!(
                "this operation requires an identity-verified cTrader hybrid demo account"
            ),
        }
    }

    pub async fn connect(name: &str) -> Result<Self> {
        Self::connect_profile(name, access::Profile::load(name)?).await
    }

    pub async fn connect_config(name: &str, config: Config) -> Result<Self> {
        Self::connect_profile(name, access::Profile::OpenApi(config)).await
    }

    pub async fn connect_profile(name: &str, profile: access::Profile) -> Result<Self> {
        let backend = match profile {
            access::Profile::Hybrid {
                open_api,
                quote,
                trade,
                binding,
            } => Backend::Hybrid(Box::new(
                hybrid::Hybrid::connect(name, open_api, quote, trade, binding).await?,
            )),
            access::Profile::OpenApi(config) => {
                Backend::OpenApi(OpenApiBroker::connect_config(name, config).await?)
            }
            access::Profile::FixTrading { quote, trade } => {
                Backend::Fix(Box::new(fix::FixBroker::connect(name, quote, trade).await?))
            }
            access::Profile::Fix(_) => color_eyre::eyre::bail!(
                "cTrader QUOTE profile has no paired FIX TRADE credentials; configure the named TRADE profile for position and order management"
            ),
        };
        Ok(Self { backend })
    }

    /// Open API's ctidTraderAccountId, unavailable on a FIX connection.
    /// A FIX terminal login is a different identifier.
    pub fn account_id(&self) -> Option<i64> {
        match &self.backend {
            Backend::OpenApi(broker) => Some(broker.account_id()),
            Backend::Fix(_) => None,
            Backend::Hybrid(broker) => Some(broker.data.account_id()),
        }
    }

    /// Prefer the bound Open API catalogue; FIX-only profiles use FIX discovery.
    pub async fn canonical_instrument(&self, instrument: &str) -> Result<String> {
        match &self.backend {
            Backend::OpenApi(broker) => Ok(broker.instrument_metadata(instrument).await?.name),
            Backend::Fix(broker) => broker.canonical_instrument(instrument).await,
            Backend::Hybrid(broker) => Ok(broker.data.instrument_metadata(instrument).await?.name),
        }
    }

    pub async fn instrument_metadata(&self, instrument: &str) -> Result<InstrumentMetadata> {
        match &self.backend {
            Backend::OpenApi(broker) => broker.instrument_metadata(instrument).await,
            Backend::Hybrid(broker) => broker.data.instrument_metadata(instrument).await,
            Backend::Fix(_) => color_eyre::eyre::bail!(
                "cTrader FIX does not supply pip conventions, lot/volume limits or currency metadata; configure Open API for strategy sizing"
            ),
        }
    }

    pub async fn equity(&self) -> Result<f64> {
        match &self.backend {
            Backend::OpenApi(broker) => broker.equity().await,
            Backend::Hybrid(broker) => broker.data.equity().await,
            Backend::Fix(_) => color_eyre::eyre::bail!(
                "cTrader FIX does not supply current account equity; configure Open API for percentage-risk sizing"
            ),
        }
    }
}

macro_rules! dispatch {
    ($self:ident, $method:ident $(, $argument:expr)* $(,)?) => {
        match &$self.backend {
            Backend::OpenApi(broker) => broker.$method($($argument),*).await,
            Backend::Fix(broker) => broker.$method($($argument),*).await,
            Backend::Hybrid(broker) => broker.data.$method($($argument),*).await,
        }
    };
}

macro_rules! manage {
    ($self:ident, $method:ident $(, $argument:expr)* $(,)?) => {
        match &$self.backend {
            Backend::OpenApi(broker) => broker.$method($($argument),*).await,
            Backend::Fix(broker) => broker.$method($($argument),*).await,
            Backend::Hybrid(broker) => broker.management.$method($($argument),*).await,
        }
    };
}

macro_rules! guarded {
    ($self:ident, $method:ident $(, $argument:expr)* $(,)?) => {
        match &$self.backend {
            Backend::OpenApi(broker) => broker.$method($($argument),*).await,
            Backend::Fix(broker) => broker.$method($($argument),*).await,
            Backend::Hybrid(broker) => broker.$method($($argument),*).await,
        }
    };
}

impl Broker for CtraderBroker {
    async fn place_entry(
        &self,
        cap: f64,
        positions: u32,
        req: &EntryRequest<'_>,
    ) -> std::result::Result<Placement, EntryError> {
        guarded!(self, place_entry, cap, positions, req)
    }
    async fn close_positions(&self, instrument: &str) -> CloseOutcome {
        manage!(self, close_positions, instrument)
    }
    async fn cancel_pending_for_instrument(&self, instrument: &str) -> usize {
        manage!(self, cancel_pending_for_instrument, instrument)
    }
    async fn lookup_attempt_state(
        &self,
        instrument: &str,
        order: &str,
        position: Option<&str>,
    ) -> std::result::Result<AttemptState, LookupError> {
        dispatch!(self, lookup_attempt_state, instrument, order, position)
    }
    async fn lookup_trade_exit(
        &self,
        instrument: &str,
        order: &str,
        position: Option<&str>,
    ) -> std::result::Result<Option<trade_control_core::broker_exit::BrokerTradeExit>, LookupError>
    {
        dispatch!(self, lookup_trade_exit, instrument, order, position)
    }
    async fn cancel_order(&self, account: &str, id: &str) -> std::result::Result<(), CancelError> {
        manage!(self, cancel_order, account, id)
    }
    async fn get_quote(&self, instrument: &str) -> std::result::Result<Quote, LookupError> {
        manage!(self, get_quote, instrument)
    }
    async fn list_open_positions(
        &self,
        account: &str,
    ) -> std::result::Result<Vec<OpenPosition>, LookupError> {
        dispatch!(self, list_open_positions, account)
    }
    async fn amend_stop(
        &self,
        account: &str,
        id: &str,
        stop: f64,
    ) -> std::result::Result<(), AmendError> {
        guarded!(self, amend_stop, account, id, stop)
    }
    async fn list_pending_orders(
        &self,
        account: &str,
    ) -> std::result::Result<Vec<PendingOrder>, LookupError> {
        manage!(self, list_pending_orders, account)
    }
    async fn get_candles(
        &self,
        instrument: &str,
        gran: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> std::result::Result<Vec<Candle>, CandleError> {
        dispatch!(self, get_candles, instrument, gran, since, now)
    }
    async fn get_bidask_candles(
        &self,
        instrument: &str,
        gran: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> std::result::Result<Vec<BidAskCandle>, CandleError> {
        dispatch!(self, get_bidask_candles, instrument, gran, since, now)
    }
    async fn fetch_settlement(
        &self,
        instrument: &str,
        since: DateTime<Utc>,
        ids: &[String],
        now: DateTime<Utc>,
    ) -> std::result::Result<Settlement, LookupError> {
        dispatch!(self, fetch_settlement, instrument, since, ids, now)
    }
}

#[cfg(test)]
mod tests;
