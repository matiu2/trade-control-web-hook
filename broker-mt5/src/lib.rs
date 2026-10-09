//! Named MT5 demo/competition execution and tick-derived mid history.
mod candles;
mod entry;
mod history;
mod mapping;
mod sizing;
#[cfg(test)]
mod tests;

use chrono::{DateTime, Utc};
use color_eyre::Result;
use mt5_data_source::{account_config::AccountConfig, tick_source::TickDataSource, web::trading::{Trader, StopChange}};
use trade_control_core::broker::*;
use trade_control_core::settlement::Settlement;

pub struct Mt5Broker {
    pub(crate) trader: Trader,
    pub(crate) candles: TickDataSource,
    account_name: String,
}
impl Mt5Broker {
    pub async fn connect(name: &str) -> Result<Self> {
        let config = AccountConfig::load(name)?;
        let trader = config.trader().await?;
        let candles = config.candles().await?;
        Ok(Self { trader, candles, account_name: name.into() })
    }
    fn account_matches(&self, name: &str) -> bool {
        // Some existing core call sites leave this blank; the client is already pinned.
        name.is_empty() || name == self.account_name
    }
    async fn recent_history(&self) -> Result<mt5_data_source::web::trading::History> {
        let now = Utc::now().with_nanosecond(0).ok_or_else(|| color_eyre::eyre::eyre!("invalid history time"))?;
        self.trader.history(now - chrono::Duration::days(31), now).await
    }
}
use chrono::Timelike;
impl Broker for Mt5Broker {
    async fn place_entry(&self, cap: f64, positions: u32, req: &EntryRequest<'_>) -> std::result::Result<Placement, EntryError> {
        entry::place(self, cap, positions, req).await
    }
    async fn close_positions(&self, instrument: &str) -> CloseOutcome {
        let Ok(trades) = self.trader.trades().await else { return CloseOutcome::Errored };
        let positions: Vec<_> = trades.positions.iter().filter(|p| p.symbol == instrument).collect();
        if positions.is_empty() { return CloseOutcome::NothingOpen; }
        for p in &positions {
            if let Err(error) = self.trader.close_position(p.ticket).await {
                tracing::error!(ticket=p.ticket, %error, "MT5 close failed; reconcile before retry");
            }
        }
        match self.trader.trades().await {
            Ok(after) if !after.positions.iter().any(|p| p.symbol == instrument) => CloseOutcome::Closed(positions.len()),
            _ => CloseOutcome::Errored,
        }
    }
    async fn cancel_pending_for_instrument(&self, instrument: &str) -> usize {
        let Ok(trades) = self.trader.trades().await else { return 0 };
        let mut cancelled = 0;
        for order in trades.orders.iter().filter(|o| o.symbol == instrument) {
            if self.cancel_order(&self.account_name, &order.ticket.to_string()).await.is_ok() { cancelled += 1; }
        }
        cancelled
    }
    async fn lookup_attempt_state(&self, instrument: &str, order: &str, position: Option<&str>) -> std::result::Result<AttemptState, LookupError> {
        let trades = self.trader.trades().await.map_err(mapping::lookup_error)?;
        let history = self.recent_history().await.map_err(mapping::lookup_error)?;
        let state = history::attempt(instrument, order, position, &trades, &history);
        if state == AttemptState::Unknown { return Err(LookupError::Transient); }
        Ok(state)
    }
    async fn cancel_order(&self, account: &str, id: &str) -> std::result::Result<(), CancelError> {
        if !self.account_matches(account) { return Err(CancelError::Transient); }
        let ticket = id.parse().map_err(|_| CancelError::Transient)?;
        let outcome = self.trader.cancel_order(ticket).await.map_err(|e| { tracing::error!(%e, "MT5 cancel failed"); CancelError::Transient })?;
        if !mapping::mutation_succeeded(&outcome) { return Err(CancelError::Transient); }
        let after = self.trader.trades().await.map_err(|_| CancelError::Transient)?;
        if after.orders.iter().any(|o| o.ticket == ticket) { return Err(CancelError::Transient); }
        Ok(())
    }
    async fn get_quote(&self, instrument: &str) -> std::result::Result<Quote, LookupError> {
        let q = self.trader.quote(instrument).await.map_err(mapping::lookup_error)?;
        Ok(Quote { bid: q.bid, ask: q.ask })
    }
    async fn list_open_positions(&self, account: &str) -> std::result::Result<Vec<OpenPosition>, LookupError> {
        if !self.account_matches(account) { return Err(LookupError::Transient); }
        let trades = self.trader.trades().await.map_err(mapping::lookup_error)?;
        let history = self.recent_history().await.map_err(mapping::lookup_error)?;
        Ok(trades.positions.iter().map(|p| mapping::position(p, &history)).collect())
    }
    async fn amend_stop(&self, account: &str, id: &str, stop: f64) -> std::result::Result<(), AmendError> {
        if !self.account_matches(account) { return Err(AmendError::Transient); }
        let ticket = id.parse().map_err(|_| AmendError::NotFound)?;
        let trades = self.trader.trades().await.map_err(|_| AmendError::Transient)?;
        let outcome = if trades.positions.iter().any(|p| p.ticket == ticket) {
            self.trader.modify_position(ticket, StopChange::Set(stop), StopChange::Keep).await
        } else if trades.orders.iter().any(|o| o.ticket == ticket) {
            self.trader.modify_order(ticket, None, StopChange::Set(stop), StopChange::Keep).await
        } else { return Err(AmendError::NotFound); };
        let outcome = outcome.map_err(|e| { tracing::error!(%e, "MT5 stop amend failed"); AmendError::Transient })?;
        if mapping::mutation_succeeded(&outcome) { Ok(()) } else { Err(AmendError::Transient) }
    }
    async fn list_pending_orders(&self, account: &str) -> std::result::Result<Vec<PendingOrder>, LookupError> {
        if !self.account_matches(account) { return Err(LookupError::Transient); }
        Ok(self.trader.trades().await.map_err(mapping::lookup_error)?.orders.iter().filter_map(mapping::pending).collect())
    }
    async fn get_candles(&self, symbol: &str, gran: Granularity, since: DateTime<Utc>, now: DateTime<Utc>) -> std::result::Result<Vec<Candle>, CandleError> {
        Ok(self.get_bidask_candles(symbol, gran, since, now).await?.iter().map(BidAskCandle::mid).collect())
    }
    async fn get_bidask_candles(&self, symbol: &str, gran: Granularity, since: DateTime<Utc>, now: DateTime<Utc>) -> std::result::Result<Vec<BidAskCandle>, CandleError> {
        candles::read(&self.candles, symbol, gran, since, now).await
    }
    async fn fetch_settlement(&self, instrument: &str, since: DateTime<Utc>, ids: &[String], now: DateTime<Utc>) -> std::result::Result<Settlement, LookupError> {
        let end = now.with_nanosecond(0).ok_or(LookupError::Transient)?;
        let start = since.with_nanosecond(0).ok_or(LookupError::Transient)?.max(end - chrono::Duration::days(31));
        if start >= end { return Err(LookupError::Transient); }
        let account = self.trader.account().await.map_err(mapping::lookup_error)?;
        let history = self.trader.history(start, end).await.map_err(mapping::lookup_error)?;
        let trades = self.trader.trades().await.map_err(mapping::lookup_error)?;
        let mut settlement = history::settlement(instrument, ids, &trades, &history, &account.currency, now);
        if start > since { settlement.warnings.push("MT5 settlement history limited to the last 31 days".into()); }
        Ok(settlement)
    }
}
