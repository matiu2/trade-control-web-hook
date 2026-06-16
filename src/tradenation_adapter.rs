//! Adapt `broker-tradenation`'s inherent methods to `core::broker::Broker`.
//!
//! Upstream owns its own copies of `EntryRequest` / `Direction` / `ResolvedEntry`
//! / `EntryError` / `RiskBudget`, structurally identical to ours. We translate
//! between them at the boundary so the worker dispatch can stay generic over
//! [`Broker`].

use broker_tradenation::TradeNationBroker;
use candle_model::Granularity as CmGranularity;
use chrono::{DateTime, Utc};
use trade_control_core::broker::{
    AmendError, AttemptState, Broker, CancelError, Candle, CandleError, EntryError, EntryRequest,
    Granularity, LookupError, OpenPosition, PendingOrder, Quote,
};
use trade_control_core::intent::{Direction, ResolvedEntry, RiskBudget};
use tradenation_api::ohlcv::PriceType;
use tradenation_api::{OpeningOrder, Position, TransactionRecord};

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
            rlog!(
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
                rlog_err!("tn lookup get_account_details: {err:?}");
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
                rlog_err!("tn lookup get_transaction_history: {err:?}");
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
                rlog_err!("tn cancel_order({broker_order_id}): {err:?}");
                CancelError::Transient
            })
    }

    async fn get_quote(&self, instrument: &str) -> Result<Quote, LookupError> {
        // Two hops: name → market_id, then market_id → (bid, ask).
        // Upstream `latest_bid_ask` returns the most recent 1m bid/ask
        // candle close as a (f64, f64) tuple. The trait's default
        // `get_current_price` takes the mid; the spread-blackout systems
        // read `spread()`.
        let market = tradenation_api::resolve_market(self.0.client(), self.0.session(), instrument)
            .await
            .map_err(|err| {
                rlog_err!("tn resolve_market({instrument}): {err:?}");
                LookupError::Transient
            })?;
        let (bid, ask) = tradenation_api::latest_bid_ask(self.0.client(), market.market_id)
            .await
            .map_err(|err| {
                rlog_err!(
                    "tn latest_bid_ask({instrument}, market_id={}): {err:?}",
                    market.market_id
                );
                LookupError::Transient
            })?;
        Ok(Quote { bid, ask })
    }

    async fn list_open_positions(
        &self,
        _account_id: &str,
    ) -> Result<Vec<OpenPosition>, LookupError> {
        // TradeNation binds the account via the session, so the
        // trait-level `account_id` is intentionally ignored.
        let details = tradenation_api::get_account_details(self.0.session())
            .await
            .map_err(|err| {
                rlog_err!("tn list_open_positions get_account_details: {err:?}");
                LookupError::Transient
            })?;
        Ok(details
            .positions
            .records
            .iter()
            .map(tn_position_to_open)
            .collect())
    }

    async fn list_pending_orders(
        &self,
        _account_id: &str,
    ) -> Result<Vec<PendingOrder>, LookupError> {
        let details = tradenation_api::get_account_details(self.0.session())
            .await
            .map_err(|err| {
                rlog_err!("tn list_pending_orders get_account_details: {err:?}");
                LookupError::Transient
            })?;
        Ok(details
            .opening_orders
            .records
            .iter()
            .filter_map(|o| {
                let mapped = tn_order_to_pending(o);
                if mapped.is_none() {
                    rlog_err!(
                        "tn list_pending_orders: skipping malformed order_id={} market={} (no stop/limit trigger)",
                        o.order_id,
                        o.market,
                    );
                }
                mapped
            })
            .collect())
    }

    async fn get_candles(
        &self,
        instrument: &str,
        granularity: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Candle>, CandleError> {
        if since >= now {
            return Err(CandleError::BadRange);
        }
        // TN's chart OHLCV is count-back-from-`end_time`, not a true range,
        // so fetch enough candles to cover the window then trim to `> since`
        // with `filter_new_candles`. M5/H4 aren't served natively by the raw
        // `ohlcv::get_candles_range` (only the higher-level client method
        // aggregates them, and `TradeNationBroker` doesn't expose it) — the
        // engine only arms TN trades on native TFs, so reject the rest loudly.
        let (cm_gran, native) = to_cm_granularity(granularity);
        if !native {
            rlog_err!("tn get_candles({instrument}): granularity {granularity:?} not TN-native");
            return Err(CandleError::BadRange);
        }
        let count = candle_count_for_window(granularity, since, now);

        // name → market_id (same first hop as `get_quote`).
        let market = tradenation_api::resolve_market(self.0.client(), self.0.session(), instrument)
            .await
            .map_err(|err| {
                rlog_err!("tn get_candles resolve_market({instrument}): {err:?}");
                CandleError::Transient
            })?;

        let raw = tradenation_api::ohlcv::get_candles_range(
            self.0.client(),
            market.market_id,
            cm_gran,
            PriceType::Mid,
            count,
            now,
        )
        .await
        .map_err(|err| {
            rlog_err!(
                "tn get_candles({instrument}, market_id={}, count={count}): {err:?}",
                market.market_id
            );
            CandleError::Transient
        })?;

        let candles = raw
            .iter()
            .map(|c| Candle {
                time: c.timestamp.with_timezone(&Utc),
                o: c.open,
                h: c.high,
                l: c.low,
                c: c.close,
            })
            .collect();

        Ok(trade_control_core::broker::filter_new_candles(
            candles, since,
        ))
    }

    async fn amend_stop(
        &self,
        _account_id: &str,
        position_or_order_id: &str,
        new_stop: f64,
    ) -> Result<(), AmendError> {
        // `amend_order` needs the originating order id, market name, stake
        // and BOTH prices ("pass existing to leave unchanged"). The
        // trait-level id alone doesn't carry that, so re-fetch and locate
        // the record. Positions first (the cron's primary target), then
        // pending orders.
        //
        // UNVERIFIED: the upstream `amend_order` (`AmendCloseOrder`) has no
        // callers and it is not yet confirmed it amends an OPEN position's
        // SL keyed by the position's originating order id. Sub-plan 4 must
        // demo-confirm before any live widening relies on this path.
        let details = tradenation_api::get_account_details(self.0.session())
            .await
            .map_err(|err| {
                rlog_err!("tn amend_stop get_account_details: {err:?}");
                AmendError::Transient
            })?;

        let target = find_amend_target(
            position_or_order_id,
            &details.positions.records,
            &details.opening_orders.records,
        )
        .ok_or(AmendError::NotFound)?;

        // TradeNation requires BOTH prices on `AmendCloseOrder`. We move
        // the stop and leave TP unchanged by passing its existing value.
        // A `None` TP becomes 0.0 — UNVERIFIED whether the platform reads
        // that as "no TP" or "TP at 0". Sub-plan 4's demo must check; until
        // then, an amend on a position with no TP is the riskier case.
        let existing_tp = target.existing_take_profit.unwrap_or(0.0);

        tradenation_api::amend_order(
            self.0.client(),
            self.0.session(),
            target.order_id,
            &target.market,
            target.stake,
            new_stop,
            existing_tp,
        )
        .await
        .map(|_| ())
        .map_err(|err| {
            rlog_err!(
                "tn amend_stop(order_id={}, market={}, new_stop={new_stop}): {err:?}",
                target.order_id,
                target.market,
            );
            AmendError::Transient
        })
    }
}

