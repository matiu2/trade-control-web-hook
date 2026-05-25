//! Adapt `broker-tradenation`'s inherent methods to `core::broker::Broker`.
//!
//! Upstream owns its own copies of `EntryRequest` / `Direction` / `ResolvedEntry`
//! / `EntryError` / `RiskBudget`, structurally identical to ours. We translate
//! between them at the boundary so the worker dispatch can stay generic over
//! [`Broker`].

use broker_tradenation::TradeNationBroker;
use trade_control_core::broker::{
    AttemptState, Broker, CancelError, EntryError, EntryRequest, LookupError,
};
use trade_control_core::intent::{Direction, ResolvedEntry, RiskBudget};
use tradenation_api::{OpeningOrder, Position, TransactionRecord};
use worker::console_error;
use worker::console_log;

/// Closed-trade scan window. Plan §3 recommends ~50; one TN page
/// returns ~50 records so we fetch a single page. **Caveat: TN's
/// `TransactionRecord` exposes only `RefID`, not the originating
/// `OrderID` or `PositionID` — so step 3 of the algorithm cannot
/// match on TradeNation in v1. Closed attempts fall through to
/// `Cancelled` instead. See report-back note in the 1b commit.
const CLOSED_TRADE_HISTORY_DAYS: u32 = 90;

pub struct TradeNationAdapter(pub TradeNationBroker);

