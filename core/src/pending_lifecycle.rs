//! **The one shared resting-order lifecycle** — cancel a resting entry order
//! through a period it must not be exposed to, and re-drive it once that period
//! lifts. Several independent reasons can pull an order, tracked as a refcount of
//! named [`HoldReason`]s (see [`crate::hold`]):
//!
//! - **[`HoldReason::SpreadHour`]** — the instrument entered its baked trough,
//!   keyed off
//!   [`spread_blackout::is_spread_hour`](crate::spread_blackout::is_spread_hour).
//!   Instrument-scoped, a pure function of the clock.
//! - **[`HoldReason::NewsPause`]** — a `pause` standoff is armed for the order's
//!   *trade* ([`pause_active`]). Trade-scoped, read from the store.
//!
//! The news reason exists because a pause otherwise only blocked *new* entries
//! (the `423 trade paused` gate at the head of [`run_enter`]) while leaving an
//! already-resting order to sit through the event and fill on the spike — the
//! 2026-07-30 bug. An entry we would refuse to place during a standoff is an
//! entry we must not leave resting through one either.
//!
//! # One derivation, one predicate (why the refcount)
//!
//! Reasons **overlap** and **lift independently**: a spread hour running 06:30–08:00
//! with a news pause from 07:00 leaves the pause armed when the spread lifts at
//! 08:00. So a reason never cancels or restores directly — it holds and releases,
//! and the order is re-placed on the release that **empties** the set
//! ([`Release::Emptied`], a transition, so exactly once per hold episode).
//!
//! Both sides funnel through one place: [`hold_reasons`] derives the set (ON) and
//! [`release_satisfied`] narrows it (OFF). That replaced two hand-written OR
//! expressions which had to be kept in agreement by hand — an out-of-sync pair
//! cancels an order and re-places it on the very next ~5s tick, straight back into
//! the window it was pulled from. Adding a `HoldReason` variant is now a compile
//! error in [`release_satisfied`] until its release condition is written.
//!
//! Note the set is **named**, not a bare counter: the cron re-evaluates the same
//! conditions every ~5s, so an incrementing count would reach the hundreds within
//! an hour and never return to zero. [`Holders::hold`] is idempotent.
//!
//! # Only THIS module talks to the broker about resting orders
//!
//! There is exactly **one** `list_pending_orders` call on the live path — in
//! [`cancel_pass`]. Neither reason reaches the broker itself; they are
//! [`HoldReason`] variants, not subsystems with their own broker access. Anything
//! new that wants to hold resting orders belongs here as a variant, **not** as
//! another place that enumerates or cancels broker orders.
//!
//! # Per-instrument and per-trade reasons on a per-trade record
//!
//! The two reasons have **different natural scopes**, which is easy to misread as a
//! mismatch:
//!
//! - `SpreadHour` is per-**instrument** (a property of the pair's clock).
//! - `NewsPause` is per-**trade** (keyed `(trade_id, blackout_id)`, no instrument).
//! - The holder set lives on a per-**trade** record.
//!
//! This composes correctly because [`cancel_pass`] iterates **every resting order**
//! and derives [`hold_reasons`] per order. So an instrument-scoped reason is
//! *fanned out* to each affected trade — three EUR/USD trades in one spread hour get
//! three records, each holding `SpreadHour` — rather than stored once per instrument.
//! Release is symmetric: each record re-evaluates `is_spread_hour` against its own
//! `record.instrument`.
//!
//! The consequence worth knowing: a hold is **trade-wide, never per-order**
//! ("pause all this trade's resting orders"), so no caller needs to ask per-order
//! questions and there is deliberately no per-order query API.
//!
//! This is the generic `core` home the live cron
//! (`trade-control-cron::blackout_*`) and the offline replay both call, so the
//! decision runs identically in production and in replay
//! (`[[strategy_changes_in_both_replayer_and_worker]]`). It is generic over a
//! swappable [`Broker`](crate::broker::Broker) (real live / `ReplayBroker` mock)
//! and a swappable [`StateStore`](crate::state::StateStore) (`PgStateStore` live
//! / `MemStateStore` replay), exactly the shape [`run_enter`] and
//! [`retry_gate::evaluate`](crate::retry_gate) already use.
//!
//! # The ON/OFF asymmetry (operator's framing — LOCKED)
//!
//! This asymmetry is about the **spread-hour** reason specifically. The news-pause
//! reason has no such split: a pause is an explicit armed state in the store, so
//! ON and OFF both read the same [`pause_active`] predicate and it is exact in
//! replay and live alike.
//!
//! Turning spread-hour ON and OFF use **different** signals, on purpose:
//!
//! - **ON (cancel resting orders) = baked per-instrument timestamp only.** The
//!   spike's *start* is a learned per-instrument fact ([`is_spread_hour`], baked
//!   mask + 30-min lead). Deterministic → identical in replay and live. **No
//!   live-quote sample decides ON.** (This is the behaviour change vs the older
//!   live cron, which sampled a quote and only cancelled on an elevated spread.)
//! - **OFF (restore resting orders) = live spread recovered OR baked-hour ended
//!   OR 3h backstop.** The spike's *duration* is variable — only the **live**
//!   spread knows when it truly calmed. The live worker samples the spread *for
//!   recovery only* and un-blocks as soon as it recovers, possibly before the
//!   nominal hour ends. Replay has no ticks, so it uses baked-hour-ended as its
//!   off-signal. Both converge; when live recovers early it is at most one hour
//!   ahead — an idealised-vs-live delta, not a divergence.
//!
//! # Safety rails carried VERBATIM from the live cron (do NOT optimise out)
//!
//! Relocated from `blackout_cancel` / `blackout_watch` / `blackout_restore`.
//! Each is load-bearing on the live money path:
//!
//! 1. **Store BEFORE cancel** ([`try_cancel_one`]). The stored `cancelled_orders`
//!    list is the source of truth for restore — push the `CancelledOrder` and
//!    upsert the record *before* calling `cancel_order`. A crash between the two
//!    leaves a recoverable duplicate; the opposite order risks losing the entry.
//! 2. **No stored body ⇒ never cancel.** An order with no `order:{id}` body can't
//!    be restored, so it is left resting.
//! 3. **Body won't verify ⇒ leave resting.** A stored body that no longer
//!    verifies (window closed / tampered) is unusable — skip without cancelling.
//! 4. **`!applied` ⇒ never touch** ([`recover_one`]). A record the box never
//!    mutated is left alone.
//! 5. **Backstop clears unconditionally** — `now >= opened_at + backstop` clears
//!    regardless of spread, so a stuck record never pins a trade forever.
//! 6. **Restore BEFORE clear** — re-drive the cancelled orders before clearing
//!    the record, or a stranded record re-detects forever.
//! 7. **Re-drive through [`run_enter`]** (never `place_entry`) so every entry
//!    gate + sizing-at-fill + the `recover_entry` fallback re-apply. The re-drive
//!    is the SAME intended entry — it does NOT `mark_seen` (off the HTTP
//!    is_seen path) and single-shot orders consume no retry slot.

use chrono::{DateTime, Duration, Utc};

use crate::blackout_recreate::{RestorePlan, restore_plan};
use crate::broker::{AttemptState, Broker, PendingOrder};
use crate::dispatch::run_enter;
use crate::dispatch_config::DispatchConfig;
use crate::hold::{HoldReason, Holders, Release};
use crate::incoming::{self, IncomingError, Verified};
use crate::intent::Resolved;
use crate::spread_blackout::{
    SAFETY_FORCE_RESTORE_SECONDS, SPREAD_BLACKOUT_RECOVERED_PIPS, is_spread_hour,
    spread_block_ttl_seconds,
};
use crate::state::{CancelledOrder, HeldTradeRecord, StateStore};

/// The one backend seam this function still needs: resolve the per-enter
/// [`DispatchConfig`] (risk caps, pip/tick fallback, per-account caps) at the
/// edge, so [`run_enter`] stays backend-free. The live cron reads
/// `Secrets` + Postgres; the replay returns a fixed offline config. Kept as a
/// tiny trait rather than threading a `CronEnv` through `core` (which can't see
/// the cron crate). Used generically, never boxed.
#[allow(async_fn_in_trait)]
pub trait EnterConfigProvider {
    /// Resolve the dispatch config for a re-driven enter.
    async fn dispatch_config(&self, verified: &Verified) -> DispatchConfig;
}

/// The outcome of recovering the [`Verified`] behind a resting/cancelled order.
/// The two error arms mirror `parse_and_verify`'s meaningful failures so the
/// callers can distinguish "drop, the window closed" from "leave resting, can't
/// trust it".
pub enum Recovered {
    /// The authentic intent+shell to cancel or re-drive.
    Ok(Box<Verified>),
    /// The signed window closed during the blackout (`Expired`/`StaleShellTime`)
    /// — on re-drive: drop the order; on cancel: leave it resting.
    Expired,
    /// No recoverable payload (no stored body, or it won't verify / is
    /// tampered). Leave the order resting — never cancel what can't be restored.
    Unrecoverable,
}

/// The seam that turns a resting order into the [`Verified`] the lifecycle needs
/// — the ONE place the live/replay split lives on the payload side.
///
/// - **Live:** `parse_and_verify` the HMAC-signed body the worker stored under
///   `order:{id}` (untrusted-wire authentication, a live-only concern).
/// - **Replay:** hand back the `Verified` the fake broker was *armed* with when
///   it "placed" the order. The offline replay has the intent+shell in hand
///   already (`ArmedPlacement`) — which is exactly what `parse_and_verify`
///   *produces* — so it needs no signing round-trip and no stored body.
///
/// `recover` is asked once per order id; the impl owns where the payload comes
/// from (store read vs armed map), so RAIL 2 ("no recoverable payload ⇒ never
/// cancel") is expressed uniformly as [`Recovered::Unrecoverable`].
#[allow(async_fn_in_trait)]
pub trait VerifiedSource {
    /// Recover the `Verified` behind `order_id`. `signed_body` is the payload the
    /// caller has on hand for this order (the store's `order:{id}` row on the
    /// cancel side, or the `CancelledOrder.signed_intent` on the re-drive side);
    /// the live impl verifies it, the replay impl ignores it in favour of its
    /// armed map keyed by `order_id`.
    async fn recover(
        &self,
        order_id: &str,
        signed_body: Option<&str>,
        now: DateTime<Utc>,
    ) -> Recovered;
}

/// The live [`VerifiedSource`]: `parse_and_verify` the stored HMAC body with the
/// worker's signing key. This is today's behaviour, made explicit as the seam.
pub struct SignedBodySource<'k> {
    /// The HMAC signing key the HTTP path verifies with.
    pub key: &'k [u8],
}

impl VerifiedSource for SignedBodySource<'_> {
    async fn recover(
        &self,
        _order_id: &str,
        signed_body: Option<&str>,
        now: DateTime<Utc>,
    ) -> Recovered {
        let Some(body) = signed_body else {
            return Recovered::Unrecoverable;
        };
        match incoming::parse_and_verify(body, self.key, now) {
            Ok(v) => Recovered::Ok(Box::new(v)),
            Err(IncomingError::Expired) | Err(IncomingError::StaleShellTime) => Recovered::Expired,
            Err(_) => Recovered::Unrecoverable,
        }
    }
}

/// The forex pip-size fallback used only to resolve absolute prices during the
/// fill-side pre-check when neither the intent nor the record carries a usable
/// pip. Mirrors `trade-control-cron::constants::DEFAULT_PIP_SIZE`.
const DEFAULT_PIP_SIZE: f64 = 0.0001;

/// What one lifecycle pass did, so the replay report can render the same lines
/// the live path logs and a test can assert the outcome without scraping logs.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LifecycleReport {
    /// `order_id`s cancelled + backed up this pass (ON).
    pub cancelled: Vec<String>,
    /// `trade_id`s whose record was cleared this pass (OFF), with the reason.
    pub restored: Vec<(String, RestoreReason)>,
    /// `order_id`s examined but left resting (no body, won't verify, or neither
    /// ON reason fired — no spread hour and no armed pause) — for visibility,
    /// not action.
    pub skipped: Vec<String>,
    /// `order_id`s the broker refused to cancel, with what a follow-up lookup
    /// said about them. **Never silently empty:** before this existed a failed
    /// cancel appeared in *none* of the report's lists, so an order the record
    /// claimed was pulled — but which was still live at the broker, or had
    /// already filled — was invisible to the caller.
    ///
    /// A [`CancelOutcome::Vanished`] entry has already been dropped from the
    /// record, so the OFF side will not re-drive it.
    pub cancel_failed: Vec<(String, CancelOutcome)>,
}