/// The bits of a position / opening order `amend_stop` needs to call the
/// upstream `amend_order`. Split out so the lookup is unit-testable.
struct AmendTarget {
    order_id: u64,
    market: String,
    stake: f64,
    existing_take_profit: Option<f64>,
}

/// Locate the amend target for `id` among open positions (matched on
/// `position_id` OR originating `order_id`), then pending orders (matched on
/// `order_id`). Returns `None` if nothing matches. Pure — unit-tested.
fn find_amend_target(
    id: &str,
    positions: &[Position],
    pending: &[OpeningOrder],
) -> Option<AmendTarget> {
    if let Some(p) = positions
        .iter()
        .find(|p| p.position_id.to_string() == id || p.order_id.to_string() == id)
    {
        return Some(AmendTarget {
            order_id: p.order_id,
            market: p.market_name.clone(),
            stake: p.stake,
            existing_take_profit: p.limit_order_price,
        });
    }
    pending
        .iter()
        .find(|o| o.order_id.to_string() == id)
        .map(|o| AmendTarget {
            order_id: o.order_id,
            market: o.market.clone(),
            stake: o.stake,
            // For a pending entry order the parsed stop/limit prices are the
            // ENTRY TRIGGER, not the SL/TP (see `PendingOrder` doc + the IDO*
            // note). We have no parsed SL/TP to preserve, so leave TP as None.
            existing_take_profit: None,
        })
}