impl Broker for TradeNationAdapter {
    async fn place_entry(
        &self,
        max_risk_pct: f64,
        max_open_positions: u32,
        req: &EntryRequest<'_>,
    ) -> Result<String, EntryError> {
        if req.dry_run {
            // Upstream `place_entry` doesn't yet support a no-op mode,
            // so we can't run the full sizing path (FX, market resolve,
            // stake calc) without risking an actual order. Log the
            // inputs and bail. Once upstream gains a `dry_run` flag,
            // switch this to the same pattern as OANDA.
            console_log!(
                "DRY-RUN tradenation: instrument={} direction={:?} entry={:?} sl={} tp={} risk={:?} (stake not computed — upstream lacks dry-run support)",
                req.instrument,
                req.direction,
                req.entry,
                req.stop_loss,
                req.take_profit,
                req.risk,
            );
            return Ok(format!("dry-run-{}", req.instrument));
        }
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

    async fn lookup_attempt_state(
        &self,
        instrument: &str,
        broker_order_id: &str,
        broker_trade_id: Option<&str>,
    ) -> Result<AttemptState, LookupError> {
        // `get_account_details` returns BOTH pending orders and open
        // positions in one round-trip, so steps 1 and 2 share a fetch.
        let details = tradenation_api::get_account_details(self.0.session())
            .await
            .map_err(|err| {
                console_error!("tn lookup get_account_details: {err:?}");
                LookupError::Transient
            })?;

        // Step 3's fetch is only needed if we already snapshotted a
        // broker_trade_id — same optimisation as the OANDA path.
        let closed: Option<Vec<TransactionRecord>> = if broker_trade_id.is_some() {
            let recs = tradenation_api::get_transaction_history(
                self.0.client(),
                self.0.session(),
                CLOSED_TRADE_HISTORY_DAYS,
                0,
            )
            .await
            .map_err(|err| {
                console_error!("tn lookup get_transaction_history: {err:?}");
                LookupError::Transient
            })?;
            Some(recs)
        } else {
            None
        };

        Ok(compute_attempt_state(
            instrument,
            broker_order_id,
            broker_trade_id,
            &details.opening_orders.records,
            &details.positions.records,
            closed.as_deref(),
        ))
    }

    async fn cancel_order(
        &self,
        _account_id: &str,
        broker_order_id: &str,
    ) -> Result<(), CancelError> {
        // TradeNation picks the account from the session, so the
        // trait-level `account_id` is intentionally ignored.
        tradenation_api::cancel_order(self.0.client(), self.0.session(), broker_order_id)
            .await
            .map_err(|err| {
                console_error!("tn cancel_order({broker_order_id}): {err:?}");
                CancelError::Transient
            })
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

/// Pure helper running plan §3's four-step algorithm against
/// pre-fetched TradeNation payloads. Split out from the trait impl
/// so unit tests can cover every branch without hitting the network.
///
/// - Step 1 matches `OpeningOrder.order_id` against `broker_order_id`.
/// - Step 2 matches `Position.order_id` (the originating order id
///   correlation) — **not** `Position.position_id`. On TN those
///   differ; the worker stored the order id on `EntryAttempt` so
///   that's what we look up by, and we return `position_id` as the
///   `broker_trade_id` so future closed-trade lookups can correlate.
/// - Step 3 matches `TransactionRecord.ref_id` against the stored
///   `broker_trade_id`. **Will never match in v1** — TN's transaction
///   history exposes `RefID`, not `PositionID`. Closed TN trades
///   fall through to `Cancelled`. Live with it for v1.
/// - Step 4 returns `Cancelled` when we had a snapshot,
///   `Unknown` when we didn't.
fn compute_attempt_state(
    instrument: &str,
    broker_order_id: &str,
    broker_trade_id: Option<&str>,
    pending: &[OpeningOrder],
    positions: &[Position],
    closed: Option<&[TransactionRecord]>,
) -> AttemptState {
    // TN market names are case-sensitive in places; normalise on the
    // way in to match the upstream broker-trait impl.
    let inst_key = instrument.to_lowercase();

    // 1. Pending: opening order on this instrument with matching id.
    let is_pending = pending
        .iter()
        .filter(|o| o.market.to_lowercase() == inst_key)
        .any(|o| o.order_id.to_string() == broker_order_id);
    if is_pending {
        return AttemptState::Pending;
    }

    // 2. Open position whose ORIGINATING order id matches. TN
    //    populates `Position.order_id` as the originating OrderID and
    //    `Position.position_id` as the distinct PositionID — we match
    //    on the former, return the latter as broker_trade_id.
    let open_match = positions
        .iter()
        .filter(|p| p.market_name.to_lowercase() == inst_key)
        .find(|p| p.order_id.to_string() == broker_order_id);
    if let Some(p) = open_match {
        return AttemptState::OpenPosition {
            broker_trade_id: p.position_id.to_string(),
        };
    }

    // 3. Closed trade match by broker_trade_id (against RefID — see
    //    helper doc for why this currently won't match on TN).
    if let (Some(btid), Some(history)) = (broker_trade_id, closed)
        && let Some(rec) = history
            .iter()
            .filter(|r| r.is_trade())
            .filter(|r| r.description.to_lowercase().contains(&inst_key))
            .find(|r| r.ref_id == btid)
    {
        let pl = rec.profit_loss_f64();
        return if pl > 0.0 {
            AttemptState::ClosedWin { realized_pl: pl }
        } else {
            AttemptState::ClosedLossOrBreakeven { realized_pl: pl }
        };
    }

    // 4. Distinguish lost-snapshot from never-snapshotted.
    if broker_trade_id.is_some() {
        AttemptState::Cancelled
    } else {
        AttemptState::Unknown
    }
}

#[cfg(test)]
mod attempt_state_tests {
    use super::*;

    fn opening_order(order_id: u64, market: &str) -> OpeningOrder {
        OpeningOrder {
            order_id,
            market_id: 0,
            market: market.into(),
            direction: "Buy".into(),
            stake: 1.0,
            stop_order_price: Some(1.1),
            limit_order_price: None,
            current_price: None,
            currency_symbol: String::new(),
            period: String::new(),
            creation_time_utc: String::new(),
            quote_id: 0,
        }
    }

    fn position(position_id: u64, order_id: u64, market_name: &str) -> Position {
        Position {
            position_id,
            order_id,
            market_id: 0,
            market_name: market_name.into(),
            direction: "Buy".into(),
            stake: 1.0,
            opening_price: 1.1,
            current_price: 1.1,
            open_pl: 0.0,
            stop_order_price: None,
            limit_order_price: None,
            imr: 0.0,
            currency_symbol: String::new(),
            creation_time: String::new(),
            quote_id: 0,
            tradable: true,
        }
    }

    fn closed_trade(ref_id: &str, description: &str, profit_loss: &str) -> TransactionRecord {
        TransactionRecord {
            description: description.into(),
            ref_id: ref_id.into(),
            action: "Trade Receivable".into(),
            // `is_trade()` requires TransactionType=2 and non-empty open/close prices.
            transaction_type: "2".into(),
            transaction_date: String::new(),
            open_period: String::new(),
            open_price: "1.0".into(),
            close_price: "1.1".into(),
            profit_loss: profit_loss.into(),
            amount: "1.0".into(),
            currency: "USD".into(),
        }
    }

    #[test]
    fn pending_when_order_id_in_opening_orders() {
        let pending = vec![opening_order(101, "EUR/USD"), opening_order(102, "EUR/USD")];
        let s = compute_attempt_state("EUR/USD", "101", None, &pending, &[], None);
        assert_eq!(s, AttemptState::Pending);
    }

    #[test]
    fn pending_filter_respects_instrument() {
        // Same order id but different instrument — should NOT match
        // (defensive: TN order ids are globally unique in practice,
        // but the algorithm filters anyway and the test pins it).
        let pending = vec![opening_order(101, "AUD/USD")];
        let s = compute_attempt_state("EUR/USD", "101", None, &pending, &[], None);
        assert_eq!(s, AttemptState::Unknown);
    }

    #[test]
    fn open_position_matches_on_originating_order_id_not_position_id() {
        // Critical TN-specific behaviour: position.order_id is the
        // originating OrderID (correlates to broker_order_id);
        // position.position_id is the distinct PositionID and is
        // returned as broker_trade_id.
        let positions = vec![position(
            /*PositionID*/ 9999, /*OrderID*/ 101, "EUR/USD",
        )];
        let s = compute_attempt_state("EUR/USD", "101", None, &[], &positions, None);
        assert_eq!(
            s,
            AttemptState::OpenPosition {
                broker_trade_id: "9999".into()
            }
        );
        // And verify we do NOT match if we look up by the position id
        // (which would be the wrong correlation key).
        let s_wrong = compute_attempt_state("EUR/USD", "9999", None, &[], &positions, None);
        assert_eq!(s_wrong, AttemptState::Unknown);
    }

    #[test]
    fn closed_win_when_ref_id_matches_and_pl_positive() {
        let closed = vec![closed_trade("ref-7", "EUR/USD", "12.5")];
        let s = compute_attempt_state("EUR/USD", "101", Some("ref-7"), &[], &[], Some(&closed));
        assert_eq!(s, AttemptState::ClosedWin { realized_pl: 12.5 });
    }

    #[test]
    fn closed_loss_or_breakeven_when_pl_non_positive() {
        let closed = vec![
            closed_trade("ref-loss", "EUR/USD", "-7.0"),
            closed_trade("ref-be", "EUR/USD", "0.0"),
        ];
        let loss =
            compute_attempt_state("EUR/USD", "101", Some("ref-loss"), &[], &[], Some(&closed));
        assert_eq!(
            loss,
            AttemptState::ClosedLossOrBreakeven { realized_pl: -7.0 }
        );
        let be = compute_attempt_state("EUR/USD", "101", Some("ref-be"), &[], &[], Some(&closed));
        assert_eq!(be, AttemptState::ClosedLossOrBreakeven { realized_pl: 0.0 });
    }

    #[test]
    fn cancelled_when_snapshotted_but_history_does_not_match() {
        // The realistic v1 TN case: trade_id was snapshotted (so we
        // know the attempt filled at some point), but the closed
        // history scan can't find it because RefID != PositionID.
        // Algorithm should report Cancelled.
        let closed = vec![closed_trade("ref-other", "EUR/USD", "5.0")];
        let s = compute_attempt_state("EUR/USD", "101", Some("9999"), &[], &[], Some(&closed));
        assert_eq!(s, AttemptState::Cancelled);
    }

    #[test]
    fn unknown_when_no_snapshot_and_nothing_to_match() {
        // No broker_trade_id snapshot, not pending, not open.
        let s = compute_attempt_state("EUR/USD", "101", None, &[], &[], None);
        assert_eq!(s, AttemptState::Unknown);
    }

    #[test]
    fn pending_takes_priority_over_open_branch() {
        // Defensive ordering check.
        let pending = vec![opening_order(101, "EUR/USD")];
        let positions = vec![position(9999, 101, "EUR/USD")];
        let s = compute_attempt_state("EUR/USD", "101", None, &pending, &positions, None);
        assert_eq!(s, AttemptState::Pending);
    }

    #[test]
    fn closed_skips_non_trade_records() {
        // is_trade() requires TransactionType="2" + non-empty
        // open/close prices. A funding/conversion record with a
        // matching ref_id must NOT be classified as a closed trade.
        let mut funding = closed_trade("ref-7", "EUR/USD", "5.0");
        funding.transaction_type = "1".into();
        let s = compute_attempt_state("EUR/USD", "101", Some("ref-7"), &[], &[], Some(&[funding]));
        assert_eq!(s, AttemptState::Cancelled);
    }
}
