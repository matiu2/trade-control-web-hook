//! Carrying out a [`PendingAction`]: cancel-and-replace, or demote to Stored.
//!
//! The effectful half of [`super::pending`] — that module decides, this one
//! moves the order at the broker. Same split as [`super::stored`] ↔
//! [`super::park`], for the same reason: the decision stays unit-testable
//! without a broker.
//!
//! # Why cancel-and-replace at all
//!
//! [`Broker::amend_stop`] moves a stop and explicitly leaves the stake untouched,
//! and the trait has **no resize method** on either implementation. But stake and
//! stop distance are two halves of one quantity — `risk = stake × sl_distance` —
//! so amending the stop alone silently re-prices the trade's risk: a stop widened
//! 3× at the original stake turns a 1% trade into a 3%.
//!
//! Given no resize, the only way to keep risk exact is to cancel the order and
//! place a fresh one. That is the operator's explicit choice, over the
//! alternative of amending the stop and letting risk drift.
//!
//! # The unguarded gap — the cost of that choice
//!
//! Between the cancel and the re-place the order is **not at the broker**. If
//! price reaches the trigger inside that window, the entry is simply missed.
//!
//! This is accepted, not overlooked. A missed entry costs an opportunity; a
//! wrongly-sized one costs money, and the setup re-fires if it is still valid.
//! The window is one broker round-trip on a ~5s cron. What the code *can* do is
//! make sure a missed re-place is never a **silent** loss, which is what the
//! ordering below is for.
//!
//! # Ordering, and why the body is captured first
//!
//! The rail: **never cancel an order you cannot re-place.** So the signed body is
//! recovered and verified *before* the cancel, and a body that will not verify
//! aborts the whole adjustment with the order left resting — exactly the rail
//! `pending_lifecycle`'s cancel pass follows (rails 2 and 3). An order left
//! resting at a slightly wrong size is recoverable next tick; an order cancelled
//! with nothing to re-place it is a setup silently lost.
//!
//! A re-place that is *rejected* is different from one that is never attempted:
//! it is reported as [`RepriceOutcome::ReplaceFailed`] so the caller can log it
//! loudly rather than have it look like a hold.
//!
//! # Why `run_enter` and not `place_entry`
//!
//! RAIL 7 — the re-place goes through the full entry path, so it passes every
//! gate a fresh fire would (pauses, vetos, cooldowns, sizing) and **inherits the
//! stop↔limit flip** from `place_entry_too_close_fallback` when price has drifted
//! past the trigger. That flip is rule 4, and it belongs to the shared re-place
//! path rather than being copied here.
//!
//! `restore = true` for the same reason promotion uses it: a re-price is a
//! replacement of an order we already placed, not a new attempt, so it must not
//! burn a `max_retries` slot.

use chrono::{DateTime, Utc};

use super::park::park_order;
use super::pending::PendingAction;
use super::stored::{StoredOrder, StoredReason, drop_at};
use crate::broker::{Broker, PendingOrder};
use crate::dispatch::run_enter;
use crate::pending_lifecycle::{EnterConfigProvider, Recovered, VerifiedSource};
use crate::state::StateStore;

