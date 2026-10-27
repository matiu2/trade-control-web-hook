//! Existing Open API implementation for protected entries and account data.
use super::*;

pub(crate) struct OpenApiBroker {
    pub(crate) shared: std::sync::Arc<session::Shared>,
    pub(crate) account_name: String,
}
impl OpenApiBroker {
    pub async fn connect_config(name: &str, config: Config) -> Result<Self> {
        let broker = Self {
            shared: session::acquire(config).await?,
            account_name: name.into(),
        };
        crate::protection::start_monitor(&broker)?;
        Ok(broker)
    }
    pub fn account_id(&self) -> i64 {
        self.shared.client.account_id()
    }
    pub(crate) fn account_matches(&self, account: &str) -> bool {
        account.is_empty() || account == self.account_name
    }
    pub(crate) async fn read(
        &self,
        request: u32,
        response: u32,
        mut payload: Value,
    ) -> Result<Value> {
        payload["ctidTraderAccountId"] = json!(self.account_id());
        let result = self
            .shared
            .client
            .request(request, response, payload)
            .await?;
        ensure!(
            wire::integer(&result, "ctidTraderAccountId")? == self.account_id(),
            "cTrader returned another account"
        );
        Ok(result)
    }
    pub(crate) async fn snapshot(&self) -> Result<Value> {
        let snapshot = self
            .read(2124, 2125, json!({"returnProtectionOrders":false}))
            .await?;
        wire::validate_snapshot(&snapshot)?;
        Ok(snapshot)
    }
    pub(crate) async fn mutate(
        &self,
        request: u32,
        mut payload: Value,
    ) -> std::result::Result<Value, ctrader_api::RequestError> {
        payload["ctidTraderAccountId"] = json!(self.account_id());
        let result = self.shared.client.request(request, 2126, payload).await?;
        if wire::integer(&result, "ctidTraderAccountId").ok() != Some(self.account_id()) {
            return Err(ctrader_api::RequestError::Unknown(
                "execution response belongs to another account".into(),
            ));
        }
        Ok(result)
    }
    pub async fn instrument_metadata(&self, instrument: &str) -> Result<InstrumentMetadata> {
        metadata::instrument(self, instrument).await
    }
    pub async fn equity(&self) -> Result<f64> {
        metadata::equity(self).await
    }
}

impl Broker for OpenApiBroker {
    async fn place_entry(
        &self,
        cap: f64,
        positions: u32,
        req: &EntryRequest<'_>,
    ) -> std::result::Result<Placement, EntryError> {
        entry::place(self, cap, positions, req).await
    }
    async fn close_positions(&self, instrument: &str) -> CloseOutcome {
        management::close(self, instrument).await
    }
    async fn cancel_pending_for_instrument(&self, instrument: &str) -> usize {
        management::cancel_instrument(self, instrument).await
    }
    async fn lookup_attempt_state(
        &self,
        instrument: &str,
        order: &str,
        position: Option<&str>,
    ) -> std::result::Result<AttemptState, LookupError> {
        history::attempt(self, instrument, order, position)
            .await
            .map_err(wire::lookup)
    }
    async fn lookup_trade_exit(
        &self,
        instrument: &str,
        order: &str,
        position: Option<&str>,
    ) -> std::result::Result<Option<trade_control_core::broker_exit::BrokerTradeExit>, LookupError>
    {
        history::trade_exit(self, instrument, order, position)
            .await
            .map_err(wire::lookup)
    }
    async fn cancel_order(&self, account: &str, id: &str) -> std::result::Result<(), CancelError> {
        management::cancel(self, account, id)
            .await
            .map_err(|error| {
                tracing::warn!(%error,"cTrader cancel not confirmed; reconcile before retry");
                CancelError::Transient
            })
    }
    async fn get_quote(&self, instrument: &str) -> std::result::Result<Quote, LookupError> {
        metadata::quote(self, instrument)
            .await
            .map_err(wire::lookup)
    }
    async fn list_open_positions(
        &self,
        account: &str,
    ) -> std::result::Result<Vec<OpenPosition>, LookupError> {
        management::positions(self, account)
            .await
            .map_err(wire::lookup)
    }
    async fn amend_stop(
        &self,
        account: &str,
        id: &str,
        stop: f64,
    ) -> std::result::Result<(), AmendError> {
        management::amend(self, account, id, stop).await
    }
    async fn list_pending_orders(
        &self,
        account: &str,
    ) -> std::result::Result<Vec<PendingOrder>, LookupError> {
        management::pending(self, account)
            .await
            .map_err(wire::lookup)
    }
    async fn get_candles(
        &self,
        instrument: &str,
        gran: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> std::result::Result<Vec<Candle>, CandleError> {
        Ok(self
            .get_bidask_candles(instrument, gran, since, now)
            .await?
            .iter()
            .map(BidAskCandle::mid)
            .collect())
    }
    async fn get_bidask_candles(
        &self,
        instrument: &str,
        gran: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> std::result::Result<Vec<BidAskCandle>, CandleError> {
        candles::read(self, instrument, gran, since, now).await
    }
    async fn fetch_settlement(
        &self,
        instrument: &str,
        since: DateTime<Utc>,
        ids: &[String],
        now: DateTime<Utc>,
    ) -> std::result::Result<Settlement, LookupError> {
        history::settlement(self, instrument, since, ids, now)
            .await
            .map_err(wire::lookup)
    }
}
