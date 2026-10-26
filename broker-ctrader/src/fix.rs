//! Management-only FIX adapter. Strategy entries require Open API risk data and protection.
use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, ensure, eyre};
use ctrader_fix::{
    Config, TradeConfig,
    market_data::Symbol,
    trading::{Client, ExecutionReport, Position},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex, OnceLock, Weak},
};
use tokio::sync::Mutex;
use trade_control_core::{broker::*, settlement::Settlement};

mod evidence;
mod management;
mod mapping;
pub(crate) mod reconciliation;
#[cfg(test)]
mod tests;

pub(crate) struct FixBroker {
    name: String,
    quote: Config,
    trade: TradeConfig,
    gate: Arc<Mutex<()>>,
}
/// A durable, account-wide interlock held across a companion Open API write.
pub(crate) struct CompanionWrite {
    _gate: tokio::sync::OwnedMutexGuard<()>,
    _lock: evidence::AccountLock,
    mutation: evidence::Mutation,
    recorded: bool,
}
impl CompanionWrite {
    pub(crate) fn id(&self) -> &str {
        self.mutation.client_order_id()
    }
    pub(crate) fn note(&mut self, phase: &str, evidence: &serde_json::Value) -> Result<()> {
        ensure!(
            !self.recorded,
            "cTrader companion outcome was already recorded"
        );
        self.mutation.record(phase, evidence)
    }
    pub(crate) fn record(&mut self, evidence: &serde_json::Value, confirmed: bool) -> Result<()> {
        ensure!(
            !self.recorded,
            "cTrader companion mutation outcome was already recorded; do not replay"
        );
        self.recorded = true;
        self.mutation.record("outcome", evidence)?;
        if confirmed {
            self.mutation.resolve(evidence)?;
        }
        Ok(())
    }
}
type Gates = HashMap<(String, String), Weak<Mutex<()>>>;
static GATES: OnceLock<StdMutex<Gates>> = OnceLock::new();
fn account_gate(quote: &Config) -> Result<Arc<Mutex<()>>> {
    let mut gates = GATES
        .get_or_init(|| StdMutex::new(HashMap::new()))
        .lock()
        .map_err(|_| eyre!("cTrader FIX account gate is poisoned"))?;
    gates.retain(|_, gate| gate.strong_count() > 0);
    let key = (quote.sender_comp_id.clone(), quote.target_comp_id.clone());
    if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
        return Ok(gate);
    }
    let gate = Arc::new(Mutex::new(()));
    gates.insert(key, Arc::downgrade(&gate));
    Ok(gate)
}
fn validate_pair(quote: &Config, trade: &TradeConfig) -> Result<()> {
    quote.validate()?;
    trade.validate()?;
    ensure!(
        quote.sender_comp_id == trade.sender_comp_id
            && quote.target_comp_id == trade.target_comp_id,
        "cTrader FIX QUOTE and TRADE profiles identify different accounts"
    );
    ensure!(
        trade.sender_comp_id.starts_with("demo."),
        "cTrader FIX management currently requires a demo account"
    );
    ensure!(
        trade.username()?.parse::<u64>()? > 0,
        "cTrader FIX account login must be positive"
    );
    Ok(())
}
impl FixBroker {
    pub(crate) async fn connect(name: &str, quote: Config, trade: TradeConfig) -> Result<Self> {
        validate_pair(&quote, &trade)?;
        Ok(Self {
            name: name.into(),
            gate: account_gate(&quote)?,
            quote,
            trade,
        })
    }
    pub(crate) async fn companion_write(
        &self,
        operation: &str,
        intent: serde_json::Value,
    ) -> Result<CompanionWrite> {
        self.companion_write_at(operation, intent, None).await
    }
    async fn companion_write_at(
        &self,
        operation: &str,
        mut intent: serde_json::Value,
        audit_root: Option<&std::path::Path>,
    ) -> Result<CompanionWrite> {
        ensure!(
            intent.is_object(),
            "cTrader companion mutation intent must be an object; no mutation sent"
        );
        let gate = self.gate.clone().lock_owned().await;
        let lock = match audit_root {
            Some(root) => evidence::AccountLock::acquire_at(self, root)?,
            None => evidence::AccountLock::acquire(self)?,
        };
        intent["operation"] = serde_json::json!(operation);
        let mutation = evidence::Mutation::prepare(self, &lock, evidence::Mutation::id(), intent)?;
        Ok(CompanionWrite {
            _gate: gate,
            _lock: lock,
            mutation,
            recorded: false,
        })
    }
    fn account_matches(&self, account: &str) -> Result<()> {
        ensure!(
            account.is_empty() || account == self.name,
            "cTrader FIX account mismatch"
        );
        Ok(())
    }
    async fn symbols_unlocked(&self) -> Result<Vec<Symbol>> {
        let mut client = ctrader_fix::market_data::Client::connect(self.quote.clone()).await?;
        let result = client.symbols().await;
        let logout = client.logout().await;
        result.and_then(|symbols| logout.map(|()| symbols))
    }
    pub(crate) async fn symbols(&self) -> Result<Vec<Symbol>> {
        let _guard = self.gate.lock().await;
        self.symbols_unlocked().await
    }
    pub(crate) async fn canonical_instrument(&self, name: &str) -> Result<String> {
        Ok(mapping::resolve(&self.symbols().await?, name)?.name.clone())
    }
    async fn finish_read<T>(&self, client: &mut Client, result: Result<T>) -> Result<T> {
        let logout = client.logout().await;
        let events = client.take_events();
        if !events.is_empty() {
            tracing::info!(account=%self.name,evidence=%serde_json::json!(events),"cTrader FIX unsolicited read-session executions");
        }
        result.and_then(|value| logout.map(|()| value))
    }
}
fn lookup(error: color_eyre::Report) -> LookupError {
    tracing::warn!(%error,"cTrader FIX read unavailable");
    LookupError::Transient
}
impl Broker for FixBroker {
    async fn place_entry(
        &self,
        _cap: f64,
        _positions: u32,
        _req: &EntryRequest<'_>,
    ) -> std::result::Result<Placement, EntryError> {
        Err(EntryError::OrderRejectedWithDetail("cTrader FIX cannot open strategy entries: account equity, currency conversion and symbol volume limits are unavailable, and native stop-loss/take-profit protection is unsupported; configure Open API".into()))
    }
    async fn close_positions(&self, instrument: &str) -> CloseOutcome {
        match self.close(instrument).await {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(account=%self.name,instrument,%error,"cTrader FIX close not confirmed; reconcile before retry");
                CloseOutcome::Errored
            }
        }
    }
    async fn cancel_pending_for_instrument(&self, instrument: &str) -> usize {
        match self.cancel_instrument(instrument).await {
            Ok(count) => count,
            Err(error) => {
                tracing::error!(account=%self.name,instrument,%error,"cTrader FIX cancellations stopped; reconcile before retry");
                0
            }
        }
    }
    async fn lookup_attempt_state(
        &self,
        instrument: &str,
        order: &str,
        position: Option<&str>,
    ) -> std::result::Result<AttemptState, LookupError> {
        self.attempt(instrument, order, position)
            .await
            .map_err(lookup)
    }
    async fn cancel_order(&self, account: &str, id: &str) -> std::result::Result<(), CancelError> {
        self.cancel(account,id).await.map_err(|error|{tracing::warn!(account=%self.name,id,%error,"cTrader FIX cancellation not confirmed; reconcile before retry");CancelError::Transient})
    }
    async fn get_quote(&self, instrument: &str) -> std::result::Result<Quote, LookupError> {
        self.live_quote(instrument).await.map_err(lookup)
    }
    async fn list_open_positions(
        &self,
        account: &str,
    ) -> std::result::Result<Vec<OpenPosition>, LookupError> {
        self.positions(account).await.map_err(lookup)
    }
    async fn amend_stop(
        &self,
        _account: &str,
        _id: &str,
        _stop: f64,
    ) -> std::result::Result<(), AmendError> {
        tracing::error!(account=%self.name,"cTrader FIX cannot amend native protection; configure Open API; no request submitted");
        Err(AmendError::Transient)
    }
    async fn list_pending_orders(
        &self,
        account: &str,
    ) -> std::result::Result<Vec<PendingOrder>, LookupError> {
        self.pending(account).await.map_err(lookup)
    }
    async fn lookup_trade_exit(
        &self,
        _instrument: &str,
        _order: &str,
        _position: Option<&str>,
    ) -> std::result::Result<Option<trade_control_core::broker_exit::BrokerTradeExit>, LookupError>
    {
        Err(lookup(eyre!(
            "cTrader FIX does not provide complete closing-deal history; configure Open API"
        )))
    }
    async fn get_candles(
        &self,
        _instrument: &str,
        _gran: Granularity,
        _since: DateTime<Utc>,
        _now: DateTime<Utc>,
    ) -> std::result::Result<Vec<Candle>, CandleError> {
        Err(CandleError::Unavailable(
            "cTrader FIX does not provide historical candles; configure Open API".into(),
        ))
    }
    async fn get_bidask_candles(
        &self,
        _instrument: &str,
        _gran: Granularity,
        _since: DateTime<Utc>,
        _now: DateTime<Utc>,
    ) -> std::result::Result<Vec<BidAskCandle>, CandleError> {
        Err(CandleError::Unavailable(
            "cTrader FIX does not provide historical candles; configure Open API".into(),
        ))
    }
    async fn fetch_settlement(
        &self,
        _instrument: &str,
        _since: DateTime<Utc>,
        _ids: &[String],
        _now: DateTime<Utc>,
    ) -> std::result::Result<Settlement, LookupError> {
        Err(lookup(eyre!(
            "cTrader FIX does not provide settlement ledger history; configure Open API"
        )))
    }
}
