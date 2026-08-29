//! Promoting a [`StoredOrder`](super::StoredOrder): park → Pending.
//!
//! The other half of [`super::park`]. Every candle, each parked order is
//! re-asked the question that parked it — *"does this trade clear its R-floor at
//! the spread right now?"* — and placed the moment the answer turns yes.
//!
//! Without this, parking would only preserve a setup, never act on it: the
//! `sgdjpy` fixture would still book 0R, just with a tidier audit trail.
//!
//! # How a Stored order is addressed
//!
//! A resting order is recovered by the **broker's** `order_id`. A Stored order
//! has none — it was never sent — so promotion keys on
//! [`Intent::id`](crate::intent::Intent::id) (`{trade_id}-enter`), which is ours
//! and exists from arm time. That is the whole reason the
//! [`VerifiedSource`] key is a generic correlation handle rather than a broker
//! id: keying on the broker's identifier made anything not-yet-placed
//! unaddressable by construction.
//!
//! Authenticity is unchanged and separate: the stored bytes are re-verified by
//! HMAC on the way out, exactly as the blackout re-drive does, so a tampered
//! park cannot promote.
//!
//! # Why re-drive through `run_enter`
//!
//! Promotion goes through the full entry path (RAIL 7 — never `place_entry`
//! directly), so a promoted order passes every gate a fresh fire would: pauses,
//! vetos, cooldowns, sizing, the spread floor itself. A promotion that skipped
//! them could place a trade the operator had since vetoed.
//!
//! It re-drives with `restore = true` because a promotion is **not a new
//! attempt** — it is the placement of an order we already intended and merely
//! deferred. Burning a `max_retries` slot would let a wide spread silently eat
//! the operator's re-entry budget.

use chrono::{DateTime, Utc};

use super::park::{clear_stored_order, stored_order};
use super::stored::{StoredVerdict, stored_verdict};
use crate::broker::Broker;
use crate::dispatch::run_enter;
use crate::pending_lifecycle::{EnterConfigProvider, Recovered, VerifiedSource};
use crate::state::StateStore;

