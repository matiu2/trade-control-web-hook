//! Adapt `broker-tradenation`'s inherent methods to `core::broker::Broker`.
//!
//! Upstream owns its own copies of `EntryRequest` / `Direction` / `ResolvedEntry`
//! / `EntryError` / `RiskBudget`, structurally identical to ours. We translate
//! between them at the boundary so the worker dispatch can stay generic over
//! [`Broker`].

use broker_tradenation::TradeNationBroker;
use trade_control_core::broker::{Broker, EntryError, EntryRequest};
use trade_control_core::intent::{Direction, ResolvedEntry, RiskBudget};

pub struct TradeNationAdapter(pub TradeNationBroker);

impl Broker for TradeNationAdapter {
    async fn place_entry(
        &self,
        max_risk_pct: f64,
        max_open_positions: u32,
        req: &EntryRequest<'_>,
    ) -> Result<String, EntryError> {
        check_min_position_size(req.risk, req.min_position_size)?;
        let upstream_req = broker_tradenation::EntryRequest {
            instrument: req.instrument,
            direction: to_upstream_direction(req.direction),
            entry: to_upstream_entry(&req.entry),
            stop_loss: req.stop_loss,
            take_profit: req.take_profit,
            risk: to_upstream_risk(req.risk),
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

fn to_upstream_risk(r: RiskBudget) -> broker_tradenation::RiskBudget {
    match r {
        RiskBudget::Percent(p) => broker_tradenation::RiskBudget::Percent(p),
        RiskBudget::Amount(a) => broker_tradenation::RiskBudget::Amount(a),
        RiskBudget::Units(s) => broker_tradenation::RiskBudget::Units(s),
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

/// Client-side floor for `RiskBudget::Units`. Other risk modes compute
/// units inside the broker (post equity / FX); their floor is the
/// broker's own minimum surfaced as `UnitsBelowMinimum`.
fn check_min_position_size(risk: RiskBudget, min: Option<f64>) -> Result<(), EntryError> {
    match (risk, min) {
        (RiskBudget::Units(s), Some(min)) if s < min => Err(EntryError::UnitsBelowMinimum),
        _ => Ok(()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_below_min_rejects() {
        let err = check_min_position_size(RiskBudget::Units(4.0), Some(5.0)).unwrap_err();
        assert!(matches!(err, EntryError::UnitsBelowMinimum));
    }

    #[test]
    fn units_at_min_passes() {
        // Strictly less-than — equal to the floor is allowed.
        check_min_position_size(RiskBudget::Units(5.0), Some(5.0)).unwrap();
    }

    #[test]
    fn units_above_min_passes() {
        check_min_position_size(RiskBudget::Units(10.0), Some(5.0)).unwrap();
    }

    #[test]
    fn no_floor_means_no_check() {
        check_min_position_size(RiskBudget::Units(0.0001), None).unwrap();
    }

    #[test]
    fn percent_mode_skips_floor() {
        // Floor only applies to explicit Units mode — Percent/Amount
        // get their floor from the broker's own UnitsBelowMinimum
        // after sizing computes a unit count.
        check_min_position_size(RiskBudget::Percent(0.5), Some(100.0)).unwrap();
    }

    #[test]
    fn amount_mode_skips_floor() {
        check_min_position_size(RiskBudget::Amount(1.0), Some(100.0)).unwrap();
    }
}
