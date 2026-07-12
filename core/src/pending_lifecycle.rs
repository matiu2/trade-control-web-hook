//! **The one shared resting-order lifecycle** — cancel a resting entry order
//! through a spread-hour trough and re-drive it once the trough lifts, keyed off
//! the single baked predicate
//! [`spread_blackout::is_spread_hour`](crate::spread_blackout::is_spread_hour).
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
use crate::broker::{Broker, PendingOrder};
use crate::dispatch::run_enter;
use crate::dispatch_config::DispatchConfig;
use crate::incoming::{self, IncomingError, Verified};
use crate::intent::Resolved;
use crate::spread_blackout::{
    BLACKOUT_BACKSTOP_SECONDS, SPREAD_BLACKOUT_RECOVERED_PIPS, is_spread_hour,
};
use crate::state::{CancelledOrder, SpreadBlackoutRecord, StateStore};

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
    /// `order_id`s examined but left resting (no body, won't verify, not a
    /// spread hour) — for visibility, not action.
    pub skipped: Vec<String>,
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

/// Run one resting-order lifecycle pass for a single `(broker, account)` at
/// `now`. Cancels resting orders that entered a spread hour and re-drives
/// records whose trough has lifted. The caller (live cron / replay loop) owns
/// the per-account fan-out and passes the already-acquired broker + signing key
/// in — mirroring how [`run_enter`] is per-enter and the cron loops accounts
/// around it.
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
) -> LifecycleReport
where
    B: Broker,
    S: StateStore,
    P: EnterConfigProvider,
    V: VerifiedSource,
{
    let mut report = LifecycleReport::default();
    cancel_pass(broker, store, src, account, now, &mut report).await;
    recover_pass(broker, store, cfg_provider, src, now, &mut report).await;
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
        // ON trigger — the pure baked clock, incl. the 30-min lead + NY-close
        // fallback. NO live quote (the ON/OFF asymmetry).
        if !is_spread_hour(&order.instrument, now) {
            report.skipped.push(order.order_id.clone());
            continue;
        }
        try_cancel_one(broker, store, src, account, order, now, report).await;
    }
}

/// Cancel + store a single resting order. Store-before-cancel (safety rail 1);
/// no-recoverable-payload / won't-verify ⇒ leave resting (rails 2, 3).
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
    let existing = match store.get_spread_blackout_record(&trade_id).await {
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
        now,
    );
    if let Err(err) = store
        .upsert_spread_blackout_record(&record, BLACKOUT_BACKSTOP_SECONDS)
        .await
    {
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
                 (trigger={})",
                if order.is_stop { "stop" } else { "limit" },
                order.order_id,
                order.trigger,
            );
            report.cancelled.push(order.order_id.clone());
        }
        Err(err) => tracing::error!(
            "pending-lifecycle[{scope}][{trade_id}]: cancel order {} FAILED ({err:?}); record \
             stays (recovery re-drive is bounded by gates if still live)",
            order.order_id,
        ),
    }
}

fn account_id_of(account: Option<&str>) -> &str {
    account.unwrap_or("")
}

/// Pure record merge: push `cancelled` onto a fresh-or-existing record, set
/// `applied = true`, and preserve any widened-stop `original_stops`. Idempotent:
/// re-cancelling the same order id de-dups. Relocated verbatim from
/// `blackout_cancel::merge_cancelled_order`.
fn merge_cancelled_order(
    existing: Option<SpreadBlackoutRecord>,
    trade_id: &str,
    instrument: &str,
    account: Option<&str>,
    pip_size: f64,
    cancelled: CancelledOrder,
    now: DateTime<Utc>,
) -> SpreadBlackoutRecord {
    let mut record = existing.unwrap_or_else(|| SpreadBlackoutRecord {
        trade_id: trade_id.to_string(),
        instrument: instrument.to_string(),
        account: account.map(|s| s.to_string()),
        applied: false,
        opened_at: now,
        expires_at: now + Duration::seconds(BLACKOUT_BACKSTOP_SECONDS as i64),
        pip_size,
        original_stops: Vec::new(),
        cancelled_orders: Vec::new(),
    });
    record.applied = true;
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

/// Walk every per-trade record and, for each `applied` one whose trough has
/// lifted, re-drive its cancelled orders then clear it.
async fn recover_pass<B: Broker, S: StateStore, P: EnterConfigProvider, V: VerifiedSource>(
    broker: &B,
    store: &S,
    cfg_provider: &P,
    src: &V,
    now: DateTime<Utc>,
    report: &mut LifecycleReport,
) {
    let records = match store.list_all_spread_blackout_records().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("pending-lifecycle: list records failed: {err}");
            return;
        }
    };
    for record in records {
        recover_one(broker, store, cfg_provider, src, &record, now, report).await;
    }
}