/// What a follow-up lookup found after `cancel_order` returned an error.
///
/// [`CancelError`](crate::broker::CancelError) is a single `Transient` variant
/// that deliberately folds "order already gone" together with a network blip —
/// its own docs say to treat the failure as "probably filled" and **re-lookup**.
/// So the distinction is resolved where the docs say to resolve it, via
/// [`Broker::lookup_attempt_state`], and this is the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The order is **no longer resting** at the broker — it filled, or was
    /// cancelled/expired/rejected out-of-band. There is nothing to restore, so it
    /// is removed from the record: leaving it would have the OFF side re-place an
    /// entry whose original already filled.
    Vanished(&'static str),
    /// The order is **still resting** at the broker — the cancel genuinely failed
    /// (network, 5xx). Left on the record: the next tick retries the cancel, and
    /// the hold still applies.
    StillResting,
    /// The lookup itself failed, so we can't tell. Treated as `StillResting`
    /// (conservative: keep the record entry, retry next tick) but reported
    /// separately so a persistent lookup outage is visible rather than looking
    /// like a stream of live orders.
    Unresolved,
}

impl CancelOutcome {
    /// Should the [`CancelledOrder`] be removed from the record?
    fn drops_from_record(&self) -> bool {
        matches!(self, Self::Vanished(_))
    }

    /// Operator-facing label for the log line.
    fn as_str(&self) -> &'static str {
        match self {
            Self::Vanished(why) => why,
            Self::StillResting => "still-resting",
            Self::Unresolved => "lookup-failed",
        }
    }
}

/// Why a record cleared on the OFF side. Ordered by precedence in
/// [`recover_one`]: backstop is checked first, then recovery/baked-hour-end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreReason {
    /// The 3h backstop fired — clear regardless of spread (safety rail 5).
    Backstop,
    /// The spread recovered (live) or the baked spread hour ended (replay).
    Recovered,
}

/// Who owns deleting the per-trade [`HeldTradeRecord`] after the OFF-side
/// restore. The record can carry BOTH System 3 (cancelled resting orders, which
/// this fn restores) AND System 2 (widened open-position stops, which this fn
/// does NOT touch). Whoever restores System 2 must clear the record — so the
/// caller declares the ownership:
///
/// - [`ClearPolicy::ClearRecord`] (default) — this fn deletes the record after
///   restoring System 3. The **replay** owner: it has no System 2, is the sole
///   record owner, and today's clearing behaviour is byte-identical.
/// - [`ClearPolicy::LeaveForCaller`] — this fn restores System 3 but LEAVES the
///   record for the caller to delete. The **live watcher** owner: it restores
///   System 2 (widened stops) alongside and issues the single `clear` itself, so
///   the coexistence contract ("restore both, clear once") is preserved. Without
///   this, the shared fn would delete a System-2-carrying record before its
///   widened stops were restored — leaving an open position's SL widened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearPolicy {
    /// Delete the record after restoring System 3 (default; replay).
    ClearRecord,
    /// Restore System 3 but leave the record for the caller to delete (live
    /// watcher, which also restores System 2 then clears once).
    LeaveForCaller,
}

/// Run one resting-order lifecycle pass for a single `(broker, account)` at
/// `now`. Cancels resting orders that entered a spread hour and re-drives
/// records whose trough has lifted. The caller (live cron / replay loop) owns
/// the per-account fan-out and passes the already-acquired broker + signing key
/// in — mirroring how [`run_enter`] is per-enter and the cron loops accounts
/// around it.
///
/// `clear` declares who deletes the record on the OFF side — see [`ClearPolicy`].
/// Replay passes `ClearRecord` (sole owner); the live watcher passes
/// `LeaveForCaller` so it can restore System 2 then clear once.
///
/// The OFF-side live-spread recovery reads through `broker.get_quote`: the live
/// worker's real broker returns the current spread (so it can un-block early);
/// the replay `ReplayBroker`'s synthesised quote inside the baked hour keeps the
/// order held until the baked hour ends (its deterministic off-signal). Same
/// function, the broker supplies the recovery signal — see the module's ON/OFF
/// asymmetry.
pub async fn pending_order_lifecycle<B, S, P, V>(
    broker: &B,
    store: &S,
    cfg_provider: &P,
    src: &V,
    account: Option<&str>,
    now: DateTime<Utc>,
    clear: ClearPolicy,
) -> LifecycleReport
where
    B: Broker,
    S: StateStore,
    P: EnterConfigProvider,
    V: VerifiedSource,
{
    let mut report = LifecycleReport::default();
    cancel_pass(broker, store, src, account, now, &mut report).await;
    recover_pass(
        broker,
        store,
        cfg_provider,
        src,
        account,
        now,
        clear,
        &mut report,
    )
    .await;
    report
}

// --- ON side: cancel + back up (baked clock only, no live quote) ---

/// Enumerate resting orders and cancel each that has entered a spread hour.
async fn cancel_pass<B: Broker, S: StateStore, V: VerifiedSource>(
    broker: &B,
    store: &S,
    src: &V,
    account: Option<&str>,
    now: DateTime<Utc>,
    report: &mut LifecycleReport,
) {
    let account_id = account.unwrap_or("");
    let pendings = match broker.list_pending_orders(account_id).await {
        Ok(p) => p,
        Err(err) => {
            tracing::error!("pending-lifecycle[{account_id}]: list_pending_orders: {err:?}");
            return;
        }
    };
    for order in &pendings {
        try_cancel_one(broker, store, src, account, order, now, report).await;
    }
}

/// Evaluate **every** hold reason for one order at `now` — the single place the
/// reason set is derived, for both the ON and the OFF side.
///
/// This is what replaced the two hand-written OR expressions (one in the cancel
/// path, one in `off_now`). Having a single derivation is the point: a third
/// reason added here is automatically honoured by both sides, so the
/// cancel-then-immediately-restore bug that an out-of-sync pair produces cannot
/// be written. Adding a `HoldReason` variant without extending this fn is a
/// non-exhaustive `match` — a compile error.
///
/// The cheap instrument-only spread-hour test is evaluated first, but note both
/// reasons are always computed: the OFF side needs the *full* set to know whether
/// anything still holds, not just whether one reason fired.
async fn hold_reasons<S: StateStore>(
    store: &S,
    instrument: &str,
    trade_id: &str,
    now: DateTime<Utc>,
) -> Holders {
    let mut holders = Holders::new();
    // SpreadHour — the pure baked clock, incl. the 30-min lead + NY-close
    // fallback. NO live quote (the ON/OFF asymmetry: the quote only ever
    // *releases* this reason early, in `spread_hour_released`).
    if is_spread_hour(instrument, now) {
        holders.hold(HoldReason::SpreadHour);
    }
    // NewsPause — a standoff armed for THIS trade.
    if pause_active(store, trade_id).await {
        holders.hold(HoldReason::NewsPause);
    }
    holders
}

/// Cancel + store a single resting order. Store-before-cancel (safety rail 1);
/// no-recoverable-payload / won't-verify ⇒ leave resting (rails 2, 3).
///
/// The ON trigger is evaluated **inside** this fn rather than by the caller,
/// because the news-pause half is keyed by `trade_id` — which only exists once
/// the payload has been recovered through the [`VerifiedSource`] seam. The cheap
/// instrument-only spread-hour test still short-circuits first, so a clean bar
/// with no pause costs exactly one `get_order_body` read per resting order.
async fn try_cancel_one<B: Broker, S: StateStore, V: VerifiedSource>(
    broker: &B,
    store: &S,
    src: &V,
    account: Option<&str>,
    order: &PendingOrder,
    now: DateTime<Utc>,
    report: &mut LifecycleReport,
) {
    let scope = account.unwrap_or("<global>");

    // The payload the live impl verifies: the store's `order:{id}` body. The
    // replay impl ignores it (uses its armed map). A store error is skip (can't
    // safely proceed). `None`/`Some` both flow into the seam, which decides
    // recoverability uniformly (RAIL 2).
    let stored_body = match store.get_order_body(&order.order_id).await {
        Ok(b) => b,
        Err(err) => {
            tracing::error!(
                "pending-lifecycle[{scope}]: get_order_body({}) failed: {err}; skip",
                order.order_id,
            );
            return;
        }
    };

    // RAILS 2 + 3 — recover the Verified via the seam. Unrecoverable (no body /
    // won't verify) or Expired ⇒ leave the order resting (never cancel what we
    // can't restore). `Ok` also recovers the trade_id (record key) + pip_size
    // (baked onto the record for the OFF-side pips math).
    let verified = match src
        .recover(&order.order_id, stored_body.as_deref(), now)
        .await
    {
        Recovered::Ok(v) => *v,
        Recovered::Expired | Recovered::Unrecoverable => {
            tracing::info!(
                "pending-lifecycle[{scope}]: order {} has no recoverable/valid payload — leaving \
                 it resting",
                order.order_id,
            );
            report.skipped.push(order.order_id.clone());
            return;
        }
    };
    // The signed payload to persist on the record for the re-drive side. Live:
    // the verified body. Replay: a placeholder — the replay's re-drive source
    // keys off the armed map by order_id, not this string.
    let signed_intent =
        stored_body.unwrap_or_else(|| format!("replay-order: {}\n", order.order_id));
    let trade_id = verified
        .intent
        .trade_id
        .clone()
        .unwrap_or_else(|| order.order_id.clone());

    // ON trigger — derive EVERY hold reason (one shared derivation, see
    // `hold_reasons`). Nothing holding ⇒ leave the order resting. Note the full
    // set is persisted, not just the first reason that fired: overlapping reasons
    // must each be released before the re-place, which is the whole point of the
    // refcount.
    let holders = hold_reasons(store, &order.instrument, &trade_id, now).await;
    if holders.is_empty() {
        report.skipped.push(order.order_id.clone());
        return;
    }

    let Some(pip_size) = verified
        .intent
        .pip_size
        .filter(|p| *p > 0.0 && p.is_finite())
    else {
        tracing::info!(
            "pending-lifecycle[{scope}]: order {} (trade {trade_id}) has no usable pip_size; skip",
            order.order_id,
        );
        report.skipped.push(order.order_id.clone());
        return;
    };

    // RAIL 1 — STORE FIRST (crash-safe): merge a CancelledOrder onto the
    // per-trade record, set `applied`, preserve any widened-stop originals,
    // and upsert BEFORE cancelling.
    let existing = match store.get_held_trade_record(&trade_id).await {
        Ok(r) => r,
        Err(err) => {
            tracing::error!(
                "pending-lifecycle[{scope}]: get_record({trade_id}): {err}; skip (won't cancel \
                 without a durable record)",
            );
            return;
        }
    };
    let record = merge_cancelled_order(
        existing,
        &trade_id,
        &order.instrument,
        account,
        pip_size,
        CancelledOrder {
            order_id: order.order_id.clone(),
            signed_intent,
        },
        &holders,
        now,
    );
    // TTL = block length + grace (concern 1), keyed off the record's own
    // `opened_at` so it matches the `expires_at` the merge stamped.
    let ttl = spread_block_ttl_seconds(&order.instrument, record.opened_at);
    if let Err(err) = store.upsert_held_trade_record(&record, ttl).await {
        tracing::error!(
            "pending-lifecycle[{scope}]: upsert_record({trade_id}) FAILED ({err}); NOT cancelling \
             (no durable record ⇒ would strand the order)",
        );
        return;
    }

    // Now cancel. A failure leaves the (idempotent) record in place; the
    // recovery re-drive of a still-live order is bounded by its own gates.
    match broker
        .cancel_order(account_id_of(account), &order.order_id)
        .await
    {
        Ok(()) => {
            tracing::info!(
                "pending-lifecycle[{scope}][{trade_id}]: cancelled resting {} order {} \
                 (trigger={}, held_by=[{}])",
                if order.is_stop { "stop" } else { "limit" },
                order.order_id,
                order.trigger,
                holders.describe(),
            );
            report.cancelled.push(order.order_id.clone());
        }
        Err(err) => {
            // The cancel failed. `CancelError` folds "already gone" in with a
            // network blip, so resolve which it was the way its own docs say to —
            // re-lookup — and act on the answer. Doing nothing here (the old
            // behaviour) left the record claiming an order was pulled when it had
            // actually FILLED, and the OFF side would then re-place an entry for a
            // trade that was already in a position.
            let outcome = classify_cancel_failure(broker, order).await;
            tracing::error!(
                "pending-lifecycle[{scope}][{trade_id}]: cancel order {} FAILED ({err:?}); \
                 lookup says {} — {}",
                order.order_id,
                outcome.as_str(),
                if outcome.drops_from_record() {
                    "dropping it from the record (nothing to restore)"
                } else {
                    "record stays, cancel retries next tick"
                },
            );
            if outcome.drops_from_record() {
                drop_cancelled_order(store, &record, &order.order_id, ttl).await;
            }
            report.cancel_failed.push((order.order_id.clone(), outcome));
        }
    }
}

