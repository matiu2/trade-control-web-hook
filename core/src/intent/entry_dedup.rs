//! [`EntryDedup`] — **who owns entry dedup for this enter**, stated outright
//! on the signed intent instead of being inferred from `max_retries`.
//!
//! ## The gap this closes
//!
//! `run_enter`'s retry gate is the only thing that reconciles a fire against
//! this trade's *prior attempts* — "is one still resting? is one still open?".
//! Entering that gate used to be conditional on `max_retries != Static(0)`,
//! which quietly conflated two unrelated questions:
//!
//! 1. **How many placements may this trade make?** — a *cap*. That is
//!    `max_retries`, and it is what its name says.
//! 2. **Can this enter fire more than once, so does it need reconciling
//!    against prior attempts?** — a *dedup ownership* question, which
//!    `max_retries` was never about.
//!
//! For H&S the two happen to agree: the enter is `FireMode::Once`, the engine
//! latches it to `Phase::Done` on its first fire, so it can only ever fire once
//! and needs no gate. `Static(0)` answered both questions correctly by accident.
//!
//! For **M/W** they come apart, and the accident became a live-money bug. An
//! M/W enter is `FireMode::EveryBar`: the engine deliberately never latches it
//! (`engine/src/evaluate.rs` — "the worker's run_enter owns the actual
//! placement/dedup"), because M/W recomputes its geometry from each new shell
//! and a resting order should track it. Its builder set `max_retries: Static(0)`
//! to mean answer-1, "only ever one placement" — which is true and remains
//! true. But that same value was read as answer-2, "no reconciliation needed",
//! so the delegated owner was never invoked. The engine fired every bar, nothing
//! downstream deduped, and on 2026-08-20 an EUR/GBP double-top placed and filled
//! three entries on three consecutive bars: 3× the intended risk, stopped only
//! by the blunt account-wide open-positions cap.
//!
//! See `BUG-mw-everybar-enter-skips-retry-gate.md`.
//!
//! ## Why a named enum and not another bool / magic value
//!
//! The defect was a *magic value* carrying two meanings. Replacing it with a
//! `bool` would only rename the trap, and a positional `bool` at a call site is
//! exactly what this repo's conventions forbid. Naming the two states makes the
//! question a pattern author has to answer explicitly, and makes the answer
//! readable at the call site (`intent.entry_dedup.needs_retry_gate()`) rather
//! than inferable only by knowing what `Static(0)` implies three layers down.
//!
//! The cap keeps its own field. An M/W enter is now
//! `entry_dedup: GateOwned` + `max_retries: 1`: reconcile every bar, but never
//! place more than one entry — a stop-out is still terminal.

use serde::{Deserialize, Serialize};

/// Who is responsible for making sure this enter places at most one entry.
///
/// Signed onto the intent at arm time, so it cannot be flipped in flight.
/// Carried as `Option<EntryDedup>` on the intent: **absent** (a pre-field
/// intent) is distinct from an explicit `EngineLatched`, and only the former is
/// healed — see [`effective_entry_dedup`].
// No `Default`, deliberately: there is no safe "obvious" answer to pick without
// looking at the enter, and the absent case is modelled by `Option::None` (see
// `effective_entry_dedup`) rather than by a default variant. A `Default` here
// would invite `..Default::default()` at a construction site and re-open the
// gap this type exists to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryDedup {
    /// **The engine guarantees single firing.** The enter is `FireMode::Once`
    /// and latches to `Phase::Done` on its first fire, so a second fire is
    /// impossible by construction and there is nothing to reconcile. The retry
    /// gate is skipped entirely — no prior-attempt lookup, no broker calls, no
    /// state-store reads. This is the H&S single-shot path and the historical
    /// default; keeping it gate-free is what makes this fix a no-op for it.
    EngineLatched,
    /// **The retry gate owns dedup.** The enter can fire on many bars (an
    /// `EveryBar` heartbeat, or a multi-shot re-entry), so every fire must be
    /// reconciled against this trade's prior attempts before placing: a still
    /// **resting** order is cancelled and re-placed at the new price, a still
    /// **open** position rejects the fire, and the `max_retries` cap bounds the
    /// total. Requires a `trade_id` — there is nothing to correlate attempts by
    /// without one.
    GateOwned,
}

impl EntryDedup {
    /// Whether `run_enter` must run the retry gate for this enter.
    ///
    /// This is the single question the gate-entry condition asks. Routing it
    /// through a named method rather than a `matches!` on a `Tunable` at the
    /// call site is the point of the type: a future pattern author choosing a
    /// value here is answering "can my enter fire twice?", not guessing at what
    /// a cap of zero implies.
    pub fn needs_retry_gate(self) -> bool {
        matches!(self, Self::GateOwned)
    }
}