/// Per-record OFF decision + clear. `!applied` ⇒ untouched (rail 4); backstop
/// clears unconditionally (rail 5); otherwise recovery (live spread) or the
/// baked spread hour ending clears it. Restore precedes clear (rail 6).
async fn recover_one<B: Broker, S: StateStore, P: EnterConfigProvider, V: VerifiedSource>(
    broker: &B,
    store: &S,
    cfg_provider: &P,
    src: &V,
    record: &SpreadBlackoutRecord,
    now: DateTime<Utc>,
    report: &mut LifecycleReport,
) {
    // RAIL 4 — never touch what you didn't apply.
    if !record.applied {
        return;
    }

    // RAIL 5 — backstop: clear regardless of spread.
    if backstop_due(record.opened_at, now) {
        restore_cancelled_orders(broker, store, cfg_provider, src, record, now).await;
        if clear(store, record).await {
            report
                .restored
                .push((record.trade_id.clone(), RestoreReason::Backstop));
        }
        return;
    }

    // OFF trigger — the ON/OFF asymmetry. `off_now` is true when EITHER the
    // live spread has recovered (live only) OR the baked spread hour has ended
    // (replay's off-signal, also a live off-signal).
    if off_now(broker, record, now).await {
        // RAIL 6 — restore BEFORE clear.
        restore_cancelled_orders(broker, store, cfg_provider, src, record, now).await;
        if clear(store, record).await {
            report
                .restored
                .push((record.trade_id.clone(), RestoreReason::Recovered));
        }
    }
}

