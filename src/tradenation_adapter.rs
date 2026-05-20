//! Adapt `broker-tradenation`'s inherent methods to `core::broker::Broker`.
//!
//! Upstream owns its own copies of `EntryRequest` / `Direction` / `ResolvedEntry`
//! / `EntryError`, structurally identical to ours. We translate between them at
//! the boundary so the worker dispatch can stay generic over [`Broker`].

use broker_tradenation::TradeNationBroker;
use trade_control_core::broker::{Broker, EntryError, EntryRequest};
use trade_control_core::intent::{Direction, ResolvedEntry, RiskBudget};
use worker::console_error;

pub struct TradeNationAdapter(pub TradeNationBroker);

impl Broker for TradeNationAdapter {
    async fn place_entry(
        &self,
        max_risk_pct: f64,
        max_open_positions: u32,
        req: &EntryRequest<'_>,
    ) -> Result<String, EntryError> {
        // Upstream's `EntryRequest` only carries `risk_pct`. Threading
        // `RiskBudget::Amount` through would mean bumping
        // `broker-tradenation` (which fetches equity inside its own
        // `place_entry`). Reject for now with a clear log — `dry_run`
        // still works for amount-mode intents on TN since dispatch
        // short-circuits before calling the broker.
        let risk_pct = match req.risk {
            RiskBudget::Percent(pct) => pct,
            RiskBudget::Amount(_) => {
                console_error!(
                    "tradenation adapter: risk_amount not yet supported on TN — \
                     use risk_pct, or wait for the broker-tradenation bump"
                );
                return Err(EntryError::OrderRejected);
            }
            RiskBudget::Units(_) => {
                console_error!(
                    "tradenation adapter: size_units not yet supported on TN — \
                     use risk_pct, or wait for the broker-tradenation bump"
                );
                return Err(EntryError::OrderRejected);
            }
        };
        let upstream_req = broker_tradenation::EntryRequest {
            instrument: req.instrument,
            direction: to_upstream_direction(req.direction),
            entry: to_upstream_entry(&req.entry),
            stop_loss: req.stop_loss,
            take_profit: req.take_profit,
            risk_pct,
        };
        self.0
            .place_entry(max_risk_pct, max_open_positions, &upstream_req)
            .await
            .map_err(from_upstream_error)
    }

    async fn close_positions(&self, instrument: &str) -> bool {
        self.0.close_positions(instrument).await
    }

    async fn cancel_pending_for_instrument(&self, instrument: &str) -> usize {
        self.0.cancel_pending_for_instrument(instrument).await
    }
}

fn to_upstream_direction(d: Direction) -> broker_tradenation::Direction {
    match d {
        Direction::Long => broker_tradenation::Direction::Long,
        Direction::Short => broker_tradenation::Direction::Short,
    }
}

fn to_upstream_entry(e: &ResolvedEntry) -> broker_tradenation::ResolvedEntry {
    match e {
        // Upstream re-fetches the live bid/ask when placing a market order, so
        // `reference_price` is not threaded through — it only matters to the
        // OANDA risk math on the other side.
        ResolvedEntry::Market { .. } => broker_tradenation::ResolvedEntry::Market,
        ResolvedEntry::Stop { trigger_price } => broker_tradenation::ResolvedEntry::Stop {
            price: *trigger_price,
        },
        ResolvedEntry::Limit { trigger_price } => broker_tradenation::ResolvedEntry::Limit {
            price: *trigger_price,
        },
    }
}

fn from_upstream_error(e: broker_tradenation::EntryError) -> EntryError {
    use broker_tradenation::EntryError as U;
    match e {
        U::AccountFetch => EntryError::AccountFetch,
        U::EquityParse => EntryError::EquityParse,
        U::RiskCapExceeded { requested, cap } => EntryError::RiskCapExceeded { requested, cap },
        U::OpenPositionsCapExceeded => EntryError::OpenPositionsCapExceeded,
        U::UnitsBelowMinimum => EntryError::UnitsBelowMinimum,
        U::OrderRejected => EntryError::OrderRejected,
    }
}