/// Resolve a failed cancel into a [`CancelOutcome`] via
/// [`Broker::lookup_attempt_state`] — the re-lookup `CancelError`'s docs call for.
///
/// `Pending` is the only state that means "genuinely still resting, retry"; every
/// filled-or-dead state means the order is gone and must not be restored. A lookup
/// error is `Unresolved`, treated conservatively as still-resting.
async fn classify_cancel_failure<B: Broker>(broker: &B, order: &PendingOrder) -> CancelOutcome {
    match broker
        .lookup_attempt_state(&order.instrument, &order.order_id, None)
        .await
    {
        Ok(AttemptState::Pending) => CancelOutcome::StillResting,
        // Filled — restoring would double up on a live/finished position.
        Ok(AttemptState::OpenPosition { .. }) => CancelOutcome::Vanished("filled-open-position"),
        Ok(AttemptState::ClosedWin { .. }) | Ok(AttemptState::ClosedLossOrBreakeven { .. }) => {
            CancelOutcome::Vanished("filled-and-closed")
        }
        // Dead without filling (rejected / expired / cancelled upstream).
        Ok(AttemptState::Cancelled) => CancelOutcome::Vanished("cancelled-upstream"),
        // Not found anywhere: we lost track. Still "not resting", so the same
        // no-restore conclusion holds — logged distinctly, per AttemptState's doc.
        Ok(AttemptState::Unknown) => CancelOutcome::Vanished("not-found"),
        Err(err) => {
            tracing::error!(
                "pending-lifecycle: lookup_attempt_state({}) failed ({err:?}); assuming still \
                 resting (conservative — keeps the record so the cancel retries)",
                order.order_id,
            );
            CancelOutcome::Unresolved
        }
    }
}

/// Remove one [`CancelledOrder`] from the record after its cancel failed because
/// the order is gone from the broker.
///
/// Best-effort: a write failure leaves the entry in place, which is the same state
/// the old code always left it in — the next tick re-derives. If the record has no
/// other cancelled orders and no widened stops it is cleared outright rather than
/// left as an empty shell that the OFF side would keep re-examining.
async fn drop_cancelled_order<S: StateStore>(
    store: &S,
    record: &HeldTradeRecord,
    order_id: &str,
    ttl: u64,
) {
    let mut pruned = record.clone();
    pruned.cancelled_orders.retain(|c| c.order_id != order_id);

    let result = if pruned.cancelled_orders.is_empty() && pruned.original_stops.is_empty() {
        // Nothing left to restore on either system — don't leave a shell behind.
        store.clear_held_trade_record(&pruned.trade_id).await
    } else {
        store.upsert_held_trade_record(&pruned, ttl).await
    };
    if let Err(err) = result {
        tracing::error!(
            "pending-lifecycle[{}]: pruning vanished order {order_id} FAILED ({err}); it stays on \
             the record and the OFF side may try to re-place it",
            pruned.trade_id,
        );
    }
    // The stored signed body is dead weight once the order is gone.
    cleanup_body(store, order_id).await;
}

fn account_id_of(account: Option<&str>) -> &str {
    account.unwrap_or("")
}

/// Pure record merge: push `cancelled` onto a fresh-or-existing record, set
/// `applied = true`, **union `holding` onto the record's holder set**, and
/// preserve any widened-stop `original_stops`. Idempotent: re-cancelling the same
/// order id de-dups, and re-holding a reason already present is a no-op (which is
/// what makes the ~5s polling cron safe).
///
/// The holder union is why a second reason arriving mid-block is additive rather
/// than a replacement: a spread hour that starts at 06:30 and a pause that arrives
/// at 07:00 leave the record holding **both**, so the 08:00 spread lift alone does
/// not restore the order.
#[allow(clippy::too_many_arguments)]
fn merge_cancelled_order(
    existing: Option<HeldTradeRecord>,
    trade_id: &str,
    instrument: &str,
    account: Option<&str>,
    pip_size: f64,
    cancelled: CancelledOrder,
    holding: &Holders,
    now: DateTime<Utc>,
) -> HeldTradeRecord {
    let mut record = existing.unwrap_or_else(|| HeldTradeRecord {
        trade_id: trade_id.to_string(),
        instrument: instrument.to_string(),
        account: account.map(|s| s.to_string()),
        applied: false,
        holders: Holders::new(),
        opened_at: now,
        // Placeholder — overwritten below from the block-length TTL.
        expires_at: now,
        pip_size,
        original_stops: Vec::new(),
        cancelled_orders: Vec::new(),
    });
    record.applied = true;
    // Union, not replace — see the fn doc. `hold` is idempotent, so a reason that
    // re-asserts itself every tick doesn't accumulate.
    for reason in holding.iter() {
        record.holders.hold(reason);
    }
    // Concern 1: the record must OUTLIVE its own spread-hour block so the
    // block-lift restore can find it. Size the TTL from the block length off the
    // (possibly-preserved) `opened_at`, not a flat backstop.
    record.expires_at = record.opened_at
        + Duration::seconds(spread_block_ttl_seconds(instrument, record.opened_at) as i64);
    if !(record.pip_size > 0.0 && record.pip_size.is_finite()) {
        record.pip_size = pip_size;
    }
    if !record
        .cancelled_orders
        .iter()
        .any(|c| c.order_id == cancelled.order_id)
    {
        record.cancelled_orders.push(cancelled);
    }
    record
}

// --- OFF side: recover (restore before clear) ---

/// Walk the per-trade records **for this account** and, for each `applied` one
/// whose trough has lifted, re-drive its cancelled orders then (under
/// `ClearRecord`) clear it.
///
/// Account-scoped, symmetric with [`cancel_pass`] (which scopes on
/// `list_pending_orders(account_id)`): the caller passes ONE account's broker,
/// so recover must only touch THAT account's records — else the live multi-account
/// cron would `off_now`/re-drive account-Y's records against account-X's broker.
/// `store.list_all_held_trade_records` is store-wide, so we filter by
/// `record.account == account`. The replay passes `account = None` and its records
/// carry `account = None`, so its behaviour is unchanged.
#[allow(clippy::too_many_arguments)]
async fn recover_pass<B: Broker, S: StateStore, P: EnterConfigProvider, V: VerifiedSource>(
    broker: &B,
    store: &S,
    cfg_provider: &P,
    src: &V,
    account: Option<&str>,
    now: DateTime<Utc>,
    clear_policy: ClearPolicy,
    report: &mut LifecycleReport,
) {
    let records = match store.list_all_held_trade_records().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("pending-lifecycle: list records failed: {err}");
            return;
        }
    };
    for record in records {
        if record.account.as_deref() != account {
            continue;
        }
        recover_one(
            broker,
            store,
            cfg_provider,
            src,
            &record,
            now,
            clear_policy,
            report,
        )
        .await;
    }
}

/// Per-record OFF decision + clear. `!applied` ⇒ untouched (rail 4); backstop
/// clears unconditionally (rail 5); otherwise recovery (live spread) or the
/// baked spread hour ending clears it. Restore precedes clear (rail 6).
///
/// `clear_policy` decides who deletes the record after the System-3 restore (see
/// [`ClearPolicy`]): `ClearRecord` deletes it here (replay); `LeaveForCaller`
/// leaves it for the live watcher to delete after it also restores System 2.
#[allow(clippy::too_many_arguments)]
async fn recover_one<B: Broker, S: StateStore, P: EnterConfigProvider, V: VerifiedSource>(
    broker: &B,
    store: &S,
    cfg_provider: &P,
    src: &V,
    record: &HeldTradeRecord,
    now: DateTime<Utc>,
    clear_policy: ClearPolicy,
    report: &mut LifecycleReport,
) {
    // RAIL 4 — never touch what you didn't apply.
    if !record.applied {
        return;
    }

    // NORMAL OFF trigger FIRST — release every satisfied reason. The restore fires
    // only on the release that EMPTIES the holder set, so overlapping reasons each
    // have to lift first. This is the path that should restore AUD/CHF at the
    // 05:00Z block lift; because the record TTL now outlives its block (concern 1),
    // this wins BEFORE any expiry and long before the safety ceiling.
    let (surviving, emptied) = release_satisfied(broker, store, record, now).await;
    if emptied {
        // RAIL 6 — restore BEFORE clear.
        restore_cancelled_orders(broker, store, cfg_provider, src, record, now).await;
        finish_recover(
            store,
            record,
            clear_policy,
            RestoreReason::Recovered,
            report,
        )
        .await;
        return;
    }

    // Still held by at least one reason. Persist the *narrowed* set so a partial
    // release is durable — otherwise a reason that lifted would be re-derived as
    // held on every subsequent tick, and (worse) a crash-restart would lose the
    // fact that it had already released. Only write when it actually changed, to
    // keep the ~5s cron from rewriting an unchanged row forever.
    if surviving != record.holders {
        let mut narrowed = record.clone();
        narrowed.holders = surviving.clone();
        let ttl = spread_block_ttl_seconds(&record.instrument, record.opened_at);
        if let Err(err) = store.upsert_held_trade_record(&narrowed, ttl).await {
            // Non-fatal: the reason will be re-evaluated next tick and the backstop
            // still bounds the worst case. Loud, because a persistently failing
            // write means partial releases aren't sticking.
            tracing::error!(
                "pending-lifecycle[{}]: narrowing holders to [{}] FAILED ({err}); will re-derive \
                 next tick",
                record.trade_id,
                surviving.describe(),
            );
        } else {
            tracing::info!(
                "pending-lifecycle[{}]: still held by [{}] — not restoring yet",
                record.trade_id,
                surviving.describe(),
            );
        }
    }

    // SAFETY force-restore (last resort) — a record still `applied` a very long
    // time after `opened_at`, i.e. the normal `off_now` restore above never
    // cleared it (a persistent quote-error storm, a repeatedly-failing `clear`,
    // or a mis-baked over-long mask that never reports a lift). The timer
    // (SAFETY_FORCE_RESTORE_SECONDS = 12h) is deliberately LONGER than any
    // realistic block, so by the time it fires we are past any legitimate block —
    // it cannot force-restore into an active block the way the old 3h ceiling did
    // (21:00+3h=00:00Z, mid-AUD/CHF's-8h-block). Belt-and-braces: for a normal
    // block it never fires because `off_now` restores at the lift first. A stuck
    // record is force-cleared rather than pinning the trade forever.
    if backstop_due(record.opened_at, now) {
        restore_cancelled_orders(broker, store, cfg_provider, src, record, now).await;
        finish_recover(store, record, clear_policy, RestoreReason::Backstop, report).await;
    }
}

/// The tail of a successful OFF-side System-3 restore: clear the record (only
/// under [`ClearPolicy::ClearRecord`]) and record the restore in the report.
///
/// Under `LeaveForCaller` the record is deliberately NOT deleted here — the live
/// watcher restores System 2 (widened stops) then issues the single clear itself
/// (Option A). The `report.restored` push happens either way: the System-3
/// restore DID occur, and the report is the caller's signal that it did.
async fn finish_recover<S: StateStore>(
    store: &S,
    record: &HeldTradeRecord,
    clear_policy: ClearPolicy,
    reason: RestoreReason,
    report: &mut LifecycleReport,
) {
    match clear_policy {
        ClearPolicy::ClearRecord => {
            if clear(store, record).await {
                report.restored.push((record.trade_id.clone(), reason));
            }
        }
        ClearPolicy::LeaveForCaller => {
            report.restored.push((record.trade_id.clone(), reason));
        }
    }
}

