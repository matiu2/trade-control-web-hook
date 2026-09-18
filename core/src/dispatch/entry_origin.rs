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
//! ## What a replacement re-points, and what it deliberately does not
//!
//! `broker_order_id` **and** `stop_loss_price` are both corrected, in one write
//! (`StateStore::set_entry_attempt_replacement`). They record the same event —
//! this replacement landing — so splitting them would admit a row naming the new
//! order while still carrying the old order's stop.
//!
//! The stop was added after the direction question the earlier note left open
//! was actually measured, and the answer was the **unsafe** one:
//!
//! - A re-drive does **not** carry the previous stop forward. `run_enter`
//!   re-resolves the signed intent from scratch — the stop restarts at the
//!   operator's DRAWN level — and only then re-runs the SL-vs-spread floor
//!   against *today's* spread. So a stop widened by a spike at first placement
//!   comes back at the drawn level once the spread calms: **tighter** than the
//!   row, not wider. (`PriceRef::AbsoluteBuffered` gives a second, independent
//!   tighten: its buffer is `offset_atr_pct × shell.atr` resolved at fire time,
//!   and a falling ATR shrinks it. Nothing ratchets either against a prior fire.)
//! - A row holding the superseded **wider** stop makes the sweep's pre-fill
//!   breach gate fire **late**, leaving an order resting after its real stop was
//!   already traded through — the guaranteed loser that gate exists to prevent.
//!   The conservative-direction reasoning in the earlier note assumed the row
//!   could only ever hold the tighter value, which is not what the code does.
//! - The second consumer, `order_control::reprice_pass::geometry_of`, turns it
//!   into `current_sl_distance` and hence the risk budget for the *next*
//!   re-price, so a stale value feeds forward into a mis-sized stake.
//!
//! **`order_control.original_stop_loss` must NOT move.** It is deliberately the
//! DRAWN level, captured in `run_enter` before any floor widened anything, and
//! it is what a later shrink measures back toward (`sl_target` clamps
//! `desired = spread_floor.max(original_sl_distance)`). Re-pointing it at a
//! widened stop would let the stop ratchet outward and never return to the level
//! the operator actually drew.
//!
//! The remaining fields are snapshots taken at the *original* placement, and a
//! re-drive can legitimately produce different values — recorded so a later
//! reader does not mistake the omission for a claim that they are fresh:
//!
//! - **`breakeven_snapshot`**, **`order_control`** — both snapshot geometry that
//!   is re-derived from the same signed intent, so a restore reproduces them
//!   (see above for why `original_stop_loss` must stay put).
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

    /// Only a `Replacing` re-drive skips the retry gate: it continues an
    /// attempt the gate already admitted and re-points that row. A
    /// **`Promotion` is the trade's FIRST placement** — nothing was placed when
    /// it parked — so it must go through the gate like a fresh fire: it is
    /// deduplicated against what the trade holds and it WRITES the
    /// `EntryAttempt` every attempt-keyed cron and every later fire's gate
    /// reads. Skipping it (pre-2026-09-18) left promoted positions unmanaged
    /// and let the next fire enter on top of them
    /// (`BUG-spread-park-bypasses-entry-dedup.md`).
    pub fn skips_retry_gate(&self) -> bool {
        matches!(self, Self::Replacing { .. })
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
        // The gate bypass is narrower than "replacement": a promotion is the
        // first placement and must be gated + recorded.
        assert!(!EntryOrigin::Fresh.skips_retry_gate());
        assert!(!EntryOrigin::Promotion.skips_retry_gate());
        assert!(
            EntryOrigin::Replacing {
                old_broker_order_id: "o-1".into()
            }
            .skips_retry_gate()
        );
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