/// What happened to a trade's parked order this candle.
#[derive(Debug, Clone, PartialEq)]
pub enum PromoteOutcome {
    /// No order is parked for this trade.
    NothingParked,
    /// Still parked — the spread hasn't calmed enough yet.
    StillWaiting,
    /// Placed. Carries `ActionResult::describe()` for the log.
    Promoted(String),
    /// Dropped: too close to expiry to be worth entering, or the signed body
    /// could no longer be recovered.
    Dropped(&'static str),
}

/// Re-check this trade's parked order and place it if it now clears its R-floor.
///
/// `clears_min_r` is the caller's verdict from
/// [`sl_target`](super::sl_target) against the **current** spread — passed in
/// rather than recomputed here so the R decision lives in exactly one place and
/// this stays a thin orchestration layer.
///
/// Errors are returned as `Err(String)` for the caller to log; a parked order is
/// left in place on any failure, so a transient store/broker problem defers the
/// promotion rather than losing the setup.
#[allow(clippy::too_many_arguments)]
pub async fn promote_stored_order<B, S, P, V>(
    broker: &B,
    store: &S,
    cfg_provider: &P,
    src: &V,
    trade_id: &str,
    clears_min_r: bool,
    now: DateTime<Utc>,
) -> Result<PromoteOutcome, String>
where
    B: Broker,
    S: StateStore,
    P: EnterConfigProvider,
    V: VerifiedSource,
{
    let Some(order) = stored_order(store, trade_id)
        .await
        .map_err(|e| format!("read stored order: {e}"))?
    else {
        return Ok(PromoteOutcome::NothingParked);
    };

    match stored_verdict(&order, now, clears_min_r) {
        StoredVerdict::KeepWaiting => return Ok(PromoteOutcome::StillWaiting),
        StoredVerdict::Drop => {
            tracing::info!(
                "stored-order: DROPPED trade={trade_id} — past drop_at={} ({} bars before \
                 expiry); no runway left for the setup to work",
                order.drop_at,
                super::stored::DROP_BARS_BEFORE_EXPIRY,
            );
            clear_stored_order(store, trade_id, now)
                .await
                .map_err(|e| format!("clear dropped order: {e}"))?;
            return Ok(PromoteOutcome::Dropped("past-drop-at"));
        }
        StoredVerdict::Promote => {}
    }

    // Re-verify the parked body. Keyed by OUR id, not the broker's — a Stored
    // order has no broker id (see the module docs).
    let verified = match src.recover(trade_id, Some(&order.signed_intent), now).await {
        Recovered::Ok(v) => *v,
        Recovered::Expired => {
            tracing::info!(
                "stored-order: DROPPED trade={trade_id} — signed window closed while parked",
            );
            clear_stored_order(store, trade_id, now)
                .await
                .map_err(|e| format!("clear expired order: {e}"))?;
            return Ok(PromoteOutcome::Dropped("window-closed"));
        }
        Recovered::Unrecoverable => {
            // Deliberately NOT cleared: an unverifiable body is a reason to
            // refuse to act, not a reason to destroy the record — the operator
            // should be able to see what was parked.
            return Err(format!("stored order for {trade_id} will not verify"));
        }
    };

    // Clear BEFORE placing. If the placement succeeds but the clear failed, the
    // next candle would promote the same order again and open a second position
    // — a duplicate entry on real money. Clearing first can at worst lose a
    // promotion (recoverable: the setup re-fires) rather than duplicate one.
    clear_stored_order(store, trade_id, now)
        .await
        .map_err(|e| format!("clear before promote: {e}"))?;

    // Full entry path (RAIL 7), `restore = true`: a promotion places an order we
    // already intended, so it must not burn a `max_retries` slot nor be
    // rejected as a replay of its own already-seen `shell.time`.
    let cfg = cfg_provider.dispatch_config(&verified).await;
    let result = run_enter(
        broker,
        store,
        &verified,
        &cfg,
        now,
        Some(&order.signed_intent),
        None,
        true,
    )
    .await;
    tracing::info!(
        "stored-order: PROMOTED trade={trade_id} (parked at {}) → {}",
        order.stored_at,
        result.describe(),
    );
    Ok(PromoteOutcome::Promoted(result.describe()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order_control::{StoredOrder, StoredReason, park_order};
    use crate::state::MemStateStore;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid test timestamp")
            .with_timezone(&Utc)
    }

    fn parked(store: &MemStateStore, drop_at: &str, now: DateTime<Utc>) {
        pollster::block_on(park_order(
            store,
            "t-1",
            "SGD_JPY",
            None,
            StoredOrder {
                signed_intent: "body".into(),
                reason: StoredReason::BelowMinR,
                original_sl_distance: 0.0020,
                tp_distance: 0.0200,
                min_r: 1.0,
                stored_at: now,
                drop_at: at(drop_at),
                shell_time: now,
            },
            at("2026-07-24T00:00:00Z"),
            now,
        ))
        .expect("park");
    }

    #[test]
    fn nothing_parked_is_not_an_error() {
        let store = MemStateStore::default();
        let out = pollster::block_on(promote_stored_order(
            &NoopBroker,
            &store,
            &TestCfg,
            &TestSrc::Unrecoverable,
            "t-1",
            true,
            at("2026-07-22T13:30:00Z"),
        ))
        .expect("no park is benign");
        assert_eq!(out, PromoteOutcome::NothingParked);
    }

    /// Below 1R the order stays parked — this is the every-candle wait.
    #[test]
    fn sub_1r_keeps_waiting() {
        let store = MemStateStore::default();
        let now = at("2026-07-22T13:30:00Z");
        parked(&store, "2026-07-23T21:00:00Z", now);
        let out = pollster::block_on(promote_stored_order(
            &NoopBroker,
            &store,
            &TestCfg,
            &TestSrc::Unrecoverable,
            "t-1",
            false,
            now,
        ))
        .expect("still waiting");
        assert_eq!(out, PromoteOutcome::StillWaiting);
        assert!(
            pollster::block_on(stored_order(&store, "t-1"))
                .expect("read")
                .is_some(),
            "the setup must still be parked",
        );
    }

    /// Past the drop deadline the order is dropped and the park cleared, even
    /// though the spread has calmed enough to promote.
    #[test]
    fn past_drop_at_is_dropped_and_cleared() {
        let store = MemStateStore::default();
        let now = at("2026-07-22T13:30:00Z");
        parked(&store, "2026-07-22T14:00:00Z", now);
        let out = pollster::block_on(promote_stored_order(
            &NoopBroker,
            &store,
            &TestCfg,
            &TestSrc::Unrecoverable,
            "t-1",
            true,
            at("2026-07-22T15:00:00Z"),
        ))
        .expect("dropped");
        assert_eq!(out, PromoteOutcome::Dropped("past-drop-at"));
        assert!(
            pollster::block_on(stored_order(&store, "t-1"))
                .expect("read")
                .is_none(),
            "a dropped order must not linger",
        );
    }

    /// An unverifiable body refuses to act but must NOT destroy the record —
    /// the operator should still be able to see what was parked.
    #[test]
    fn unverifiable_body_errors_without_clearing() {
        let store = MemStateStore::default();
        let now = at("2026-07-22T13:30:00Z");
        parked(&store, "2026-07-23T21:00:00Z", now);
        let err = pollster::block_on(promote_stored_order(
            &NoopBroker,
            &store,
            &TestCfg,
            &TestSrc::Unrecoverable,
            "t-1",
            true,
            now,
        ))
        .expect_err("must not silently promote an unverifiable body");
        assert!(err.contains("will not verify"), "{err}");
        assert!(
            pollster::block_on(stored_order(&store, "t-1"))
                .expect("read")
                .is_some(),
            "refusing to act must not destroy the record",
        );
    }

    /// A signed window that closed while parked drops the order rather than
    /// entering on a stale thesis.
    #[test]
    fn expired_window_drops_the_park() {
        let store = MemStateStore::default();
        let now = at("2026-07-22T13:30:00Z");
        parked(&store, "2026-07-23T21:00:00Z", now);
        let out = pollster::block_on(promote_stored_order(
            &NoopBroker,
            &store,
            &TestCfg,
            &TestSrc::Expired,
            "t-1",
            true,
            now,
        ))
        .expect("dropped");
        assert_eq!(out, PromoteOutcome::Dropped("window-closed"));
        assert!(
            pollster::block_on(stored_order(&store, "t-1"))
                .expect("read")
                .is_none(),
        );
    }

    /// None of these tests reach the broker — every case short-circuits before
    /// `run_enter`. `place_entry` panics to keep that explicit: if a future
    /// change made one of them place an order, it would fail loudly here rather
    /// than silently exercising a fake fill.
    struct NoopBroker;
    impl Broker for NoopBroker {
        async fn place_entry(
            &self,
            _max_risk_pct: f64,
            _max_open_positions: u32,
            _req: &crate::broker::EntryRequest<'_>,
        ) -> Result<String, crate::broker::EntryError> {
            panic!("no test in this module should reach the broker")
        }
        async fn close_positions(&self, _instrument: &str) -> crate::broker::CloseOutcome {
            crate::broker::CloseOutcome::NothingOpen
        }
        async fn cancel_pending_for_instrument(&self, _instrument: &str) -> usize {
            0
        }
        async fn lookup_attempt_state(
            &self,
            _instrument: &str,
            _broker_order_id: &str,
            _broker_trade_id: Option<&str>,
        ) -> Result<crate::broker::AttemptState, crate::broker::LookupError> {
            Ok(crate::broker::AttemptState::Unknown)
        }
        async fn cancel_order(
            &self,
            _account_id: &str,
            _broker_order_id: &str,
        ) -> Result<(), crate::broker::CancelError> {
            Ok(())
        }
        async fn get_quote(
            &self,
            _instrument: &str,
        ) -> Result<crate::broker::Quote, crate::broker::LookupError> {
            Ok(crate::broker::Quote {
                bid: 1.0,
                ask: 1.0001,
            })
        }
        async fn list_open_positions(
            &self,
            _account_id: &str,
        ) -> Result<Vec<crate::broker::OpenPosition>, crate::broker::LookupError> {
            Ok(vec![])
        }
        async fn amend_stop(
            &self,
            _account_id: &str,
            _position_or_order_id: &str,
            _new_stop: f64,
        ) -> Result<(), crate::broker::AmendError> {
            Ok(())
        }
        async fn list_pending_orders(
            &self,
            _account_id: &str,
        ) -> Result<Vec<crate::broker::PendingOrder>, crate::broker::LookupError> {
            Ok(vec![])
        }
        async fn get_candles(
            &self,
            _instrument: &str,
            _granularity: crate::broker::Granularity,
            _since: DateTime<Utc>,
            _now: DateTime<Utc>,
        ) -> Result<Vec<crate::broker::Candle>, crate::broker::CandleError> {
            Ok(vec![])
        }
    }

    struct TestCfg;
    impl EnterConfigProvider for TestCfg {
        async fn dispatch_config(
            &self,
            _verified: &crate::incoming::Verified,
        ) -> crate::dispatch_config::DispatchConfig {
            crate::dispatch_config::DispatchConfig {
                worker_max_risk_pct: 1.0,
                worker_max_open_positions: 3,
                pip_size: 0.0001,
                tick_size: None,
                caps: Default::default(),
            }
        }
    }

    enum TestSrc {
        Unrecoverable,
        Expired,
    }
    impl VerifiedSource for TestSrc {
        async fn recover(
            &self,
            _key: &str,
            _signed_body: Option<&str>,
            _now: DateTime<Utc>,
        ) -> Recovered {
            match self {
                Self::Unrecoverable => Recovered::Unrecoverable,
                Self::Expired => Recovered::Expired,
            }
        }
    }
}