/// Is a news standoff (`pause`) currently armed for this trade?
///
/// The second ON reason alongside `is_spread_hour`, and a veto on OFF. A pause is
/// keyed `(trade_id, blackout_id)` and carries **no instrument**
/// ([`PauseEntry`](crate::state::PauseEntry)) — deliberately, since it targets one
/// setup rather than every order on the pair — so it can only be reached through
/// a trade_id, which both call sites already have in hand (the ON side off the
/// recovered `Verified`, the OFF side off the record).
///
/// A store error is treated as **paused** (`true`): on the ON side that is the
/// safe direction only because the cancel is separately gated by rails 1–3, and
/// on the OFF side it defers the restore by one tick rather than re-placing an
/// order into a standoff we can't currently see. The 12h backstop remains the
/// escape hatch if the store stays unreadable.
async fn pause_active<S: StateStore>(store: &S, trade_id: &str) -> bool {
    match store.list_pauses_for_trade(trade_id).await {
        Ok(pauses) => !pauses.is_empty(),
        Err(err) => {
            tracing::error!(
                "pending-lifecycle: list_pauses_for_trade({trade_id}) failed ({err}); treating as \
                 PAUSED (hold the order rather than expose it to an unseen standoff)",
            );
            true
        }
    }
}

/// Has the `SpreadHour` reason released?
///
/// The documented ON/OFF asymmetry, carried verbatim: turning **on** is the pure
/// baked clock, but turning **off** is the **baked hour ending OR the live spread
/// recovering** — the live worker samples the quote so it can un-block early,
/// possibly before the nominal hour ends. Replay's synthesised quote inside the
/// hour keeps it held until the baked hour ends, its deterministic off-signal.
/// A quote error means "not yet recovered", so we wait for the hour end / backstop.
async fn spread_hour_released<B: Broker>(
    broker: &B,
    record: &HeldTradeRecord,
    now: DateTime<Utc>,
) -> bool {
    // Baked-hour-end — the deterministic off-signal (replay + live).
    if !is_spread_hour(&record.instrument, now) {
        return true;
    }
    // Live-spread recovery — the early un-block, still inside the baked hour.
    match broker.get_quote(&record.instrument).await {
        Ok(quote) => spread_recovered(spread_in_pips(quote.spread(), record.pip_size)),
        Err(_) => false,
    }
}

/// The OFF-side decision (excluding the backstop, handled by the caller):
/// **release every reason that is now satisfied, and restore only if that empties
/// the holder set.**
///
/// This is the shared refcount doing its job. Each reason owns its own release
/// condition — `SpreadHour` via [`spread_hour_released`] (baked-hour-end or live
/// recovery), `NewsPause` via the absence of a pause row — and they lift
/// **independently**. The operator's case: a 06:30–08:00 spread hour with a pause
/// from 07:00 releases `SpreadHour` at 08:00 but stays held by `NewsPause`, so the
/// order is not re-placed into the standoff.
///
/// Returns the surviving holder set and whether the release **emptied** it (the
/// restore trigger — a transition, so it fires exactly once per hold episode).
/// Adding a [`HoldReason`] variant forces a new arm here: that non-exhaustive
/// `match` is the compile error that keeps ON and OFF from drifting apart.
async fn release_satisfied<B: Broker, S: StateStore>(
    broker: &B,
    store: &S,
    record: &HeldTradeRecord,
    now: DateTime<Utc>,
) -> (Holders, bool) {
    let mut holders = effective_holders(record);
    let mut emptied = false;
    for reason in holders.clone().iter() {
        let released = match reason {
            HoldReason::SpreadHour => spread_hour_released(broker, record, now).await,
            HoldReason::NewsPause => !pause_active(store, &record.trade_id).await,
        };
        if released && holders.release(reason) == Release::Emptied {
            emptied = true;
        }
    }
    (holders, emptied)
}

/// The holder set to reason about, healing a **pre-v120 row**.
///
/// A record written before the refcount existed has `applied: true` but no
/// `holders` (the field decodes to empty via `#[serde(default)]`). Treating that
/// as "nothing holds it" would restore every in-flight order on the first tick
/// after deploy — re-placing them into spread hours and news standoffs that are
/// still live. So an `applied` record with an empty set is read as holding
/// `{SpreadHour}`, the pre-v120 meaning: it then re-derives normally on this tick
/// and restores only if that reason has genuinely released.
///
/// Only reachable during the deploy window (holder rows are TTL'd, minutes to
/// hours), but the failure mode it prevents is placing live orders into a blackout.
fn effective_holders(record: &HeldTradeRecord) -> Holders {
    if record.applied && record.holders.is_empty() {
        let mut healed = Holders::new();
        healed.hold(HoldReason::SpreadHour);
        return healed;
    }
    record.holders.clone()
}

/// Re-drive (or drop) every cancelled resting order on a record. Relocated from
/// `blackout_restore`. Per-order errors log + skip so the clear still proceeds.
async fn restore_cancelled_orders<
    B: Broker,
    S: StateStore,
    P: EnterConfigProvider,
    V: VerifiedSource,
>(
    broker: &B,
    store: &S,
    cfg_provider: &P,
    src: &V,
    record: &HeldTradeRecord,
    now: DateTime<Utc>,
) {
    for cancelled in &record.cancelled_orders {
        if let Err(err) =
            restore_one_order(broker, store, cfg_provider, src, record, cancelled, now).await
        {
            tracing::error!(
                "pending-lifecycle restore[{}]: order {} re-drive error: {err}",
                record.trade_id,
                cancelled.order_id,
            );
        }
    }
}

/// Re-drive or drop one cancelled order. Returns `Err` only for genuinely
/// unexpected failures; every *expected* drop path returns `Ok(())` after
/// logging, so the watcher treats them as handled. Relocated verbatim from
/// `blackout_restore::restore_one_order` (RAIL 7).
#[allow(clippy::too_many_arguments)]
async fn restore_one_order<B: Broker, S: StateStore, P: EnterConfigProvider, V: VerifiedSource>(
    broker: &B,
    store: &S,
    cfg_provider: &P,
    src: &V,
    record: &HeldTradeRecord,
    cancelled: &CancelledOrder,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let tid = &record.trade_id;

    // 1. Reconstruct an authentic Verified via the seam (live: parse+verify the
    //    stored body; replay: the armed Verified for this order_id).
    let verified = match src
        .recover(&cancelled.order_id, Some(&cancelled.signed_intent), now)
        .await
    {
        Recovered::Ok(v) => *v,
        Recovered::Expired => {
            tracing::info!(
                "pending-lifecycle restore[{tid}]: stored intent expired, dropped order {} \
                 (window closed during blackout)",
                cancelled.order_id,
            );
            cleanup_body(store, &cancelled.order_id).await;
            return Ok(());
        }
        Recovered::Unrecoverable => {
            return Err(format!(
                "re-verify stored intent for {}",
                cancelled.order_id
            ));
        }
    };

    // 2. Fill-side pre-check using the pure restore_plan + a fresh quote.
    let pip = verified
        .intent
        .pip_size
        .filter(|p| *p > 0.0 && p.is_finite())
        .or(Some(record.pip_size).filter(|p| *p > 0.0 && p.is_finite()))
        .unwrap_or(DEFAULT_PIP_SIZE);
    let tick = verified.intent.tick_size.unwrap_or(pip);
    let resolved = Resolved::from_intent(&verified.intent, &verified.shell, pip, tick)
        .map_err(|e| format!("resolve: {e}"))?;
    let quote = broker
        .get_quote(&resolved.instrument)
        .await
        .map_err(|e| format!("quote: {e:?}"))?;
    let recover_entry = resolved.recover_entry.as_ref().map(|o| o.action);

    let plan = restore_plan(
        &resolved.entry,
        resolved.direction,
        resolved.stop_loss,
        resolved.take_profit,
        quote.bid,
        quote.ask,
        recover_entry,
    );
    match plan {
        RestorePlan::DropStopOverrunSkip => {
            tracing::info!(
                "pending-lifecycle restore[{tid}]: stop overrun, recover_entry=skip, dropped \
                 order {} (bid={} ask={})",
                cancelled.order_id,
                quote.bid,
                quote.ask,
            );
            cleanup_body(store, &cancelled.order_id).await;
            return Ok(());
        }
        RestorePlan::DropStaleLimit => {
            tracing::info!(
                "pending-lifecycle restore[{tid}]: limit stale (bid/ask wrong side), dropped \
                 order {} (bid={} ask={})",
                cancelled.order_id,
                quote.bid,
                quote.ask,
            );
            cleanup_body(store, &cancelled.order_id).await;
            return Ok(());
        }
        RestorePlan::DropUnexpectedMarket => {
            tracing::info!(
                "pending-lifecycle restore[{tid}]: unexpected resting market order {}, dropped",
                cancelled.order_id,
            );
            cleanup_body(store, &cancelled.order_id).await;
            return Ok(());
        }
        RestorePlan::Redrive => {}
    }

    // 3. Re-drive through run_enter. SAME intended entry — we do NOT mark_seen
    //    (off the HTTP is_seen path) and we pass the signed body so a re-placed
    //    order re-stores its own order:{order_id} row. `restore = true` bypasses
    //    the retry gate: this is a re-placement of the order we cancelled, not a
    //    fresh fire, so it must not be `retry-fire-replay`-rejected on its own
    //    already-seen `shell.time` nor burn a multi-shot slot (RAIL 7).
    let cfg = cfg_provider.dispatch_config(&verified).await;
    let result = run_enter(
        broker,
        store,
        &verified,
        &cfg,
        now,
        Some(&cancelled.signed_intent),
        None,
        true,
    )
    .await;
    tracing::info!(
        "pending-lifecycle restore[{tid}]: re-drive order {} → {}",
        cancelled.order_id,
        result.describe(),
    );
    cleanup_body(store, &cancelled.order_id).await;
    Ok(())
}

/// Best-effort delete of the stored order body once handled. Logged, not fatal.
async fn cleanup_body<S: StateStore>(store: &S, order_id: &str) {
    if let Err(err) = store.delete_order_body(order_id).await {
        tracing::error!("pending-lifecycle restore: delete_order_body({order_id}) failed: {err}");
    }
}

/// Clear the record after restore. Returns `true` on success (for the report).
async fn clear<S: StateStore>(store: &S, record: &HeldTradeRecord) -> bool {
    match store.clear_held_trade_record(&record.trade_id).await {
        Ok(()) => true,
        Err(err) => {
            tracing::error!(
                "pending-lifecycle: clear({}) failed: {err}",
                record.trade_id
            );
            false
        }
    }
}

// --- pure predicates (relocated from blackout_watch, unit-tested) ---

/// Safety force-restore timer: true once `now >= opened_at +
/// SAFETY_FORCE_RESTORE_SECONDS`. This is only the *timer* half of the safety
/// gate — the caller (`recover_one`) ANDs it with `!is_spread_hour` so the
/// force-restore never fires back into an active block.
pub fn backstop_due(opened_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now >= opened_at + Duration::seconds(SAFETY_FORCE_RESTORE_SECONDS as i64)
}

/// Convert an absolute `ask − bid` spread to pips via the record's baked pip.
/// Returns `f64::INFINITY` for an unusable pip so recovery never fires on a
/// bogus division (backstop becomes the only clear).
fn spread_in_pips(spread_abs: f64, pip_size: f64) -> f64 {
    if pip_size > 0.0 && pip_size.is_finite() {
        spread_abs / pip_size
    } else {
        f64::INFINITY
    }
}

