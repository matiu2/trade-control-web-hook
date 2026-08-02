//! The every-candle re-check (rule 7), shared by the live cron and the replay.
//!
//! [`promote_due_orders`] is the *decision* half of the order-control tick: walk
//! every parked order, re-ask [`sl_target`] whether it now clears its R-floor at
//! the current spread, and promote the ones that do.
//!
//! # Why this is in `core` and not in the cron driver
//!
//! It was written in `trade-control-cron` first, which put it out of reach of
//! the offline replay — the CLI doesn't depend on that crate, and the cron
//! driver is written against a `CronEnv` seam the replay has no impl of. That
//! would have meant a parked order promoting live and never in replay: a
//! silent divergence in exactly the direction
//! `[[strategy_changes_in_both_replayer_and_worker]]` exists to forbid, and one
//! that only shows up as a fixture that quietly disagrees with production.
//!
//! So the decision lives here, generic over `Broker` + `StateStore` like
//! everything else in this module, and each side supplies its own plumbing:
//! the cron acquires a broker per account, the replay hands over its
//! `ReplayBroker`. Same rule as [`crate::pending_lifecycle`], for the same
//! reason.
//!
//! # What stays outside
//!
//! The **re-price** half is deliberately not here. It needs a live
//! `list_pending_orders` join against `EntryAttempt` rows and a real
//! cancel-and-replace at a broker — machinery the replay models differently
//! (its fills come from a held ledger, not resting broker orders). Promotion is
//! the half that changes what a fixture *books*, so it is the half that has to
//! be shared.

use chrono::{DateTime, Utc};

use super::promote::{PromoteOutcome, promote_stored_order};
use super::sl_target::{SlAction, SpreadInputs, sl_target};
use crate::broker::Broker;
use crate::pending_lifecycle::{EnterConfigProvider, VerifiedSource};
use crate::spread_blackout::spread_forecast_frac;
use crate::state::StateStore;

/// Which parked orders a [`promote_due_orders`] pass considers.
///
/// A named enum rather than an `Option<&str>` because the two meanings are not
/// interchangeable and picking the wrong one is silent either way — see the
/// function docs for the two bugs that motivated it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromoteScope<'a> {
    /// Only records belonging to this account. What the live cron passes, once
    /// per account in its fan-out, so each pass acts through that account's own
    /// broker.
    Account(&'a str),
    /// Records with no account at all — the worker-global scope, matching the
    /// cron's `None` fan-out entry.
    Global,
    /// Every record regardless of account. What the offline replay passes: it
    /// has one broker and one plan, and its records carry the plan's account.
    Every,
}

impl PromoteScope<'_> {
    /// Does this scope include a record with account `account`?
    fn covers(self, account: Option<&str>) -> bool {
        match self {
            Self::Account(want) => account == Some(want),
            Self::Global => account.is_none(),
            Self::Every => true,
        }
    }
}

/// Re-check every parked order on `account` and promote the ones that now clear
/// their R-floor.
///
/// # Scope is an explicit [`PromoteScope`], not an `Option<&str>`
///
/// This pass enumerates *records*, which carry a real account name — unlike the
/// neighbouring
/// [`pending_order_lifecycle`](crate::pending_lifecycle::pending_order_lifecycle),
/// which enumerates *broker orders* and can pass `None` to mean "don't scope"
/// because the broker already decided whose orders it is answering for.
///
/// Overloading `Option<&str>` the same way here is a trap that cost a debugging
/// round: the replay passes no account and its records ARE written with one
/// (`m-and-w` in the sgdjpy fixture), so `r.account.as_deref() == None` matched
/// nothing and the pass ran every bar doing exactly nothing. Reading `None` as
/// "all accounts" instead swaps that for a worse bug — the live cron fans out
/// over `affected_accounts`, which legitimately yields `None` for global rows,
/// and that pass would then promote *every* account's parked orders through the
/// global broker.
///
/// So the two meanings get two names and the caller must say which it means.
///
/// Per-order errors are logged and skipped — one unpromotable park must never
/// stop the rest of the pass, exactly as the sweep refuses to abort on one bad
/// row.
pub async fn promote_due_orders<B, S, P, V>(
    broker: &B,
    store: &S,
    cfg: &P,
    src: &V,
    scope: PromoteScope<'_>,
    now: DateTime<Utc>,
) -> Vec<(String, PromoteOutcome)>
where
    B: Broker,
    S: StateStore,
    P: EnterConfigProvider,
    V: VerifiedSource,
{
    let records = match store.list_all_held_trade_records().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("order-control promote: list records: {err}");
            return Vec::new();
        }
    };
    let mut outcomes = Vec::new();
    for record in records
        .iter()
        .filter(|r| scope.covers(r.account.as_deref()) && !r.stored_orders.is_empty())
    {
        let Some(parked) = record.stored_orders.first() else {
            continue;
        };
        // One measured spread per record. Not cached across records here (unlike
        // the re-price pass): a parked order is rare, so the extra round-trip is
        // noise, and the caller's broker may cache anyway.
        let measured = match broker.get_quote(&record.instrument).await {
            Ok(q) => q.spread(),
            Err(err) => {
                // A quote we can't read contributes 0.0 rather than blocking the
                // decision — the baked forecast still applies, and `sl_target`
                // drops degenerate readings from its `max`.
                tracing::warn!(
                    "order-control promote: get_quote({}) failed: {err:?}",
                    record.instrument,
                );
                0.0
            }
        };
        let (expected_this_hour, expected_next_hour) =
            spread_forecast_frac(&record.instrument, now);
        // A parked order has no stop distinct from its drawn one — it was never
        // placed — so the drawn distance is both the original and the current.
        let target = sl_target(
            SpreadInputs {
                last_candle: measured,
                expected_this_hour,
                expected_next_hour,
            },
            parked.original_sl_distance,
            parked.original_sl_distance,
            parked.tp_distance,
            parked.min_r,
        );
        let clears_min_r = target.action != SlAction::BelowMinR;
        match promote_stored_order(broker, store, cfg, src, &record.trade_id, clears_min_r, now)
            .await
        {
            Ok(outcome) => {
                if outcome != PromoteOutcome::StillWaiting {
                    tracing::info!(
                        "order-control promote[{}]: {outcome:?} (r={:.3})",
                        record.trade_id,
                        target.r,
                    );
                }
                outcomes.push((record.trade_id.clone(), outcome));
            }
            Err(err) => tracing::error!("order-control promote[{}]: {err}", record.trade_id),
        }
    }
    outcomes
}