/// The dedup owner to actually act on, **healing pre-field intents at read
/// time** rather than trusting the bare serde default.
///
/// An intent minted before `entry_dedup` existed carries no field, so serde
/// gives [`EntryDedup::EngineLatched`]. For a single-shot enter that is exactly
/// right — it is what such an intent always did. But for a **multi-shot** one
/// (`max_retries > 0`) it is wrong: that intent WAS gate-owned under the old
/// `max_retries != Static(0)` rule, and reading it as engine-latched would
/// silently switch its dedup off — the very bug this field exists to fix,
/// re-introduced for every plan armed before the change (and for all 4041
/// saved corpus enters, which are all `max_retries: 5`).
///
/// So the legacy rule is re-derived for exactly the case where it was the
/// authority. Only an **absent** field is healed; any explicitly stated value
/// is obeyed as written, in both directions.
///
/// Deliberately NOT a serde default: the stored body stays a faithful record of
/// what was signed, and the healing is visible at the point of use. Same posture
/// as `HeldTradeRecord::effective_holders` (v120).
pub(crate) fn effective_entry_dedup(
    stored: Option<EntryDedup>,
    max_retries: &crate::tunable::Tunable<u32>,
) -> EntryDedup {
    match stored {
        // Stated explicitly at arm time — obeyed as written, in BOTH directions.
        // This is what keeps the field authoritative rather than advisory: an
        // explicit `EngineLatched` is not second-guessed from the cap.
        Some(explicit) => explicit,
        // Absent: a pre-field intent. Re-derive the rule that WAS the authority
        // when it was signed.
        None if matches!(max_retries, crate::tunable::Tunable::Static(0)) => {
            EntryDedup::EngineLatched
        }
        None => EntryDedup::GateOwned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the type: only `GateOwned` reaches the gate.
    #[test]
    fn only_gate_owned_needs_the_retry_gate() {
        assert!(EntryDedup::GateOwned.needs_retry_gate());
        assert!(!EntryDedup::EngineLatched.needs_retry_gate());
    }

    /// A body with no `entry_dedup` — every intent minted before this field
    /// existed — deserializes to `None`, which is DISTINCT from an explicit
    /// `EngineLatched`. That distinction is the whole back-compat mechanism:
    /// only `None` is healed.
    #[test]
    fn absent_field_deserializes_to_none_not_a_variant() {
        #[derive(Deserialize)]
        struct Holder {
            #[serde(default)]
            entry_dedup: Option<EntryDedup>,
        }
        let h: Holder = serde_json::from_str("{}").expect("empty body deserializes");
        assert_eq!(h.entry_dedup, None);
        let h: Holder = serde_json::from_str(r#"{"entry_dedup":"engine_latched"}"#)
            .expect("explicit body deserializes");
        assert_eq!(h.entry_dedup, Some(EntryDedup::EngineLatched));
    }

    /// A pre-field MULTI-SHOT intent must keep its gate. Serde hands us
    /// `EngineLatched` (no field on the wire), but under the old
    /// `max_retries != Static(0)` rule that intent WAS gate-owned — reading it
    /// literally would silently disable dedup on every plan armed before this
    /// field existed.
    #[test]
    fn legacy_multi_shot_intent_heals_to_gate_owned() {
        assert_eq!(
            effective_entry_dedup(None, &crate::tunable::Tunable::Static(5)),
            EntryDedup::GateOwned
        );
    }

    /// A pre-field SINGLE-SHOT intent stays engine-latched — that is genuinely
    /// what it always did, and it must keep making zero gate calls.
    #[test]
    fn legacy_single_shot_intent_stays_engine_latched() {
        assert_eq!(
            effective_entry_dedup(None, &crate::tunable::Tunable::Static(0)),
            EntryDedup::EngineLatched
        );
    }

    /// A script `max_retries` counts as multi-shot, mirroring the old rule
    /// (we can't resolve it here, and writing one means the operator meant it).
    #[test]
    fn legacy_script_max_retries_heals_to_gate_owned() {
        assert_eq!(
            effective_entry_dedup(None, &crate::tunable::Tunable::from_script("3")),
            EntryDedup::GateOwned
        );
    }

    /// **The field is AUTHORITATIVE, not advisory.** An explicit
    /// `EngineLatched` is obeyed even with a non-zero cap — it is NOT
    /// second-guessed from `max_retries`. Without this, healing would re-derive
    /// the gate from the cap in every case and `entry_dedup` would carry no
    /// independent information at runtime (the two questions would be
    /// re-conflated, just one layer further down).
    #[test]
    fn an_explicit_engine_latched_is_obeyed_even_with_a_nonzero_cap() {
        assert_eq!(
            effective_entry_dedup(
                Some(EntryDedup::EngineLatched),
                &crate::tunable::Tunable::Static(5)
            ),
            EntryDedup::EngineLatched
        );
    }

    /// An explicitly stored `GateOwned` is never downgraded, whatever the cap.
    #[test]
    fn stored_gate_owned_always_wins() {
        assert_eq!(
            effective_entry_dedup(
                Some(EntryDedup::GateOwned),
                &crate::tunable::Tunable::Static(0)
            ),
            EntryDedup::GateOwned
        );
    }

    /// Wire form is snake_case, and round-trips.
    #[test]
    fn wire_form_is_snake_case() {
        let s = serde_json::to_string(&EntryDedup::GateOwned).expect("serialize");
        assert_eq!(s, "\"gate_owned\"");
        let back: EntryDedup = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, EntryDedup::GateOwned);
    }
}