/// True when the sampled spread (in pips) has dropped to/under the recovered
/// cutoff — the live-only early-un-block side of the OFF decision.
fn spread_recovered(spread_pips: f64) -> bool {
    spread_pips <= SPREAD_BLACKOUT_RECOVERED_PIPS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountCaps;
    use crate::broker::{
        AmendError, AttemptState, CancelError, Candle, CandleError, EntryError, EntryRequest,
        Granularity, LookupError, OpenPosition, Quote,
    };
    use crate::intent::Direction;
    use crate::state::MemStateStore;
    use std::cell::RefCell;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    /// Drive an async body on the core test runtime (pollster — core has no
    /// tokio dev-dep; matches `retry_gate`'s `run`).
    fn run<F: std::future::Future>(f: F) -> F::Output {
        pollster::block_on(f)
    }

    // --- pure predicates (relocated from blackout_watch) ---

    #[test]
    fn safety_force_restore_due_at_or_after_twelve_hours() {
        // The safety ceiling is now 12h (SAFETY_FORCE_RESTORE_SECONDS), longer
        // than any realistic block so it can't fire mid-block.
        let opened = ts("2026-07-08T21:05:00Z");
        assert!(backstop_due(opened, ts("2026-07-09T09:05:00Z")));
        assert!(backstop_due(opened, ts("2026-07-09T09:05:01Z")));
    }

    #[test]
    fn safety_force_restore_not_due_before_twelve_hours() {
        let opened = ts("2026-07-08T21:05:00Z");
        assert!(!backstop_due(opened, ts("2026-07-09T09:04:59Z")));
        // Notably NOT due at 3h (00:05Z) — the old bug fired here, mid-AUD/CHF-block.
        assert!(!backstop_due(opened, ts("2026-07-09T00:05:00Z")));
        assert!(!backstop_due(opened, ts("2026-07-08T21:20:00Z")));
    }

    #[test]
    fn spread_in_pips_uses_record_pip_size() {
        assert!((spread_in_pips(0.0022, 0.0001) - 22.0).abs() < 1e-9);
        // Unusable pip → INFINITY so recovery never fires on a bogus division.
        assert_eq!(spread_in_pips(0.0022, 0.0), f64::INFINITY);
        assert_eq!(spread_in_pips(0.0022, f64::NAN), f64::INFINITY);
        assert!(!spread_recovered(spread_in_pips(0.0001, 0.0)));
    }

    #[test]
    fn spread_recovered_below_and_at_cutoff() {
        assert!(spread_recovered(2.0));
        assert!(spread_recovered(SPREAD_BLACKOUT_RECOVERED_PIPS));
        assert!(!spread_recovered(20.0));
        assert!(!spread_recovered(6.0), "hysteresis band is not recovered");
    }

    // --- merge_cancelled_order (relocated from blackout_cancel) ---

    /// A holder set with just `reason` — the common test shape.
    fn held_by(reason: HoldReason) -> Holders {
        let mut h = Holders::new();
        h.hold(reason);
        h
    }

    fn cancelled(order_id: &str) -> CancelledOrder {
        CancelledOrder {
            order_id: order_id.into(),
            signed_intent: format!("id: {order_id}\nsig: v1-sig.xxx\n"),
        }
    }

    #[test]
    fn merge_onto_fresh_record_sets_applied_and_pushes() {
        let rec = merge_cancelled_order(
            None,
            "hs-aud-chf-abc",
            "AUD/CHF",
            Some("reversals"),
            0.0001,
            cancelled("ORD-1"),
            &held_by(HoldReason::SpreadHour),
            ts("2026-07-08T21:05:00Z"),
        );
        assert!(rec.applied, "cancel is a real broker mutation");
        assert_eq!(rec.trade_id, "hs-aud-chf-abc");
        assert_eq!(rec.instrument, "AUD/CHF");
        assert_eq!(rec.account.as_deref(), Some("reversals"));
        assert_eq!(rec.cancelled_orders.len(), 1);
        assert_eq!(rec.cancelled_orders[0].order_id, "ORD-1");
    }

    #[test]
    fn merge_dedups_same_order_id_on_refire() {
        let existing = merge_cancelled_order(
            None,
            "t1",
            "AUD/CHF",
            None,
            0.0001,
            cancelled("ORD-1"),
            &held_by(HoldReason::SpreadHour),
            ts("2026-07-08T21:05:00Z"),
        );
        let rec = merge_cancelled_order(
            Some(existing),
            "t1",
            "AUD/CHF",
            None,
            0.0001,
            cancelled("ORD-1"),
            &held_by(HoldReason::SpreadHour),
            ts("2026-07-08T21:06:00Z"),
        );
        assert_eq!(rec.cancelled_orders.len(), 1, "no exact-duplicate growth");
    }

    // --- mock broker (scriptable pending orders, cancel log, quote) ---

    #[derive(Default)]
    struct MockBroker {
        pendings: RefCell<Vec<PendingOrder>>,
        cancel_calls: RefCell<Vec<(String, String)>>,
        quote: RefCell<Option<Quote>>,
        /// When true, `cancel_order` returns `Transient` — the failure the
        /// vanished-order path exists to classify.
        cancel_fails: RefCell<bool>,
        /// What `lookup_attempt_state` answers. `None` ⇒ a lookup error.
        lookup: RefCell<Option<AttemptState>>,
    }

    impl MockBroker {
        fn with_pending(order: PendingOrder) -> Self {
            let b = Self::default();
            b.pendings.borrow_mut().push(order);
            b
        }
        fn set_quote(&self, bid: f64, ask: f64) {
            *self.quote.borrow_mut() = Some(Quote { bid, ask });
        }
        /// Fail the cancel, and answer a follow-up lookup with `state`.
        fn failing_cancel(self, state: Option<AttemptState>) -> Self {
            *self.cancel_fails.borrow_mut() = true;
            *self.lookup.borrow_mut() = state;
            self
        }
    }

    impl Broker for MockBroker {
        async fn place_entry(
            &self,
            _max_risk_pct: f64,
            _max_open_positions: u32,
            _req: &EntryRequest<'_>,
        ) -> Result<String, EntryError> {
            Ok("order-redriven".into())
        }
        async fn close_positions(&self, _instrument: &str) -> bool {
            false
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
            self.lookup.borrow().clone().ok_or(LookupError::Transient)
        }
        async fn cancel_order(
            &self,
            account_id: &str,
            broker_order_id: &str,
        ) -> Result<(), CancelError> {
            self.cancel_calls
                .borrow_mut()
                .push((account_id.to_string(), broker_order_id.to_string()));
            if *self.cancel_fails.borrow() {
                return Err(CancelError::Transient);
            }
            Ok(())
        }
        async fn get_quote(&self, _instrument: &str) -> Result<Quote, LookupError> {
            self.quote.borrow().ok_or(LookupError::Transient)
        }
        async fn list_open_positions(
            &self,
            _account_id: &str,
        ) -> Result<Vec<OpenPosition>, LookupError> {
            Ok(Vec::new())
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
            Ok(self.pendings.borrow().clone())
        }
        async fn get_candles(
            &self,
            _instrument: &str,
            _granularity: Granularity,
            _since: DateTime<Utc>,
            _now: DateTime<Utc>,
        ) -> Result<Vec<Candle>, CandleError> {
            Ok(Vec::new())
        }
    }

    /// Offline dispatch-config provider — a fixed config, never reads a backend.
    /// A signing key for the live-style [`SignedBodySource`] used in these
    /// tests. None of the ON/OFF-gate tests drive a re-verify (no stored body /
    /// no cancelled_orders), so the key value is inert — it just satisfies the
    /// seam. The behaviour under test is byte-identical to the pre-seam code.
    const KEY: [u8; 32] = [9u8; 32];

    fn src() -> SignedBodySource<'static> {
        SignedBodySource { key: &KEY }
    }

    struct StubCfg;
    impl EnterConfigProvider for StubCfg {
        async fn dispatch_config(&self, _verified: &Verified) -> DispatchConfig {
            DispatchConfig {
                worker_max_risk_pct: 1.0,
                worker_max_open_positions: 3,
                pip_size: 0.0001,
                tick_size: None,
                caps: AccountCaps::default(),
            }
        }
    }

    fn pending(order_id: &str, instrument: &str) -> PendingOrder {
        PendingOrder {
            order_id: order_id.into(),
            instrument: instrument.into(),
            direction: Direction::Short,
            trigger: 0.5598,
            is_stop: true,
            stake: 1.0,
        }
    }

    // --- ON gate: cancel only on a spread hour, only with a stored body ---

    /// A resting order with NO stored body is NEVER cancelled (safety rail 2),
    /// even inside a spread hour.
    #[test]
    fn no_stored_body_leaves_order_resting_in_spread_hour() {
        let broker = MockBroker::with_pending(pending("ORD-nobody", "AUD/CHF"));
        let store = MemStateStore::new();
        // AUD/CHF 21:00Z is a baked spread hour (the origin bar).
        let now = ts("2026-07-08T21:00:00Z");
        store.set_clock(now);
        let report = run(pending_order_lifecycle(
            &broker,
            &store,
            &StubCfg,
            &src(),
            Some("reversals"),
            now,
            ClearPolicy::ClearRecord,
        ));
        assert!(
            broker.cancel_calls.borrow().is_empty(),
            "no stored body ⇒ must never cancel"
        );
        assert!(report.cancelled.is_empty());
        assert!(report.skipped.contains(&"ORD-nobody".to_string()));
    }

    /// A resting order on a CLEAN (non-spread-hour) bar is left resting — the
    /// ON trigger is the baked clock, so midday is a no-op.
    #[test]
    fn clean_bar_never_cancels() {
        let broker = MockBroker::with_pending(pending("ORD-clean", "AUD/CHF"));
        let store = MemStateStore::new();
        // Midday is not a spread hour for AUD/CHF.
        let now = ts("2026-07-08T12:00:00Z");
        store.set_clock(now);
        let report = run(pending_order_lifecycle(
            &broker,
            &store,
            &StubCfg,
            &src(),
            None,
            now,
            ClearPolicy::ClearRecord,
        ));
        assert!(broker.cancel_calls.borrow().is_empty());
        assert!(report.cancelled.is_empty());
        assert!(report.skipped.contains(&"ORD-clean".to_string()));
    }

    // --- OFF decision (release_satisfied): pure of run_enter ---

    /// `true` when the OFF pass released everything and the order should be
    /// re-placed. Wraps `release_satisfied`, whose bool is the restore trigger.
    async fn off(
        broker: &MockBroker,
        store: &MemStateStore,
        rec: &HeldTradeRecord,
        now: DateTime<Utc>,
    ) -> bool {
        release_satisfied(broker, store, rec, now).await.1
    }

    fn applied_record(instrument: &str, opened: &str) -> HeldTradeRecord {
        HeldTradeRecord {
            trade_id: "t-off".into(),
            instrument: instrument.into(),
            account: None,
            applied: true,
            // The existing OFF tests describe a spread-hour-created record, so
            // that is the reason holding it. `held_with` overrides for the
            // overlap cases.
            holders: held_by(HoldReason::SpreadHour),
            opened_at: ts(opened),
            expires_at: ts(opened) + Duration::seconds(SAFETY_FORCE_RESTORE_SECONDS as i64),
            pip_size: 0.0001,
            original_stops: Vec::new(),
            cancelled_orders: Vec::new(),
        }
    }

    /// OFF fires when the baked spread hour has ended (the deterministic
    /// off-signal shared by replay + live) — no quote needed.
    #[test]
    fn spread_hour_releases_when_baked_hour_ended() {
        let broker = MockBroker::default(); // no quote set
        let store = MemStateStore::new(); // no pause armed
        let rec = applied_record("AUD/CHF", "2026-07-08T21:05:00Z");
        // Midday — no longer a spread hour → OFF regardless of any quote.
        assert!(run(off(&broker, &store, &rec, ts("2026-07-08T12:00:00Z"))));
    }

    /// OFF fires EARLY (still inside the baked hour) when the LIVE spread has
    /// recovered — the live-only early-un-block. Replay (no quote) would wait
    /// for the baked-hour-end instead.
    #[test]
    fn spread_hour_releases_early_when_live_spread_recovered() {
        let broker = MockBroker::default();
        broker.set_quote(0.5600, 0.5602); // 2p spread ≤ 4p recovered cutoff
        let store = MemStateStore::new(); // no pause armed
        let rec = applied_record("AUD/CHF", "2026-07-08T21:00:00Z");
        // Still inside the 21:00Z baked hour, but the live spread has calmed.
        assert!(run(off(&broker, &store, &rec, ts("2026-07-08T21:20:00Z"))));
    }

    /// OFF does NOT fire inside the baked hour when the live spread is still
    /// blown (and no quote → also not recovered).
    #[test]
    fn spread_hour_still_holds_inside_hour_with_wide_spread() {
        let broker = MockBroker::default();
        broker.set_quote(0.5590, 0.5602); // 12p spread, still blown
        let store = MemStateStore::new(); // no pause armed
        let rec = applied_record("AUD/CHF", "2026-07-08T21:00:00Z");
        assert!(!run(off(&broker, &store, &rec, ts("2026-07-08T21:20:00Z"))));

        // No quote available → treated as "not yet recovered".
        let broker_noquote = MockBroker::default();
        assert!(!run(off(
            &broker_noquote,
            &store,
            &rec,
            ts("2026-07-08T21:20:00Z")
        )));
    }

    // --- recover_one rails ---

    /// RAIL 4 — a record the box never applied is left untouched (never
    /// cleared), even long past its backstop.
    #[test]
    fn unapplied_record_is_never_cleared() {
        let broker = MockBroker::default();
        let store = MemStateStore::new();
        let mut rec = applied_record("AUD/CHF", "2026-07-08T21:05:00Z");
        rec.applied = false;
        let now = ts("2026-07-09T02:00:00Z"); // well past backstop
        store.set_clock(now);
        let mut report = LifecycleReport::default();
        run(recover_one(
            &broker,
            &store,
            &StubCfg,
            &src(),
            &rec,
            now,
            ClearPolicy::ClearRecord,
            &mut report,
        ));
        assert!(report.restored.is_empty(), "unapplied ⇒ never cleared");
    }

    /// SAFETY force-restore — the last-resort ceiling clears a stuck applied
    /// record. To exercise it in isolation the normal `off_now` restore must be
    /// UNABLE to fire: `now` is chosen to be a spread hour for AUD/CHF (so
    /// `off_now`'s block-lift branch is false) with no quote (so its recovery
    /// branch is false too) AND ≥ 12h after `opened_at` (so the safety timer is
    /// due). This is the pathological "off_now never cleared it" case the safety
    /// net exists for; in a normal block `off_now` restores at the lift first and
    /// this never fires. No cancelled orders here → no run_enter drive.
    #[test]
    fn backstop_clears_applied_record() {
        let broker = MockBroker::default(); // no quote → never "recovered"
        let store = MemStateStore::new();
        let rec = applied_record("AUD/CHF", "2026-07-08T21:05:00Z");
        // Next day's spread hour: still in-block per the mask (off_now false) and
        // > 12h after opened_at (safety timer due).
        let now = ts("2026-07-09T21:30:00Z");
        store.set_clock(now);
        run(async {
            store
                .upsert_held_trade_record(&rec, SAFETY_FORCE_RESTORE_SECONDS)
                .await
                .expect("upsert record");
            let mut report = LifecycleReport::default();
            recover_one(
                &broker,
                &store,
                &StubCfg,
                &src(),
                &rec,
                now,
                ClearPolicy::ClearRecord,
                &mut report,
            )
            .await;
            assert_eq!(
                report.restored,
                vec![("t-off".to_string(), RestoreReason::Backstop)],
                "backstop must clear the stuck record"
            );
        });
    }

    /// Option A — `ClearPolicy::LeaveForCaller`: the OFF-side System-3 restore
    /// runs and the report records it, but the shared fn does NOT delete the
    /// record — the live watcher (which also restores System 2) owns the single
    /// clear. Uses a block-ENDED bar so `off_now` fires without any quote.
    #[test]
    fn leave_for_caller_restores_but_does_not_clear_the_record() {
        use crate::state::RememberedStop;
        let broker = MockBroker::default(); // no quote; block-end drives off_now
        let store = MemStateStore::new();
        // A System-2-ONLY record: EMPTY cancelled_orders (nothing for System 3 to
        // restore) but a WIDENED STOP in original_stops (System 2's data). This is
        // the exact regression case: the shared fn must NOT delete this record —
        // the live watcher still needs it to restore the widened stop, then clears.
        let mut rec = applied_record("AUD/CHF", "2026-07-08T21:05:00Z");
        rec.original_stops = vec![RememberedStop {
            position_or_order_id: "POS-9".into(),
            original_stop: 0.5620,
        }];
        let now = ts("2026-07-08T12:00:00Z"); // midday — block ended → off_now true
        store.set_clock(now);
        run(async {
            store
                .upsert_held_trade_record(&rec, SAFETY_FORCE_RESTORE_SECONDS)
                .await
                .expect("upsert record");
            let mut report = LifecycleReport::default();
            recover_one(
                &broker,
                &store,
                &StubCfg,
                &src(),
                &rec,
                now,
                ClearPolicy::LeaveForCaller,
                &mut report,
            )
            .await;
            // The restore is reported (System 3 restore did occur — here a no-op
            // over empty cancelled_orders — and the caller is signalled).
            assert_eq!(
                report.restored,
                vec![("t-off".to_string(), RestoreReason::Recovered)],
                "LeaveForCaller still reports the restore"
            );
            // The record is LEFT for the caller — NOT deleted here — AND it still
            // carries its widened stop, so the watcher can restore System 2.
            let still_there = store
                .get_held_trade_record("t-off")
                .await
                .expect("record read")
                .expect(
                    "LeaveForCaller must NOT delete the System-2-only record — the watcher \
                     restores its widened stop then clears",
                );
            assert_eq!(
                still_there.original_stops.len(),
                1,
                "the widened-stop data survives for the watcher's System-2 restore"
            );
            assert_eq!(still_there.original_stops[0].position_or_order_id, "POS-9");
        });
    }

    /// The twin: `ClearPolicy::ClearRecord` (replay/default) DOES delete the
    /// record on the same OFF trigger — so the policy actually gates the delete.
    #[test]
    fn clear_record_deletes_the_record_on_off() {
        let broker = MockBroker::default();
        let store = MemStateStore::new();
        let rec = applied_record("AUD/CHF", "2026-07-08T21:05:00Z");
        let now = ts("2026-07-08T12:00:00Z"); // block ended → off_now true
        store.set_clock(now);
        run(async {
            store
                .upsert_held_trade_record(&rec, SAFETY_FORCE_RESTORE_SECONDS)
                .await
                .expect("upsert record");
            let mut report = LifecycleReport::default();
            recover_one(
                &broker,
                &store,
                &StubCfg,
                &src(),
                &rec,
                now,
                ClearPolicy::ClearRecord,
                &mut report,
            )
            .await;
            assert_eq!(
                report.restored,
                vec![("t-off".to_string(), RestoreReason::Recovered)]
            );
            let gone = store
                .get_held_trade_record("t-off")
                .await
                .expect("record read");
            assert!(gone.is_none(), "ClearRecord deletes the record");
        });
    }

    // --- replay-style VerifiedSource: cancel WITHOUT any signed body (PR 4a) ---

    /// A replay-style [`VerifiedSource`]: hands back an armed `Verified` keyed by
    /// `order_id`, ignoring the (absent) signed body. This is the offline seam —
    /// the fake broker armed the intent+shell at placement, so the lifecycle
    /// re-drives with NO HMAC round-trip. Mirrors what `ReplayBroker` will hold.
    struct ArmedSource {
        armed: std::collections::HashMap<String, Verified>,
    }
    impl VerifiedSource for ArmedSource {
        async fn recover(
            &self,
            order_id: &str,
            _signed_body: Option<&str>,
            _now: DateTime<Utc>,
        ) -> Recovered {
            match self.armed.get(order_id) {
                Some(v) => Recovered::Ok(Box::new(v.clone())),
                None => Recovered::Unrecoverable,
            }
        }
    }

    /// A minimal valid enter `Verified` (serde-built intent + a plain shell),
    /// carrying a trade_id + pip_size so the cancel side can key the record.
    fn armed_verified(order_instrument: &str) -> Verified {
        use crate::broker::Candle;
        use crate::intent::{Intent, Shell};
        let intent: Intent = serde_json::from_str(&format!(
            r#"{{
                "v": 1,
                "id": "t-enter",
                "not_after": "2026-07-09T00:00:00Z",
                "action": "enter",
                "instrument": "{order_instrument}",
                "direction": "short",
                "entry": {{ "type": "stop", "from": "close", "offset_pips": 0.0, "at": 0.5598 }},
                "stop_loss": {{ "absolute": 0.5607 }},
                "take_profit": {{ "absolute": 0.5560 }},
                "broker": "tradenation",
                "trade_id": "t",
                "pip_size": 0.0001
            }}"#
        ))
        .expect("valid enter intent");
        let shell = Shell::from_candle(&Candle {
            time: ts("2026-07-08T20:00:00Z"),
            o: 0.5600,
            h: 0.5605,
            l: 0.5595,
            c: 0.5600,
        });
        Verified { shell, intent }
    }

    /// The offline seam works: a resting order with an ARMED verified (no stored
    /// signed body) IS cancelled + backed up in a spread hour — the capability
    /// the old signed-body-only path lacked. This is what lets replay reproduce
    /// the live cancel without threading a signing key through the loop.
    #[test]
    fn armed_source_cancels_without_a_signed_body() {
        let broker = MockBroker::with_pending(pending("t-enter-o1", "AUD/CHF"));
        let store = MemStateStore::new();
        let mut armed = std::collections::HashMap::new();
        armed.insert("t-enter-o1".to_string(), armed_verified("AUD/CHF"));
        let source = ArmedSource { armed };

        let now = ts("2026-07-08T21:00:00Z"); // AUD/CHF baked spread hour
        store.set_clock(now);
        let report = run(pending_order_lifecycle(
            &broker,
            &store,
            &StubCfg,
            &source,
            Some("reversals"),
            now,
            ClearPolicy::ClearRecord,
        ));
        assert_eq!(
            report.cancelled,
            vec!["t-enter-o1".to_string()],
            "armed order in a spread hour must be cancelled with no signed body"
        );
        assert_eq!(
            broker.cancel_calls.borrow().len(),
            1,
            "the broker cancel must have been issued"
        );
        // And the crash-safe record was written (store-before-cancel, RAIL 1).
        run(async {
            let rec = store.get_held_trade_record("t").await.expect("record read");
            let rec = rec.expect("a record was upserted before the cancel");
            assert!(rec.applied);
            assert_eq!(rec.cancelled_orders.len(), 1);
            assert_eq!(rec.cancelled_orders[0].order_id, "t-enter-o1");
        });
    }

    /// PR-2 TRIGGER DELTA (characterisation): the ON-side cancel now fires on the
    /// pure baked clock (`is_spread_hour`) and DOES NOT read the live quote. The
    /// old live-cron cancel sampled `get_quote` and cancelled only when
    /// `spread_pips > elevated_threshold` (~5× the instrument's median, e.g.
    /// ~4.5p for AUD/CHF). This test pins the NEW behaviour: inside a baked spread
    /// hour the order is cancelled EVEN WITH A NARROW live spread (2p, well under
    /// the old ~4.5p threshold) — proving the quote no longer gates the cancel.
    /// Replaces the deleted `current_cancel_trigger_uses_5x_median_threshold_for_aud_chf`.
    #[test]
    fn cancel_trigger_is_baked_clock_not_live_quote() {
        let broker = MockBroker::with_pending(pending("t-enter-o1", "AUD/CHF"));
        // A NARROW live spread (2p) — the old 5×-median live-quote gate (~4.5p)
        // would have left this order resting. The baked clock ignores it.
        broker.set_quote(0.5600, 0.5602);
        let store = MemStateStore::new();
        let mut armed = std::collections::HashMap::new();
        armed.insert("t-enter-o1".to_string(), armed_verified("AUD/CHF"));
        let source = ArmedSource { armed };

        let now = ts("2026-07-08T21:00:00Z"); // AUD/CHF baked spread hour
        store.set_clock(now);
        let report = run(pending_order_lifecycle(
            &broker,
            &store,
            &StubCfg,
            &source,
            Some("reversals"),
            now,
            ClearPolicy::ClearRecord,
        ));
        assert_eq!(
            report.cancelled,
            vec!["t-enter-o1".to_string()],
            "baked-clock ON trigger cancels in a spread hour regardless of a narrow live spread"
        );
    }

    /// The predicate-false twin: the SAME armed order on a clean bar is left
    /// resting (ON = baked clock), proving the seam didn't change the gate.
    #[test]
    fn armed_source_leaves_order_resting_on_a_clean_bar() {
        let broker = MockBroker::with_pending(pending("t-enter-o1", "AUD/CHF"));
        let store = MemStateStore::new();
        let mut armed = std::collections::HashMap::new();
        armed.insert("t-enter-o1".to_string(), armed_verified("AUD/CHF"));
        let source = ArmedSource { armed };

        let now = ts("2026-07-08T12:00:00Z"); // clean
        store.set_clock(now);
        let report = run(pending_order_lifecycle(
            &broker,
            &store,
            &StubCfg,
            &source,
            Some("reversals"),
            now,
            ClearPolicy::ClearRecord,
        ));
        assert!(report.cancelled.is_empty());
        assert!(broker.cancel_calls.borrow().is_empty());
    }

    // --- news pause: a paused trade's resting order is pulled too ---

    /// Arm a pause on trade `t` (the `trade_id` `armed_verified` bakes) so the
    /// news-standoff branch of the ON trigger is live for that order.
    async fn pause_trade(store: &MemStateStore, now: DateTime<Utc>) {
        store
            .set_pause("t", "cal-cpi-pause", Some("news:AUD/CHF"), now, 3600)
            .await
            .expect("set pause");
    }

    /// THE BUG (2026-07-30). A resting entry order on a trade whose news pause
    /// is active must be CANCELLED, exactly as a spread hour cancels it —
    /// otherwise it sits through the event and fills on the news spike while
    /// `run_enter` is (correctly) rejecting new entries with 423. The bar here is
    /// CLEAN (`!is_spread_hour`), so only the pause can drive the cancel.
    #[test]
    fn active_pause_cancels_the_resting_order_on_a_clean_bar() {
        let broker = MockBroker::with_pending(pending("t-enter-o1", "AUD/CHF"));
        let store = MemStateStore::new();
        let mut armed = std::collections::HashMap::new();
        armed.insert("t-enter-o1".to_string(), armed_verified("AUD/CHF"));
        let source = ArmedSource { armed };

        let now = ts("2026-07-08T12:00:00Z"); // clean bar — NOT a spread hour
        store.set_clock(now);
        let report = run(async {
            pause_trade(&store, now).await;
            pending_order_lifecycle(
                &broker,
                &store,
                &StubCfg,
                &source,
                Some("reversals"),
                now,
                ClearPolicy::ClearRecord,
            )
            .await
        });
        assert_eq!(
            report.cancelled,
            vec!["t-enter-o1".to_string()],
            "an active news pause must pull the resting order, like a spread hour does"
        );
        assert_eq!(
            broker.cancel_calls.borrow().len(),
            1,
            "the broker cancel must have been issued"
        );
        // RAIL 1 — the crash-safe record was written before the cancel, so the
        // OFF side can re-place the order when the pause lifts.
        run(async {
            let rec = store
                .get_held_trade_record("t")
                .await
                .expect("record read")
                .expect("a record was upserted before the cancel");
            assert!(rec.applied);
            assert_eq!(rec.cancelled_orders.len(), 1);
            assert_eq!(rec.cancelled_orders[0].order_id, "t-enter-o1");
        });
    }

    /// Scoping: the pause is keyed by `trade_id`, so a pause on a DIFFERENT
    /// trade must not touch this order. Guards against the instrument-wide
    /// widening that `core::state`'s `PauseEntry` docs explicitly reject.
    #[test]
    fn pause_on_another_trade_leaves_this_order_resting() {
        let broker = MockBroker::with_pending(pending("t-enter-o1", "AUD/CHF"));
        let store = MemStateStore::new();
        let mut armed = std::collections::HashMap::new();
        armed.insert("t-enter-o1".to_string(), armed_verified("AUD/CHF"));
        let source = ArmedSource { armed };

        let now = ts("2026-07-08T12:00:00Z"); // clean bar
        store.set_clock(now);
        let report = run(async {
            // A pause on some OTHER setup on the same pair.
            store
                .set_pause("other-trade", "cal-cpi-pause", None, now, 3600)
                .await
                .expect("set pause");
            pending_order_lifecycle(
                &broker,
                &store,
                &StubCfg,
                &source,
                Some("reversals"),
                now,
                ClearPolicy::ClearRecord,
            )
            .await
        });
        assert!(
            report.cancelled.is_empty(),
            "a pause is trade-scoped — another trade's pause must not pull this order"
        );
        assert!(broker.cancel_calls.borrow().is_empty());
    }

    /// OFF side: while the pause is STILL ACTIVE the record must not restore,
    /// even though the bar is clean (`!is_spread_hour`, which alone would have
    /// returned true). Without this the order would be re-placed on the very
    /// next cron tick — straight back into the news window it was pulled from.
    #[test]
    fn off_now_false_while_pause_still_active() {
        let broker = MockBroker::default();
        let store = MemStateStore::new();
        let mut rec = applied_record("AUD/CHF", "2026-07-08T11:55:00Z");
        rec.holders = held_by(HoldReason::NewsPause); // pause-created record
        let now = ts("2026-07-08T12:00:00Z"); // clean bar — spread side says OFF
        store.set_clock(now);
        let held = run(async {
            store
                .set_pause(&rec.trade_id, "cal-cpi-pause", None, now, 3600)
                .await
                .expect("set pause");
            off(&broker, &store, &rec, now).await
        });
        assert!(
            !held,
            "an active pause must hold the order back even on a clean bar"
        );
    }

    /// RAIL 5 still holds over a pause: the 12h backstop force-restores even
    /// while a pause is armed. `pause_active` fails CLOSED (a store error reads
    /// as paused), so without an escape hatch a stuck pause row — or a store
    /// that never answers — would pin the order forever. The backstop is checked
    /// independently of `off_now`, so it remains that hatch.
    #[test]
    fn backstop_restores_even_while_paused() {
        let broker = MockBroker::default(); // no quote → never "recovered"
        let store = MemStateStore::new();
        let mut rec = applied_record("AUD/CHF", "2026-07-08T21:05:00Z");
        rec.holders = held_by(HoldReason::NewsPause);
        // > 12h after opened_at, and a pause is STILL armed.
        let now = ts("2026-07-09T21:30:00Z");
        store.set_clock(now);
        run(async {
            store
                .upsert_held_trade_record(&rec, SAFETY_FORCE_RESTORE_SECONDS)
                .await
                .expect("upsert record");
            store
                .set_pause(&rec.trade_id, "stuck-pause", None, now, 86_400)
                .await
                .expect("set pause");
            // Precondition: the normal OFF path is genuinely blocked by the pause.
            assert!(
                !off(&broker, &store, &rec, now).await,
                "the pause must be holding OFF back, or this test proves nothing"
            );
            let mut report = LifecycleReport::default();
            recover_one(
                &broker,
                &store,
                &StubCfg,
                &src(),
                &rec,
                now,
                ClearPolicy::ClearRecord,
                &mut report,
            )
            .await;
            assert_eq!(
                report.restored,
                vec![("t-off".to_string(), RestoreReason::Backstop)],
                "the 12h backstop must still free a record wedged behind a pause"
            );
        });
    }

    /// The twin: once the pause is CLEARED the same record restores on the same
    /// clean bar — so the pause genuinely gates OFF rather than wedging it.
    #[test]
    fn off_now_true_once_pause_cleared() {
        let broker = MockBroker::default();
        let store = MemStateStore::new();
        let rec = applied_record("AUD/CHF", "2026-07-08T11:55:00Z");
        let now = ts("2026-07-08T12:00:00Z");
        store.set_clock(now);
        let off_fired = run(async {
            store
                .set_pause(&rec.trade_id, "cal-cpi-pause", None, now, 3600)
                .await
                .expect("set pause");
            store
                .clear_pause(&rec.trade_id, "cal-cpi-pause")
                .await
                .expect("clear pause");
            off(&broker, &store, &rec, now).await
        });
        assert!(off_fired, "pause cleared + clean bar ⇒ restore");
    }

    /// End-to-end through the shared lifecycle: pause active ⇒ cancelled and NOT
    /// restored in the same pass. Proves `cancel_pass` and `recover_pass` agree
    /// on the pause (a mismatch would cancel then immediately re-place).
    #[test]
    fn paused_order_is_not_restored_in_the_same_pass() {
        let broker = MockBroker::with_pending(pending("t-enter-o1", "AUD/CHF"));
        let store = MemStateStore::new();
        let mut armed = std::collections::HashMap::new();
        armed.insert("t-enter-o1".to_string(), armed_verified("AUD/CHF"));
        let source = ArmedSource { armed };

        let now = ts("2026-07-08T12:00:00Z"); // clean bar
        store.set_clock(now);
        let report = run(async {
            pause_trade(&store, now).await;
            pending_order_lifecycle(
                &broker,
                &store,
                &StubCfg,
                &source,
                Some("reversals"),
                now,
                ClearPolicy::ClearRecord,
            )
            .await
        });
        assert_eq!(report.cancelled, vec!["t-enter-o1".to_string()]);
        assert!(
            report.restored.is_empty(),
            "must not restore while the pause that caused the cancel is still active"
        );
        // The record survives for the post-pause restore.
        run(async {
            assert!(
                store
                    .get_held_trade_record("t")
                    .await
                    .expect("record read")
                    .is_some(),
                "the record must outlive the pause so the order can be re-placed"
            );
        });
    }

    // --- OVERLAP: a spread hour and a news pause at the same time ---
    //
    // The operator's case (2026-07-30): "spread hour is 06:30–08:00, but what if
    // the news pause happens at 07:00?" Both reasons hold; they lift
    // independently; only the LAST one out may restore the order. AUD/CHF 21:00Z
    // is the baked spread hour these tests use (12:00Z is clean), so the
    // "spread hour" leg is real baked data rather than an invented window.

    /// A record held by BOTH reasons, as the ON side would have written it when
    /// the pause arrived partway through the spread hour.
    fn held_by_both(instrument: &str, opened: &str) -> HeldTradeRecord {
        let mut rec = applied_record(instrument, opened);
        rec.holders = Holders::new();
        rec.holders.hold(HoldReason::SpreadHour);
        rec.holders.hold(HoldReason::NewsPause);
        rec
    }

    /// OVERLAP 1 — **the spread hour lifts first, the pause is still armed.**
    /// This is the operator's exact scenario: the 06:30–08:00 trough ends at
    /// 08:00 but the 07:00 news standoff runs on. `SpreadHour` releases,
    /// `NewsPause` survives, and the order must NOT be re-placed.
    #[test]
    fn overlap_spread_lifts_first_pause_still_holds() {
        let broker = MockBroker::default();
        let store = MemStateStore::new();
        let rec = held_by_both("AUD/CHF", "2026-07-08T21:05:00Z");
        // Midday: the baked spread hour has ended (so SpreadHour releases)...
        let now = ts("2026-07-08T12:00:00Z");
        store.set_clock(now);
        let (surviving, emptied) = run(async {
            // ...but the news pause is still armed.
            store
                .set_pause(&rec.trade_id, "cal-cpi-pause", None, now, 3600)
                .await
                .expect("set pause");
            release_satisfied(&broker, &store, &rec, now).await
        });
        assert!(
            !emptied,
            "the pause still holds — the order must NOT be re-placed at the spread lift"
        );
        assert_eq!(surviving.len(), 1, "count went 2 → 1, not 2 → 0");
        assert!(surviving.contains(HoldReason::NewsPause));
        assert!(
            !surviving.contains(HoldReason::SpreadHour),
            "the released reason is dropped from the set"
        );
    }

    /// OVERLAP 2 — **the pause clears first, the spread hour is still live.**
    /// The mirror image. Note the live quote must be BLOWN, because
    /// `SpreadHour`'s release condition includes early live-spread recovery — a
    /// narrow quote would legitimately release it and empty the set.
    #[test]
    fn overlap_pause_clears_first_spread_hour_still_holds() {
        let broker = MockBroker::default();
        broker.set_quote(0.5590, 0.5602); // 12p — still blown, no early release
        let store = MemStateStore::new();
        let rec = held_by_both("AUD/CHF", "2026-07-08T21:00:00Z");
        // Inside the baked 21:00Z spread hour, and no pause is armed.
        let now = ts("2026-07-08T21:20:00Z");
        store.set_clock(now);
        let (surviving, emptied) = run(release_satisfied(&broker, &store, &rec, now));
        assert!(
            !emptied,
            "the spread hour still holds — the order must NOT be re-placed when the pause clears"
        );
        assert_eq!(surviving.len(), 1);
        assert!(surviving.contains(HoldReason::SpreadHour));
        assert!(!surviving.contains(HoldReason::NewsPause));
    }

    /// OVERLAP 3 — **both lift** ⇒ the set empties and the order is restored.
    /// The other side of 1 and 2: proves they're held by a live condition rather
    /// than something that never releases.
    #[test]
    fn overlap_both_lift_then_restores() {
        let broker = MockBroker::default();
        let store = MemStateStore::new(); // no pause armed
        let rec = held_by_both("AUD/CHF", "2026-07-08T21:05:00Z");
        let now = ts("2026-07-08T12:00:00Z"); // clean bar — spread lifted too
        store.set_clock(now);
        let (surviving, emptied) = run(release_satisfied(&broker, &store, &rec, now));
        assert!(emptied, "both reasons released ⇒ restore");
        assert!(surviving.is_empty());
    }

    /// OVERLAP 4 — the pause arrives PARTWAY THROUGH a spread hour and the ON
    /// side unions it onto the existing record, taking the count 1 → 2. Without
    /// the union the second reason would overwrite the first and the 08:00 spread
    /// lift would restore straight into the standoff.
    #[test]
    fn overlap_second_reason_unions_onto_an_existing_record() {
        let existing = applied_record("AUD/CHF", "2026-07-08T21:00:00Z");
        assert_eq!(existing.holders.len(), 1, "starts held by the spread hour");

        // The pause arrives; the ON side merges with only NewsPause in hand.
        let merged = merge_cancelled_order(
            Some(existing),
            "t-off",
            "AUD/CHF",
            None,
            0.0001,
            cancelled("ORD-2"),
            &held_by(HoldReason::NewsPause),
            ts("2026-07-08T21:30:00Z"),
        );
        assert_eq!(merged.holders.len(), 2, "union, not replace");
        assert!(merged.holders.contains(HoldReason::SpreadHour));
        assert!(merged.holders.contains(HoldReason::NewsPause));
    }

    /// OVERLAP 5 — the ~5s cron re-deriving the SAME reason many times must not
    /// inflate the set. This is the property a bare integer refcount would fail
    /// (it would climb to 200/hour and never reach zero), checked here through the
    /// real merge path rather than only on `Holders` in isolation.
    #[test]
    fn overlap_repeated_ticks_do_not_inflate_the_holder_set() {
        let mut rec = applied_record("AUD/CHF", "2026-07-08T21:00:00Z");
        for tick in 0..50 {
            rec = merge_cancelled_order(
                Some(rec),
                "t-off",
                "AUD/CHF",
                None,
                0.0001,
                cancelled("ORD-1"),               // same order id every tick
                &held_by(HoldReason::SpreadHour), // same reason every tick
                ts("2026-07-08T21:00:00Z") + Duration::seconds(5 * tick),
            );
        }
        assert_eq!(rec.holders.len(), 1, "50 ticks must not accumulate holders");
        assert_eq!(rec.cancelled_orders.len(), 1, "nor duplicate the order");
    }

    /// BACK-COMPAT — a pre-refcount (v119) row in flight during the deploy has
    /// `applied: true` and NO holders. Treated naively ("empty ⇒ restore") the
    /// first tick after deploy would re-place every such order into windows that
    /// are still live. `effective_holders` reads it as `{SpreadHour}` so it
    /// re-derives instead: here, inside the baked hour with a blown quote, it
    /// must still be held.
    #[test]
    fn pre_refcount_row_does_not_restore_blind() {
        let broker = MockBroker::default();
        broker.set_quote(0.5590, 0.5602); // still blown
        let store = MemStateStore::new();
        let mut rec = applied_record("AUD/CHF", "2026-07-08T21:00:00Z");
        rec.holders = Holders::new(); // a v119 row: applied, no holders
        let now = ts("2026-07-08T21:20:00Z"); // inside the baked spread hour
        store.set_clock(now);
        let (surviving, emptied) = run(release_satisfied(&broker, &store, &rec, now));
        assert!(
            !emptied,
            "a pre-refcount row must NOT restore blind while its block is still live"
        );
        assert!(surviving.contains(HoldReason::SpreadHour));
    }

    /// An `!applied` record with no holders must NOT report `emptied` — there is
    /// nothing to restore, and reporting the transition would let a record the box
    /// never mutated drive a re-place. `recover_one` gates on `!applied` first
    /// (rail 4), so this is defence in depth for direct callers; it is load-bearing
    /// because `emptied` is what triggers the broker re-drive.
    #[test]
    fn unapplied_holderless_record_never_reports_emptied() {
        let broker = MockBroker::default();
        let store = MemStateStore::new();
        let mut rec = applied_record("AUD/CHF", "2026-07-08T21:05:00Z");
        rec.applied = false;
        rec.holders = Holders::new();
        let now = ts("2026-07-08T12:00:00Z"); // block lifted — the tempting case
        store.set_clock(now);
        let (surviving, emptied) = run(release_satisfied(&broker, &store, &rec, now));
        assert!(
            !emptied,
            "no holders were ever released, so there is no transition to restore on"
        );
        assert!(surviving.is_empty());
    }

    /// The twin: the same healed pre-refcount row DOES restore once its block has
    /// genuinely lifted — the healing defers the decision, it doesn't wedge it.
    #[test]
    fn pre_refcount_row_restores_once_its_block_lifts() {
        let broker = MockBroker::default();
        let store = MemStateStore::new();
        let mut rec = applied_record("AUD/CHF", "2026-07-08T21:05:00Z");
        rec.holders = Holders::new(); // a v119 row
        let now = ts("2026-07-08T12:00:00Z"); // clean bar — block lifted
        store.set_clock(now);
        let (_, emptied) = run(release_satisfied(&broker, &store, &rec, now));
        assert!(
            emptied,
            "healed row restores once the spread hour has lifted"
        );
    }

    // --- A cancel that FAILS: DB/broker divergence ---
    //
    // The old code logged the failure and moved on, pushing nothing to the report.
    // So an order the record claimed was pulled — but which had actually FILLED —
    // was invisible, and the OFF side would faithfully re-place an entry for a
    // trade already in a position. These pin the classification + the prune.

    /// Drive one ON pass over a spread-hour order whose cancel fails, with
    /// `lookup` as the follow-up answer.
    fn cancel_failure_pass(lookup: Option<AttemptState>) -> (LifecycleReport, MemStateStore) {
        let broker =
            MockBroker::with_pending(pending("t-enter-o1", "AUD/CHF")).failing_cancel(lookup);
        let store = MemStateStore::new();
        let mut armed = std::collections::HashMap::new();
        armed.insert("t-enter-o1".to_string(), armed_verified("AUD/CHF"));
        let source = ArmedSource { armed };
        let now = ts("2026-07-08T21:00:00Z"); // AUD/CHF baked spread hour
        store.set_clock(now);
        let report = run(pending_order_lifecycle(
            &broker,
            &store,
            &StubCfg,
            &source,
            Some("reversals"),
            now,
            ClearPolicy::LeaveForCaller, // don't let the OFF side clear it out from under us
        ));
        (report, store)
    }

    /// THE DIVERGENCE. The cancel fails because the order already FILLED. It must
    /// be reported and **dropped from the record**, so the OFF side never re-places
    /// an entry for a position that already exists.
    #[test]
    fn cancel_failure_on_a_filled_order_drops_it_from_the_record() {
        let (report, store) = cancel_failure_pass(Some(AttemptState::OpenPosition {
            broker_trade_id: "TRADE-7".into(),
        }));

        assert!(
            report.cancelled.is_empty(),
            "a failed cancel is not a cancel"
        );
        assert_eq!(
            report.cancel_failed,
            vec![(
                "t-enter-o1".to_string(),
                CancelOutcome::Vanished("filled-open-position")
            )],
            "the failure must be REPORTED, not silently swallowed"
        );
        // The record must no longer list the order — nothing to restore.
        run(async {
            let rec = store.get_held_trade_record("t").await.expect("record read");
            match rec {
                None => {} // pruned to nothing and cleared — also correct
                Some(r) => assert!(
                    r.cancelled_orders.is_empty(),
                    "a filled order must not stay on the record, or the OFF side re-places it"
                ),
            }
        });
    }

    /// A cancel that fails while the order is genuinely **still resting** (network
    /// blip) must KEEP the record entry, so the next tick retries the cancel and
    /// the order is still restorable.
    #[test]
    fn cancel_failure_while_still_resting_keeps_the_record_entry() {
        let (report, store) = cancel_failure_pass(Some(AttemptState::Pending));

        assert_eq!(
            report.cancel_failed,
            vec![("t-enter-o1".to_string(), CancelOutcome::StillResting)],
        );
        run(async {
            let rec = store
                .get_held_trade_record("t")
                .await
                .expect("record read")
                .expect("the record must survive — the order is still out there");
            assert_eq!(
                rec.cancelled_orders.len(),
                1,
                "a still-resting order stays on the record so the cancel can retry"
            );
        });
    }

    /// A failed cancel whose follow-up LOOKUP also fails is `Unresolved` and
    /// treated conservatively as still-resting — the entry stays. Reported
    /// distinctly so a lookup outage doesn't masquerade as live orders.
    #[test]
    fn cancel_failure_with_an_unresolvable_lookup_keeps_the_entry() {
        let (report, store) = cancel_failure_pass(None); // lookup errors

        assert_eq!(
            report.cancel_failed,
            vec![("t-enter-o1".to_string(), CancelOutcome::Unresolved)],
        );
        run(async {
            let rec = store
                .get_held_trade_record("t")
                .await
                .expect("record read")
                .expect("conservative: keep the record when we can't tell");
            assert_eq!(rec.cancelled_orders.len(), 1);
        });
    }

    /// Every "not resting any more" state drops the order, not just the filled one:
    /// cancelled-upstream (TN rejects / OANDA TIF expiry) and not-found both mean
    /// there is nothing to restore.
    #[test]
    fn every_vanished_state_drops_the_order() {
        for (state, want) in [
            (AttemptState::Cancelled, "cancelled-upstream"),
            (AttemptState::Unknown, "not-found"),
            (
                AttemptState::ClosedWin { realized_pl: 12.0 },
                "filled-and-closed",
            ),
            (
                AttemptState::ClosedLossOrBreakeven { realized_pl: -8.0 },
                "filled-and-closed",
            ),
        ] {
            let (report, _) = cancel_failure_pass(Some(state.clone()));
            assert_eq!(
                report.cancel_failed,
                vec![("t-enter-o1".to_string(), CancelOutcome::Vanished(want))],
                "state {state:?} must classify as vanished/{want}"
            );
        }
    }

    /// A successful cancel reports NOTHING in `cancel_failed` — the new field is
    /// only populated on the failure path, so it stays a useful signal.
    #[test]
    fn a_successful_cancel_reports_no_failure() {
        let broker = MockBroker::with_pending(pending("t-enter-o1", "AUD/CHF"));
        let store = MemStateStore::new();
        let mut armed = std::collections::HashMap::new();
        armed.insert("t-enter-o1".to_string(), armed_verified("AUD/CHF"));
        let source = ArmedSource { armed };
        let now = ts("2026-07-08T21:00:00Z");
        store.set_clock(now);
        let report = run(pending_order_lifecycle(
            &broker,
            &store,
            &StubCfg,
            &source,
            Some("reversals"),
            now,
            ClearPolicy::LeaveForCaller,
        ));
        assert_eq!(report.cancelled, vec!["t-enter-o1".to_string()]);
        assert!(report.cancel_failed.is_empty());
    }
}