/// Map a TradeNation [`Position`] to a broker-agnostic [`OpenPosition`].
/// Pure — unit-tested for the Buy/Sell → direction and SL/TP optionality
/// branches. `direction` is `"Buy"` / `"Sell"` in upstream fixtures.
fn tn_position_to_open(p: &Position) -> OpenPosition {
    OpenPosition {
        instrument: p.market_name.clone(),
        direction: if p.direction == "Sell" {
            Direction::Short
        } else {
            Direction::Long
        },
        stop_loss: p.stop_order_price,
        take_profit: p.limit_order_price,
        position_id: p.position_id.to_string(),
        order_id: p.order_id.to_string(),
        stake: p.stake,
    }
}

/// Map a TradeNation [`OpeningOrder`] to a broker-agnostic [`PendingOrder`].
/// Returns `None` for a malformed order with neither stop nor limit trigger
/// (caller skips it rather than failing the whole list).
///
/// **Trigger semantics:** on a pending entry order, whichever of
/// `stop_order_price` / `limit_order_price` is set is the ENTRY TRIGGER —
/// stop-entry if `stop_order_price` is set, else limit-entry. This is the
/// trigger, NOT the attached SL/TP (those live in unparsed `IDO*` fields).
fn tn_order_to_pending(o: &OpeningOrder) -> Option<PendingOrder> {
    let (trigger, is_stop) = match (o.stop_order_price, o.limit_order_price) {
        (Some(s), _) => (s, true),
        (None, Some(l)) => (l, false),
        (None, None) => return None,
    };
    Some(PendingOrder {
        order_id: o.order_id.to_string(),
        instrument: o.market.clone(),
        direction: if o.direction == "Sell" {
            Direction::Short
        } else {
            Direction::Long
        },
        trigger,
        is_stop,
        stake: o.stake,
    })
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

/// Engine [`Granularity`] → `candle_model::Granularity` + whether TN serves it
/// natively via the raw `ohlcv::get_candles_range`. TN's native timeframes are
/// minute / quarter(15m) / hour / day; M5 and H4 are only available through the
/// higher-level aggregating client method (not reachable from
/// `TradeNationBroker`), so they map but are flagged non-native. Pure.
fn to_cm_granularity(g: Granularity) -> (CmGranularity, bool) {
    match g {
        Granularity::M1 => (CmGranularity::OneMinute, true),
        Granularity::M15 => (CmGranularity::FifteenMinutes, true),
        Granularity::H1 => (CmGranularity::OneHour, true),
        Granularity::D1 => (CmGranularity::OneDay, true),
        Granularity::M5 => (CmGranularity::FiveMinutes, false),
        Granularity::H4 => (CmGranularity::FourHours, false),
    }
}

/// How many candles to fetch ending at `now` to be sure of covering
/// `(since, now]`. TN's OHLCV is count-back-from-end, so we ask for one bar per
/// `granularity` step in the window plus a small slack for boundary alignment,
/// clamped to TN's per-request ceiling. `filter_new_candles` trims any extra.
/// Pure — unit-tested.
fn candle_count_for_window(g: Granularity, since: DateTime<Utc>, now: DateTime<Utc>) -> usize {
    const SLACK: i64 = 3;
    /// TN's chart endpoint caps a single request; keep well under it.
    const MAX: i64 = 1000;
    let span = (now - since).num_seconds().max(0);
    let bars = span / g.seconds() + 1 + SLACK;
    bars.clamp(1, MAX) as usize
}

fn from_upstream_error(e: broker_tradenation::EntryError) -> EntryError {
    use broker_tradenation::EntryError as U;
    match e {
        U::AccountFetch => EntryError::AccountFetch,
        U::EquityParse => EntryError::EquityParse,
        U::RiskCapExceeded { requested, cap } => EntryError::RiskCapExceeded { requested, cap },
        U::OpenPositionsCapExceeded => EntryError::OpenPositionsCapExceeded,
        U::UnitsBelowMinimum => EntryError::UnitsBelowMinimum,
        U::EntryTooCloseToMarket => EntryError::EntryTooCloseToMarket,
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
            period: None,
            period_original: String::new(),
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
            creation_time: None,
            creation_time_original: String::new(),
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
            transaction_date: None,
            transaction_date_original: String::new(),
            open_period: None,
            open_period_original: String::new(),
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

#[cfg(test)]
mod candle_fetch_tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn native_granularities_map_and_flag() {
        assert_eq!(
            to_cm_granularity(Granularity::M1),
            (CmGranularity::OneMinute, true)
        );
        assert_eq!(
            to_cm_granularity(Granularity::M15),
            (CmGranularity::FifteenMinutes, true)
        );
        assert_eq!(
            to_cm_granularity(Granularity::H1),
            (CmGranularity::OneHour, true)
        );
        assert_eq!(
            to_cm_granularity(Granularity::D1),
            (CmGranularity::OneDay, true)
        );
    }

    #[test]
    fn non_native_granularities_are_flagged() {
        assert!(!to_cm_granularity(Granularity::M5).1);
        assert!(!to_cm_granularity(Granularity::H4).1);
    }

    #[test]
    fn count_covers_window_plus_slack() {
        // 5 H1 bars of span → 5 + 1 + 3 slack = 9.
        let since = ts("2026-06-16T00:00:00Z");
        let now = ts("2026-06-16T05:00:00Z");
        assert_eq!(candle_count_for_window(Granularity::H1, since, now), 9);
    }

    #[test]
    fn count_is_at_least_one_for_zero_span() {
        let t = ts("2026-06-16T00:00:00Z");
        // Degenerate span is guarded earlier (BadRange) but the helper must
        // still never return 0.
        assert!(candle_count_for_window(Granularity::H1, t, t) >= 1);
    }

    #[test]
    fn count_clamps_to_ceiling() {
        // A year of M1 bars would be ~525k; must clamp to the 1000 cap.
        let since = ts("2025-06-16T00:00:00Z");
        let now = ts("2026-06-16T00:00:00Z");
        assert_eq!(candle_count_for_window(Granularity::M1, since, now), 1000);
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::*;

    fn position_with(
        position_id: u64,
        order_id: u64,
        market_name: &str,
        direction: &str,
        sl: Option<f64>,
        tp: Option<f64>,
    ) -> Position {
        Position {
            position_id,
            order_id,
            market_id: 0,
            market_name: market_name.into(),
            direction: direction.into(),
            stake: 2.5,
            opening_price: 1.1,
            current_price: 1.1,
            open_pl: 0.0,
            stop_order_price: sl,
            limit_order_price: tp,
            imr: 0.0,
            currency_symbol: String::new(),
            creation_time: None,
            creation_time_original: String::new(),
            quote_id: 0,
            tradable: true,
        }
    }

    fn opening_order_with(
        order_id: u64,
        market: &str,
        direction: &str,
        stop: Option<f64>,
        limit: Option<f64>,
    ) -> OpeningOrder {
        OpeningOrder {
            order_id,
            market_id: 0,
            market: market.into(),
            direction: direction.into(),
            stake: 3.0,
            stop_order_price: stop,
            limit_order_price: limit,
            current_price: None,
            currency_symbol: String::new(),
            period: None,
            period_original: String::new(),
            creation_time_utc: String::new(),
            quote_id: 0,
        }
    }

    #[test]
    fn position_maps_buy_to_long_and_keeps_sl_tp() {
        let p = position_with(9999, 101, "EUR/USD", "Buy", Some(1.05), Some(1.20));
        let o = tn_position_to_open(&p);
        assert_eq!(
            o,
            OpenPosition {
                instrument: "EUR/USD".into(),
                direction: Direction::Long,
                stop_loss: Some(1.05),
                take_profit: Some(1.20),
                position_id: "9999".into(),
                order_id: "101".into(),
                stake: 2.5,
            }
        );
    }

    #[test]
    fn position_maps_sell_to_short_and_optional_sl_tp() {
        let p = position_with(1, 2, "Spot Gold", "Sell", None, None);
        let o = tn_position_to_open(&p);
        assert_eq!(o.direction, Direction::Short);
        assert_eq!(o.stop_loss, None);
        assert_eq!(o.take_profit, None);
        assert_eq!(o.position_id, "1");
        assert_eq!(o.order_id, "2");
    }

    #[test]
    fn pending_stop_entry_sets_is_stop_true_and_uses_trigger() {
        // stop_order_price set → stop-entry, trigger = that price.
        let ord = opening_order_with(55, "EUR/USD", "Buy", Some(1.1234), None);
        let p = tn_order_to_pending(&ord).expect("mapped");
        assert_eq!(
            p,
            PendingOrder {
                order_id: "55".into(),
                instrument: "EUR/USD".into(),
                direction: Direction::Long,
                trigger: 1.1234,
                is_stop: true,
                stake: 3.0,
            }
        );
    }

    #[test]
    fn pending_limit_entry_sets_is_stop_false() {
        let ord = opening_order_with(56, "AUD/USD", "Sell", None, Some(0.6500));
        let p = tn_order_to_pending(&ord).expect("mapped");
        assert!(!p.is_stop);
        assert_eq!(p.trigger, 0.6500);
        assert_eq!(p.direction, Direction::Short);
    }

    #[test]
    fn pending_prefers_stop_when_both_present() {
        // Defensive: if both are somehow set, the stop side wins (matches
        // the "whichever of stop/limit is set" rule, stop-first).
        let ord = opening_order_with(57, "EUR/USD", "Buy", Some(1.10), Some(1.20));
        let p = tn_order_to_pending(&ord).expect("mapped");
        assert!(p.is_stop);
        assert_eq!(p.trigger, 1.10);
    }

    #[test]
    fn pending_with_neither_trigger_is_skipped() {
        let ord = opening_order_with(58, "EUR/USD", "Buy", None, None);
        assert_eq!(tn_order_to_pending(&ord), None);
    }

    #[test]
    fn amend_target_matches_position_by_position_id() {
        let positions = vec![position_with(
            9999,
            101,
            "EUR/USD",
            "Buy",
            Some(1.05),
            Some(1.20),
        )];
        let t = find_amend_target("9999", &positions, &[]).expect("found");
        // amend key is the ORIGINATING order id, not the position id.
        assert_eq!(t.order_id, 101);
        assert_eq!(t.market, "EUR/USD");
        assert_eq!(t.stake, 2.5);
        assert_eq!(t.existing_take_profit, Some(1.20));
    }

    #[test]
    fn amend_target_matches_position_by_order_id() {
        let positions = vec![position_with(9999, 101, "EUR/USD", "Buy", None, None)];
        let t = find_amend_target("101", &positions, &[]).expect("found");
        assert_eq!(t.order_id, 101);
        assert_eq!(t.existing_take_profit, None);
    }

    #[test]
    fn amend_target_falls_back_to_pending_order() {
        let pending = vec![opening_order_with(77, "EUR/USD", "Buy", Some(1.10), None)];
        let t = find_amend_target("77", &[], &pending).expect("found");
        assert_eq!(t.order_id, 77);
        // Pending orders carry no parsed SL/TP — TP left None.
        assert_eq!(t.existing_take_profit, None);
    }

    #[test]
    fn amend_target_none_when_id_absent() {
        assert!(find_amend_target("404", &[], &[]).is_none());
    }
}