/// The OFF-side decision (excluding the backstop, handled by the caller):
/// restore when the **live spread recovered** OR the **baked spread hour ended**.
/// Live samples the quote (recovery, may un-block early); replay's quote is
/// synthesised so it too can read recovery, but the baked-hour-end is the
/// deterministic off-signal both share.
async fn off_now<B: Broker>(broker: &B, record: &SpreadBlackoutRecord, now: DateTime<Utc>) -> bool {
    // Baked-hour-end — the deterministic off-signal (replay + live). If the
    // instrument is no longer in a spread hour at `now`, the trough has lifted.
    if !is_spread_hour(&record.instrument, now) {
        return true;
    }
    // Live-spread recovery — un-block early if the live spread has already
    // calmed even though the nominal baked hour hasn't ended. A quote error
    // (or a synthesised replay quote still inside the hour) simply means "not
    // yet recovered" and we wait for the baked-hour-end / backstop.
    match broker.get_quote(&record.instrument).await {
        Ok(quote) => spread_recovered(spread_in_pips(quote.spread(), record.pip_size)),
        Err(_) => false,
    }
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
    record: &SpreadBlackoutRecord,
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
    record: &SpreadBlackoutRecord,
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
    //    order re-stores its own order:{order_id} row.
    let cfg = cfg_provider.dispatch_config(&verified).await;
    let result = run_enter(
        broker,
        store,
        &verified,
        &cfg,
        now,
        Some(&cancelled.signed_intent),
        None,
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
async fn clear<S: StateStore>(store: &S, record: &SpreadBlackoutRecord) -> bool {
    match store.clear_spread_blackout_record(&record.trade_id).await {
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

/// Backstop: true once `now >= opened_at + BLACKOUT_BACKSTOP_SECONDS`.
pub fn backstop_due(opened_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now >= opened_at + Duration::seconds(BLACKOUT_BACKSTOP_SECONDS as i64)
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
    fn backstop_due_at_or_after_three_hours() {
        let opened = ts("2026-07-08T21:05:00Z");
        assert!(backstop_due(opened, ts("2026-07-09T00:05:00Z")));
        assert!(backstop_due(opened, ts("2026-07-09T00:05:01Z")));
    }

    #[test]
    fn backstop_not_due_before_three_hours() {
        let opened = ts("2026-07-08T21:05:00Z");
        assert!(!backstop_due(opened, ts("2026-07-09T00:04:59Z")));
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
            ts("2026-07-08T21:05:00Z"),
        );
        let rec = merge_cancelled_order(
            Some(existing),
            "t1",
            "AUD/CHF",
            None,
            0.0001,
            cancelled("ORD-1"),
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
            Err(LookupError::Transient)
        }
        async fn cancel_order(
            &self,
            account_id: &str,
            broker_order_id: &str,
        ) -> Result<(), CancelError> {
            self.cancel_calls
                .borrow_mut()
                .push((account_id.to_string(), broker_order_id.to_string()));
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
        ));
        assert!(broker.cancel_calls.borrow().is_empty());
        assert!(report.cancelled.is_empty());
        assert!(report.skipped.contains(&"ORD-clean".to_string()));
    }

    // --- OFF decision (off_now): pure of run_enter ---

    fn applied_record(instrument: &str, opened: &str) -> SpreadBlackoutRecord {
        SpreadBlackoutRecord {
            trade_id: "t-off".into(),
            instrument: instrument.into(),
            account: None,
            applied: true,
            opened_at: ts(opened),
            expires_at: ts(opened) + Duration::seconds(BLACKOUT_BACKSTOP_SECONDS as i64),
            pip_size: 0.0001,
            original_stops: Vec::new(),
            cancelled_orders: Vec::new(),
        }
    }

    /// OFF fires when the baked spread hour has ended (the deterministic
    /// off-signal shared by replay + live) — no quote needed.
    #[test]
    fn off_now_true_when_baked_hour_ended() {
        let broker = MockBroker::default(); // no quote set
        let rec = applied_record("AUD/CHF", "2026-07-08T21:05:00Z");
        // Midday — no longer a spread hour → OFF regardless of any quote.
        assert!(run(off_now(&broker, &rec, ts("2026-07-08T12:00:00Z"))));
    }

    /// OFF fires EARLY (still inside the baked hour) when the LIVE spread has
    /// recovered — the live-only early-un-block. Replay (no quote) would wait
    /// for the baked-hour-end instead.
    #[test]
    fn off_now_true_when_live_spread_recovered_inside_hour() {
        let broker = MockBroker::default();
        broker.set_quote(0.5600, 0.5602); // 2p spread ≤ 4p recovered cutoff
        let rec = applied_record("AUD/CHF", "2026-07-08T21:00:00Z");
        // Still inside the 21:00Z baked hour, but the live spread has calmed.
        assert!(run(off_now(&broker, &rec, ts("2026-07-08T21:20:00Z"))));
    }

    /// OFF does NOT fire inside the baked hour when the live spread is still
    /// blown (and no quote → also not recovered).
    #[test]
    fn off_now_false_inside_hour_with_wide_spread() {
        let broker = MockBroker::default();
        broker.set_quote(0.5590, 0.5602); // 12p spread, still blown
        let rec = applied_record("AUD/CHF", "2026-07-08T21:00:00Z");
        assert!(!run(off_now(&broker, &rec, ts("2026-07-08T21:20:00Z"))));

        // No quote available → treated as "not yet recovered".
        let broker_noquote = MockBroker::default();
        assert!(!run(off_now(
            &broker_noquote,
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
            &mut report,
        ));
        assert!(report.restored.is_empty(), "unapplied ⇒ never cleared");
    }

    /// RAIL 5 — the backstop clears a stuck applied record unconditionally,
    /// regardless of spread. No cancelled orders here → no run_enter drive.
    #[test]
    fn backstop_clears_applied_record() {
        let broker = MockBroker::default(); // no quote → never "recovered"
        let store = MemStateStore::new();
        let rec = applied_record("AUD/CHF", "2026-07-08T21:05:00Z");
        let now = ts("2026-07-09T00:10:00Z"); // > 3h after opened_at
        store.set_clock(now);
        run(async {
            store
                .upsert_spread_blackout_record(&rec, BLACKOUT_BACKSTOP_SECONDS)
                .await
                .expect("upsert record");
            let mut report = LifecycleReport::default();
            recover_one(&broker, &store, &StubCfg, &src(), &rec, now, &mut report).await;
            assert_eq!(
                report.restored,
                vec![("t-off".to_string(), RestoreReason::Backstop)],
                "backstop must clear the stuck record"
            );
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
            let rec = store
                .get_spread_blackout_record("t")
                .await
                .expect("record read");
            let rec = rec.expect("a record was upserted before the cancel");
            assert!(rec.applied);
            assert_eq!(rec.cancelled_orders.len(), 1);
            assert_eq!(rec.cancelled_orders[0].order_id, "t-enter-o1");
        });
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
        ));
        assert!(report.cancelled.is_empty());
        assert!(broker.cancel_calls.borrow().is_empty());
    }
}
