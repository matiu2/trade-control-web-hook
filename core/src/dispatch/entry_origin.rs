//! [`EntryOrigin`] — **what this `run_enter` call is**, and (when it replaces a
//! resting order) **which order id it is replacing**.
//!
//! ## The gap this closes
//!
//! `run_enter` used to take a positional `restore: bool`. `true` meant three
//! things at once — skip the retry gate, skip the blackout gate, burn no
//! `max_retries` slot — all of which are correct, and one thing it did NOT do:
//! **re-point the `EntryAttempt` row at the order the broker actually placed.**
//!
//! A restore re-places the entry at the broker, which answers with a **new**
//! order id. The stored `EntryAttempt.broker_order_id` still named the old one,
//! and nothing anywhere updated it — the re-drive skips `record_placement`
//! precisely because it must not write a second row. After one hold episode the
//! row named an order the broker had discarded while a live order with a
//! different id rested on the book.
//!
//! The consumer that makes this bite is the scheduled sweep
//! (`trade-control-cron::sweep`), which calls
//! `broker.cancel_order(account, &attempt.broker_order_id)` **directly** — no
//! lookup, no fallback. Its pre-fill SL-breach cancel was therefore aimed at a
//! dead id, so the live restored order survived a breach that should have pulled
//! it and could still fill. (The retry gate reads the same field but routes it
//! through `lookup_attempt_state` first, where a dead id resolves to
//! `AttemptState::Unknown` and rejects — it forfeits a re-entry rather than
//! stacking a duplicate, so that consumer fails safe.)
//!
//! Note the honest scope: the SL-breach sweep has been *measured* as
//! outcome-inert on the fixture corpus (it fires on 854 orders and changes 0
//! outcomes), so this is a real hole in a protection that has not yet
//! demonstrably saved a trade — not a bug with a known cost attached.
//!
//! ## Why an enum with a payload, not a second `bool`
//!
//! The old shape let a caller say "this is a restore" without saying *what it is
//! restoring*, which is exactly how the re-point came to be missing: there was
//! no place to put the answer, so nobody noticed the question. Carrying the old
//! order id **inside** the variant makes the two inseparable — a caller cannot
//! claim to be replacing a resting order without naming it — and a new
//! restore-style caller gets a compile error instead of a silent omission.
//!
//! It also distinguishes the two cases the `bool` conflated, which really are
//! different: a **re-price/restore** replaces an order that reached the broker
//! and left a row behind, whereas a **promotion** places a setup that was parked
//! *before* ever reaching the broker, so it has no broker id and no row to
//! re-point.
//!
//! ## Known-stale siblings, deliberately NOT re-pointed here
//!
//! Only `broker_order_id` is corrected. The other `EntryAttempt` fields are
//! snapshots taken at the *original* placement, and a re-drive can legitimately
//! produce different values — this is recorded so a later reader does not
//! mistake the omission for a claim that they are fresh:
//!
//! - **`stop_loss_price`** — genuinely can go stale. A re-drive re-runs the
//!   SL-vs-spread floor against the spread at the *restore* bar, and the widen
//!   mutates `resolved.stop_loss` in place, so the order may now rest behind a
//!   wider stop than the row records. Two consumers read this field: the sweep's
//!   pre-fill breach gate and `order_control::reprice_pass`. Left alone here
//!   because it is a separate decision with its own direction-of-safety
//!   question (a row holding the *tighter* drawn stop makes the breach gate fire
//!   *earlier*, which is the conservative side), and because fixing it belongs
//!   with a measurement rather than bundled into an id correction.
//! - **`breakeven_snapshot`**, **`order_control`** — both snapshot geometry that
//!   is re-derived from the same signed intent, so a restore reproduces them;
//!   `order_control.original_stop_loss` is deliberately the DRAWN stop, which a
//!   re-floor does not move.
//! - **`cancel_at`**, **`direction`**, **`attempt_no`**, **`shell_time`** —
//!   properties of the original fire, and correctly unchanged by a re-placement
//!   of that same entry.
//! - **`placed_at`** — still the original placement time. That is arguably the
//!   honest reading (the attempt began then), and nothing bounds a decision on
//!   it; note the separate hazard that `placed_at` is not a *fill* time.

/// What this `run_enter` call is, from the caller's point of view.
///
/// Gate behaviour keys off [`Self::is_replacement`]: a replacement of an order
/// we already placed must not be re-gated as if it were a fresh fire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryOrigin {
    /// A **fresh fire** — a webhook alert or an engine rule firing this bar.
    /// The full gate chain applies: retry gate, blackout gate, and a successful
    /// placement records a new `EntryAttempt` row against a `max_retries` slot.
    Fresh,
    /// **Re-placing a resting order we cancelled**, carrying the broker order id
    /// it had before the cancel.
    ///
    /// Used by `pending_lifecycle`'s hold-release restore (RAIL 7) and by
    /// `order_control::reprice`. Both cancel a real broker order and put an
    /// equivalent one back, so this is a continuation of the original attempt,
    /// not a new entry: it skips the retry gate (it would otherwise be
    /// `retry-fire-replay`-rejected on its own already-seen `shell.time`),
    /// skips the blackout gate, records no second row — and **re-points the
    /// existing row** at whatever id the broker answers with.
    Replacing {
        /// The order id the `EntryAttempt` row currently holds — the one being
        /// replaced. Matching on it is what lets the update find its row
        /// without an `attempt_no` the caller never learns.
        old_broker_order_id: String,
    },
    /// **Promoting a parked setup** that never reached the broker.
    ///
    /// `order_control::promote` places a setup that was held back (below
    /// min-R, below min size). It shares the gate bypasses — the intent was
    /// already accepted, and its `shell.time` was already marked seen — but has
    /// **no** broker order id and therefore no row to re-point: the park
    /// happened instead of a placement, not after one.
    Promotion,
}

impl EntryOrigin {
    /// Whether this call re-places an entry we already put through the gates,
    /// rather than firing a fresh one. Drives the retry-gate and blackout-gate
    /// bypasses, and suppresses the new-`EntryAttempt` write.
    pub fn is_replacement(&self) -> bool {
        !matches!(self, Self::Fresh)
    }

    /// The broker order id this call replaces, when there is one to re-point.
    /// `None` for a fresh fire (nothing to replace) and for a promotion (the
    /// parked setup never reached the broker).
    pub fn replaced_order_id(&self) -> Option<&str> {
        match self {
            Self::Replacing {
                old_broker_order_id,
            } => Some(old_broker_order_id),
            Self::Fresh | Self::Promotion => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All three bypass-sharing variants must agree with the old `bool`: the two
    /// replacement kinds skip the gates, a fresh fire does not.
    #[test]
    fn only_a_fresh_fire_runs_the_full_gate_chain() {
        assert!(!EntryOrigin::Fresh.is_replacement());
        assert!(EntryOrigin::Promotion.is_replacement());
        assert!(
            EntryOrigin::Replacing {
                old_broker_order_id: "ORD-1".into(),
            }
            .is_replacement()
        );
    }

    /// Only a `Replacing` names an order to re-point. A promotion shares the
    /// bypasses but has no broker id — reading one out of it would re-point some
    /// other trade's row.
    #[test]
    fn only_replacing_carries_an_order_id_to_repoint() {
        assert_eq!(EntryOrigin::Fresh.replaced_order_id(), None);
        assert_eq!(EntryOrigin::Promotion.replaced_order_id(), None);
        assert_eq!(
            EntryOrigin::Replacing {
                old_broker_order_id: "ORD-1".into(),
            }
            .replaced_order_id(),
            Some("ORD-1"),
        );
    }
}