/// What happened to one resting order this candle.
#[derive(Debug, Clone, PartialEq)]
pub enum RepriceOutcome {
    /// Left exactly as it rests — the common case.
    Held,
    /// Cancelled and re-placed. Carries `ActionResult::describe()` for the log.
    Repriced(String),
    /// Cancelled and parked as a [`StoredOrder`]: it fell below its R-floor, so
    /// it must not be left where it can fill, but the setup is kept.
    Demoted,
    /// The order was cancelled but the replacement did not go on. The setup is
    /// **parked** so the next tick can retry it — a rejected re-place must not
    /// silently lose the trade. Carries the reason for the log.
    ReplaceFailed(String),
    /// Nothing was done because the order could not be safely acted on: no
    /// recoverable body, or a store failure. The order is left **resting**.
    Skipped(&'static str),
}

/// Carry out `action` against one resting order.
///
/// `action` comes from [`pending_action`](super::pending_action) — computed by
/// the caller, which owns the spread readings and the trade's geometry. Keeping
/// the decision outside means this function has no opinion of its own about when
/// an order should move, only about how to move it safely.
///
/// Errors are returned as `Err(String)` for the caller to log. On any failure the
/// order is left in the safest state reachable from where the failure happened —
/// see the module docs on ordering.
#[allow(clippy::too_many_arguments)]
pub async fn reprice_pending_order<B, S, P, V>(
    broker: &B,
    store: &S,
    cfg_provider: &P,
    src: &V,
    order: &PendingOrder,
    account: Option<&str>,
    action: PendingAction,
    expires_at: DateTime<Utc>,
    bar_seconds: i64,
    now: DateTime<Utc>,
) -> Result<RepriceOutcome, String>
where
    B: Broker,
    S: StateStore,
    P: EnterConfigProvider,
    V: VerifiedSource,
{
    if action == PendingAction::Hold {
        return Ok(RepriceOutcome::Held);
    }

    // --- Recover BEFORE cancelling (the rail). ------------------------------
    // An order we cannot re-place is an order we must not cancel. A store read
    // failure is a skip for the same reason: we cannot prove we could restore it.
    let stored_body = store
        .get_order_body(&order.order_id)
        .await
        .map_err(|e| format!("get_order_body({}): {e}", order.order_id))?;

    let verified = match src
        .recover(&order.order_id, stored_body.as_deref(), now)
        .await
    {
        Recovered::Ok(v) => *v,
        Recovered::Expired => {
            // The signed window closed. Re-placing it would enter on a stale
            // thesis, so leave it resting and let the normal expiry path retire
            // it — this module's job is sizing, not lifecycle.
            return Ok(RepriceOutcome::Skipped("window-closed"));
        }
        Recovered::Unrecoverable => {
            return Ok(RepriceOutcome::Skipped("no-recoverable-body"));
        }
    };
    let trade_id = verified
        .intent
        .trade_id
        .clone()
        .unwrap_or_else(|| order.order_id.clone());
    // The bytes to re-drive with. Live: the verified body. Replay: a placeholder,
    // whose source keys off the armed map rather than this string.
    let signed_intent =
        stored_body.unwrap_or_else(|| format!("replay-order: {}\n", order.order_id));

    // --- Cancel. ------------------------------------------------------------
    // From here the order is off the broker: everything below must leave the
    // setup recoverable.
    broker
        .cancel_order(account.unwrap_or(""), &order.order_id)
        .await
        .map_err(|e| format!("cancel_order({}): {e}", order.order_id))?;

    // A demote stops here: parked, deliberately not re-placed.
    if action == PendingAction::Demote {
        park_below_min_r(
            store,
            &trade_id,
            &order.instrument,
            account,
            signed_intent,
            verified.shell.time,
            expires_at,
            bar_seconds,
            now,
        )
        .await?;
        tracing::info!(
            "reprice[{trade_id}]: DEMOTED resting order {} — below its R-floor at the current \
             spread; parked rather than left where it could fill",
            order.order_id,
        );
        return Ok(RepriceOutcome::Demoted);
    }

    // --- Re-place through the full entry path (RAIL 7). ---------------------
    // `restore = true`: a re-price replaces an order we already placed, so it
    // must not burn a `max_retries` slot. The stop↔limit flip on "#19-10 too
    // close to market" is inherited from this path, not re-implemented here.
    let cfg = cfg_provider.dispatch_config(&verified).await;
    let result = run_enter(
        broker,
        store,
        &verified,
        &cfg,
        now,
        Some(&signed_intent),
        None,
        true,
    )
    .await;

    if matches!(result, crate::dispatch::ActionResult::Ok(_)) {
        tracing::info!(
            "reprice[{trade_id}]: re-placed order {} at the new size → {}",
            order.order_id,
            result.describe(),
        );
        return Ok(RepriceOutcome::Repriced(result.describe()));
    }

    // The re-place was rejected and the original is already gone. Park the setup
    // so the next tick can retry it — the accepted cost of cancel-and-replace is
    // a *missed* entry, never a silently *lost* one.
    let why = result.describe();
    tracing::error!(
        "reprice[{trade_id}]: order {} was cancelled but the re-place FAILED ({why}); parking the \
         setup so it can be retried",
        order.order_id,
    );
    park_below_min_r(
        store,
        &trade_id,
        &order.instrument,
        account,
        signed_intent,
        verified.shell.time,
        expires_at,
        bar_seconds,
        now,
    )
    .await?;
    Ok(RepriceOutcome::ReplaceFailed(why))
}

/// Park a cancelled order as Stored, so a later tick can promote it.
///
/// Shared by the demote path and the failed-re-place path: both end with the
/// order off the broker and the setup needing to survive. The distance recorded
/// is the *drawn* one — a park is re-sized when it promotes, against the spread
/// at that time, so freezing today's widened distance into it would carry a stale
/// spread forward.
#[allow(clippy::too_many_arguments)]
async fn park_below_min_r<S: StateStore>(
    store: &S,
    trade_id: &str,
    instrument: &str,
    account: Option<&str>,
    signed_intent: String,
    shell_time: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    bar_seconds: i64,
    now: DateTime<Utc>,
) -> Result<(), String> {
    park_order(
        store,
        trade_id,
        instrument,
        account,
        StoredOrder {
            signed_intent,
            reason: StoredReason::BelowMinR,
            // Sized at promotion time against the spread then — see the fn doc.
            original_sl_distance: 0.0,
            tp_distance: 0.0200,
            min_r: 1.0,
            stored_at: now,
            drop_at: drop_at(expires_at, bar_seconds, now),
            shell_time,
        },
        expires_at,
        now,
    )
    .await
    .map_err(|e| format!("park {trade_id}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::{
        AmendError, AttemptState, CancelError, Candle, CandleError, EntryError, EntryRequest,
        Granularity, LookupError, OpenPosition, Quote,
    };
    use crate::intent::Direction;
    use crate::order_control::stored_order;
    use crate::state::MemStateStore;
    use std::cell::RefCell;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid test timestamp")
            .with_timezone(&Utc)
    }

    fn resting() -> PendingOrder {
        PendingOrder {
            order_id: "ord-1".into(),
            instrument: "EUR_USD".into(),
            direction: Direction::Long,
            trigger: 1.1000,
            is_stop: true,
            stake: 1.0,
        }
    }

    fn adjust() -> PendingAction {
        PendingAction::Adjust {
            sl_distance: 0.0064,
            stake: 15_625.0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run<B: Broker>(
        broker: &B,
        store: &MemStateStore,
        src: &TestSrc,
        action: PendingAction,
        now: DateTime<Utc>,
    ) -> Result<RepriceOutcome, String> {
        pollster::block_on(reprice_pending_order(
            broker,
            store,
            &TestCfg,
            src,
            &resting(),
            None,
            action,
            at("2026-07-24T00:00:00Z"),
            3600,
            now,
        ))
    }

    /// A `Hold` must not reach the broker at all — no cancel, no re-place, no
    /// unguarded gap. This is the overwhelmingly common path, so it is also the
    /// one whose cost matters most.
    ///
    /// Mutation check: remove the early return and `cancels` goes to 1.
    #[test]
    fn hold_never_touches_the_broker() {
        let store = MemStateStore::default();
        let broker = SpyBroker::default();
        let out = run(
            &broker,
            &store,
            &TestSrc::Unrecoverable,
            PendingAction::Hold,
            at("2026-07-22T13:30:00Z"),
        )
        .expect("hold is benign");
        assert_eq!(out, RepriceOutcome::Held);
        assert_eq!(broker.cancels.borrow().len(), 0, "a hold cancels nothing");
    }

    /// THE RAIL: an order whose body will not verify must be left **resting**,
    /// never cancelled. Cancelling something we cannot re-place loses the setup
    /// outright.
    ///
    /// Mutation check: move the `recover` call after the cancel and this goes
    /// red — which is exactly the ordering bug it exists to catch.
    #[test]
    fn an_unrecoverable_body_is_never_cancelled() {
        let store = MemStateStore::default();
        let broker = SpyBroker::default();
        let out = run(
            &broker,
            &store,
            &TestSrc::Unrecoverable,
            adjust(),
            at("2026-07-22T13:30:00Z"),
        )
        .expect("skip is not an error");
        assert_eq!(out, RepriceOutcome::Skipped("no-recoverable-body"));
        assert_eq!(
            broker.cancels.borrow().len(),
            0,
            "never cancel an order you cannot re-place",
        );
    }

    /// An expired signed window likewise leaves the order resting: re-placing
    /// it would enter on a stale thesis, and cancelling it here would pre-empt
    /// the lifecycle that properly retires it.
    #[test]
    fn an_expired_window_is_left_resting() {
        let store = MemStateStore::default();
        let broker = SpyBroker::default();
        let out = run(
            &broker,
            &store,
            &TestSrc::Expired,
            adjust(),
            at("2026-07-22T13:30:00Z"),
        )
        .expect("skip is not an error");
        assert_eq!(out, RepriceOutcome::Skipped("window-closed"));
        assert_eq!(broker.cancels.borrow().len(), 0);
    }

    /// A demote cancels and parks — and deliberately does NOT re-place. The
    /// broker spy's `place_entry` panics, so a demote that fell through to the
    /// re-place would fail loudly here rather than quietly re-placing a sub-1R
    /// order.
    #[test]
    fn a_demote_cancels_and_parks_without_replacing() {
        let store = MemStateStore::default();
        let broker = SpyBroker::default();
        let now = at("2026-07-22T13:30:00Z");
        let out = run(&broker, &store, &TestSrc::Ok, PendingAction::Demote, now).expect("demote");
        assert_eq!(out, RepriceOutcome::Demoted);
        assert_eq!(
            broker.cancels.borrow().as_slice(),
            ["ord-1"],
            "the sub-1R order must come off the broker",
        );
        assert!(
            pollster::block_on(stored_order(&store, "t-1"))
                .expect("read")
                .is_some(),
            "...but the SETUP must survive as a park",
        );
    }

    /// The failure this module's ordering exists to bound: the cancel succeeds,
    /// the re-place is rejected. The order is gone from the broker, so the setup
    /// MUST be parked — otherwise a rejected re-place silently loses the trade.
    ///
    /// Mutation check: drop the park from the failure arm and this goes red.
    #[test]
    fn a_rejected_replace_parks_rather_than_losing_the_setup() {
        let store = MemStateStore::default();
        let broker = SpyBroker {
            reject_entry: true,
            ..Default::default()
        };
        let now = at("2026-07-22T13:30:00Z");
        let out = run(&broker, &store, &TestSrc::Ok, adjust(), now).expect("reported, not fatal");
        assert!(
            matches!(out, RepriceOutcome::ReplaceFailed(_)),
            "a rejected re-place must be reported distinctly, got {out:?}",
        );
        assert_eq!(broker.cancels.borrow().as_slice(), ["ord-1"]);
        assert!(
            pollster::block_on(stored_order(&store, "t-1"))
                .expect("read")
                .is_some(),
            "the cancelled order's setup must be recoverable",
        );
    }

    /// A cancel that fails leaves the order resting and reports the error — it
    /// must NOT go on to place a second order, which would double the position.
    ///
    /// Mutation check: swallow the cancel error and `places` goes to 1.
    #[test]
    fn a_failed_cancel_does_not_place_a_second_order() {
        let store = MemStateStore::default();
        let broker = SpyBroker {
            fail_cancel: true,
            ..Default::default()
        };
        let err = run(
            &broker,
            &store,
            &TestSrc::Ok,
            adjust(),
            at("2026-07-22T13:30:00Z"),
        )
        .expect_err("a failed cancel is an error");
        assert!(err.contains("cancel_order"), "{err}");
        assert_eq!(
            broker.places.borrow().len(),
            0,
            "placing after a failed cancel would double the order",
        );
    }

    /// The park written by a demote records the SHELL time of the order it
    /// replaced, not `now` — so a promotion is judged against the signal that
    /// actually fired, and the drop clock is anchored to the real expiry.
    #[test]
    fn the_park_carries_the_orders_own_shell_time_and_a_drop_deadline() {
        let store = MemStateStore::default();
        let broker = SpyBroker::default();
        let now = at("2026-07-22T13:30:00Z");
        run(&broker, &store, &TestSrc::Ok, PendingAction::Demote, now).expect("demote");
        let parked = pollster::block_on(stored_order(&store, "t-1"))
            .expect("read")
            .expect("a park");
        assert_eq!(
            parked.shell_time,
            at("2026-07-22T12:00:00Z"),
            "the shell time comes from the recovered order, not the clock",
        );
        // expiry 2026-07-24T00:00 minus 3 H1 bars.
        assert_eq!(parked.drop_at, at("2026-07-23T21:00:00Z"));
    }

    // ---- test doubles -------------------------------------------------------

    /// Records what reached the broker. `place_entry` returns an id (or a
    /// rejection) rather than panicking, because the re-place path legitimately
    /// reaches it — the assertions are on the recorded calls instead.
    #[derive(Default)]
    struct SpyBroker {
        cancels: RefCell<Vec<String>>,
        places: RefCell<Vec<String>>,
        fail_cancel: bool,
        reject_entry: bool,
    }

    impl Broker for SpyBroker {
        async fn place_entry(
            &self,
            _max_risk_pct: f64,
            _max_open_positions: u32,
            req: &EntryRequest<'_>,
        ) -> Result<String, EntryError> {
            self.places.borrow_mut().push(req.instrument.to_string());
            if self.reject_entry {
                // The real rule-4 rejection: price drifted past the trigger
                // between cancel and re-place.
                return Err(EntryError::EntryTooCloseToMarket);
            }
            Ok("ord-2".into())
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
        ) -> Result<AttemptState, LookupError> {
            Ok(AttemptState::Unknown)
        }
        async fn cancel_order(
            &self,
            _account_id: &str,
            broker_order_id: &str,
        ) -> Result<(), CancelError> {
            if self.fail_cancel {
                return Err(CancelError::Transient);
            }
            self.cancels.borrow_mut().push(broker_order_id.to_string());
            Ok(())
        }
        async fn get_quote(&self, _instrument: &str) -> Result<Quote, LookupError> {
            Ok(Quote {
                bid: 1.1000,
                ask: 1.1001,
            })
        }
        async fn list_open_positions(
            &self,
            _account_id: &str,
        ) -> Result<Vec<OpenPosition>, LookupError> {
            Ok(vec![])
        }
        async fn amend_stop(
            &self,
            _account_id: &str,
            _position_or_order_id: &str,
            _new_stop: f64,
        ) -> Result<(), AmendError> {
            Ok(())
        }
        async fn list_pending_orders(
            &self,
            _account_id: &str,
        ) -> Result<Vec<PendingOrder>, LookupError> {
            Ok(vec![])
        }
        async fn get_candles(
            &self,
            _instrument: &str,
            _granularity: Granularity,
            _since: DateTime<Utc>,
            _now: DateTime<Utc>,
        ) -> Result<Vec<Candle>, CandleError> {
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

    /// A minimal valid enter `Verified` for trade `t-1`, whose shell time is
    /// deliberately EARLIER than `now` in the tests so a park that copied the
    /// clock instead of the order's own shell time is visible.
    fn test_verified() -> crate::incoming::Verified {
        use crate::intent::{Intent, Shell};
        let intent: Intent = serde_json::from_str(
            r#"{
                "v": 1,
                "id": "t-1-enter",
                "not_after": "2026-07-24T00:00:00Z",
                "action": "enter",
                "instrument": "EUR_USD",
                "direction": "long",
                "entry": { "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.1000 },
                "stop_loss": { "absolute": 1.0980 },
                "take_profit": { "absolute": 1.1200 },
                "broker": "oanda",
                "trade_id": "t-1",
                "pip_size": 0.0001
            }"#,
        )
        .expect("valid enter intent");
        let shell = Shell::from_candle(&Candle {
            time: at("2026-07-22T12:00:00Z"),
            o: 1.0990,
            h: 1.1005,
            l: 1.0985,
            c: 1.0995,
        });
        crate::incoming::Verified { shell, intent }
    }

    enum TestSrc {
        Ok,
        Expired,
        Unrecoverable,
    }

    impl VerifiedSource for TestSrc {
        async fn recover(
            &self,
            _key: &str,
            _signed_body: Option<&str>,
            _now: DateTime<Utc>,
        ) -> Recovered {
            match self {
                Self::Expired => Recovered::Expired,
                Self::Unrecoverable => Recovered::Unrecoverable,
                Self::Ok => Recovered::Ok(Box::new(test_verified())),
            }
        }
    }
}
