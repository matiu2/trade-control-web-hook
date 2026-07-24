//! A single entry in an enter's [`requires_preps`](crate::intent::Intent::requires_preps)
//! gate.
//!
//! Historically `requires_preps` was a flat `Vec<String>` — an **ordered AND**
//! list of prep step names, each of which had to be set with a strictly-later
//! `set_at` than the previous. A [`PrepReq`] generalises one slot in that list
//! to allow an **either/or (OR)** group:
//!
//! ```text
//! requires_preps: [break-and-close, [retest, pullback]]
//! ```
//!
//! - `break-and-close`  → [`PrepReq::All`] — this exact prep must be set.
//! - `[retest, pullback]` → [`PrepReq::Any`] — **at least one** of these must be
//!   set (the group is satisfied by whichever alternative landed).
//!
//! The list stays **ordered** across groups: the satisfying prep in each group
//! must be set strictly after the previous group's satisfying prep. Only the
//! *within-group* choice is a disjunction. See
//! [`crate::dispatch`]'s enter gate for the runtime satisfaction logic.
//!
//! ## Wire form & back-compat
//!
//! [`PrepReq`] is `#[serde(untagged)]`, so a bare string deserialises to
//! [`PrepReq::All`] and a nested list to [`PrepReq::Any`]. Crucially this means
//! **every plan/intent signed before this type existed round-trips
//! byte-identically**: `[break-and-close, retest]` still parses (as two `All`s)
//! and re-serialises to the same two bare strings. The whole-body HMAC over a
//! legacy intent is therefore unchanged.
//!
//! A single-member `Any([x])` is semantically identical to `All(x)`; builders
//! should prefer emitting the bare-string `All` form so the wire stays clean
//! (see [`PrepReq::from_alternatives`]).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One slot in an enter's prep gate: either a single required prep, or an
/// either/or group where any one alternative satisfies it. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PrepReq {
    /// A single prep step that must be set. Wire form: a bare string
    /// (`break-and-close`).
    All(String),
    /// An either/or group — the slot is satisfied when **any** listed prep is
    /// set. Wire form: a nested list (`[retest, pullback]`). An empty group is
    /// vacuous and should not be emitted (see [`Self::from_alternatives`]).
    Any(Vec<String>),
}

impl PrepReq {
    /// Build a slot from a list of alternatives, collapsing the trivial cases so
    /// the wire form stays minimal:
    /// - `[]`  → `None` (nothing to require; the caller should drop the slot).
    /// - `[x]` → `Some(All(x))` (a lone alternative is just a required prep).
    /// - `[x, y, …]` → `Some(Any([x, y, …]))`.
    pub fn from_alternatives<I, S>(alts: I) -> Option<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut names: Vec<String> = alts.into_iter().map(Into::into).collect();
        match names.len() {
            0 => None,
            1 => Some(PrepReq::All(names.remove(0))),
            _ => Some(PrepReq::Any(names)),
        }
    }

    /// The alternative prep step names this slot admits, in wire order. For
    /// [`PrepReq::All`] this is a single name; for [`PrepReq::Any`] the whole
    /// group. Used by satisfaction / membership logic.
    pub fn alternatives(&self) -> &[String] {
        match self {
            PrepReq::All(name) => std::slice::from_ref(name),
            PrepReq::Any(names) => names,
        }
    }

    /// Does this slot admit `step` as one of its alternatives? Replaces the old
    /// `requires_preps.iter().any(|p| p == step)` membership test, which is used
    /// widely to ask "does the enter require prep X at all?".
    pub fn contains_step(&self, step: &str) -> bool {
        self.alternatives().iter().any(|s| s == step)
    }
}

/// The outcome of testing one [`PrepReq`] slot against the store, given the
/// previous slot's satisfying timestamp. Pure — computed by [`resolve_slot`] from
/// the alternatives' looked-up `set_at`s so the ordered-OR decision can be unit
/// tested without a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotOutcome {
    /// A qualifying alternative was found; its `set_at` becomes the new "previous"
    /// bar for the next slot.
    Satisfied(DateTime<Utc>),
    /// At least one alternative is set, but none is strictly after the previous
    /// slot — an ordering violation (the operator saw a stale prep).
    OutOfOrder,
    /// No alternative is set at all — a missing prep.
    Missing,
}

/// Resolve one [`PrepReq`] slot given each alternative's looked-up prep timestamp
/// (in the slot's wire order; `None` = that alternative is unset) and the previous
/// slot's satisfying timestamp (`None` for the first slot).
///
/// A slot is [`Satisfied`](SlotOutcome::Satisfied) by whichever set alternative is
/// strictly after `prev`; the **earliest** such timestamp is chosen so the next
/// slot has the loosest bar to clear. If some alternative is set but none beats
/// `prev` it is [`OutOfOrder`](SlotOutcome::OutOfOrder); if none is set at all it
/// is [`Missing`](SlotOutcome::Missing). A single-member slot (`All`) reduces to
/// exactly the historical "present and strictly after previous" rule.
pub fn resolve_slot(
    alt_set_ats: &[Option<DateTime<Utc>>],
    prev: Option<DateTime<Utc>>,
) -> SlotOutcome {
    let mut any_set = false;
    let mut earliest_qualifying: Option<DateTime<Utc>> = None;
    for set_at in alt_set_ats.iter().flatten() {
        any_set = true;
        if prev.is_none_or(|p| *set_at > p) {
            earliest_qualifying = Some(match earliest_qualifying {
                Some(existing) => existing.min(*set_at),
                None => *set_at,
            });
        }
    }
    match earliest_qualifying {
        Some(set_at) => SlotOutcome::Satisfied(set_at),
        None if any_set => SlotOutcome::OutOfOrder,
        None => SlotOutcome::Missing,
    }
}

/// Convenience membership + iteration over a whole `requires_preps` list. These
/// keep the many `requires_preps.iter().any(|p| p == X)` call sites terse after
/// the flat-`Vec<String>` → `Vec<PrepReq>` change.
pub trait PrepReqSliceExt {
    /// Does any slot in the list admit `step`? I.e. "does this enter require
    /// prep `step` (possibly as one alternative of an either/or group)?".
    fn requires_step(&self, step: &str) -> bool;
    /// Every distinct prep step name mentioned anywhere in the list, in wire
    /// order (duplicates across groups are preserved — callers that need a set
    /// should dedup).
    fn all_step_names(&self) -> Vec<&str>;
}

impl PrepReqSliceExt for [PrepReq] {
    fn requires_step(&self, step: &str) -> bool {
        self.iter().any(|req| req.contains_step(step))
    }

    fn all_step_names(&self) -> Vec<&str> {
        self.iter()
            .flat_map(|req| req.alternatives().iter().map(String::as_str))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_string_parses_as_all() {
        let req: PrepReq = serde_yaml::from_str("break-and-close").unwrap();
        assert_eq!(req, PrepReq::All("break-and-close".into()));
    }

    #[test]
    fn nested_list_parses_as_any() {
        let req: PrepReq = serde_yaml::from_str("[retest, pullback]").unwrap();
        assert_eq!(req, PrepReq::Any(vec!["retest".into(), "pullback".into()]));
    }

    #[test]
    fn all_serialises_back_to_a_bare_string() {
        // Back-compat: an `All` must round-trip to the bare string, not a tagged
        // object, so a legacy `[break-and-close, retest]` list is byte-identical.
        let yaml = serde_yaml::to_string(&PrepReq::All("retest".into())).unwrap();
        assert_eq!(yaml.trim(), "retest");
    }

    #[test]
    fn any_serialises_back_to_a_list() {
        let yaml =
            serde_yaml::to_string(&PrepReq::Any(vec!["retest".into(), "pullback".into()])).unwrap();
        // A YAML sequence; exact flow/block style doesn't matter, just that both
        // members are present and it's a list, not a bare string.
        let back: PrepReq = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, PrepReq::Any(vec!["retest".into(), "pullback".into()]));
    }

    #[test]
    fn legacy_flat_list_round_trips_byte_identically() {
        // The critical HMAC-stability property: a pre-PrepReq list parses and
        // re-serialises unchanged.
        let yaml = "[break-and-close, retest]";
        let reqs: Vec<PrepReq> = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            reqs,
            vec![
                PrepReq::All("break-and-close".into()),
                PrepReq::All("retest".into()),
            ]
        );
        let out = serde_yaml::to_string(&reqs).unwrap();
        let reparsed: Vec<PrepReq> = serde_yaml::from_str(&out).unwrap();
        assert_eq!(reparsed, reqs);
        // No tagged keys leaked into the output.
        assert!(!out.contains("All"));
        assert!(!out.contains("Any"));
    }

    #[test]
    fn mixed_list_parses() {
        let yaml = "[break-and-close, [retest, pullback]]";
        let reqs: Vec<PrepReq> = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            reqs,
            vec![
                PrepReq::All("break-and-close".into()),
                PrepReq::Any(vec!["retest".into(), "pullback".into()]),
            ]
        );
    }

    #[test]
    fn from_alternatives_collapses_trivial_cases() {
        assert_eq!(PrepReq::from_alternatives(Vec::<String>::new()), None);
        assert_eq!(
            PrepReq::from_alternatives(["retest"]),
            Some(PrepReq::All("retest".into()))
        );
        assert_eq!(
            PrepReq::from_alternatives(["retest", "pullback"]),
            Some(PrepReq::Any(vec!["retest".into(), "pullback".into()]))
        );
    }

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn resolve_all_slot_matches_legacy_present_and_after() {
        // Single-member (All) slot: present & after prev → satisfied at its own ts.
        assert_eq!(
            resolve_slot(&[Some(ts(100))], None),
            SlotOutcome::Satisfied(ts(100))
        );
        assert_eq!(
            resolve_slot(&[Some(ts(200))], Some(ts(100))),
            SlotOutcome::Satisfied(ts(200))
        );
        // Present but not strictly after prev → out of order.
        assert_eq!(
            resolve_slot(&[Some(ts(100))], Some(ts(100))),
            SlotOutcome::OutOfOrder
        );
        assert_eq!(
            resolve_slot(&[Some(ts(50))], Some(ts(100))),
            SlotOutcome::OutOfOrder
        );
        // Unset → missing.
        assert_eq!(resolve_slot(&[None], Some(ts(100))), SlotOutcome::Missing);
    }

    #[test]
    fn resolve_any_slot_satisfied_by_either_alternative() {
        // Neither set → missing.
        assert_eq!(resolve_slot(&[None, None], None), SlotOutcome::Missing);
        // Only the second alt set & after prev → satisfied by it.
        assert_eq!(
            resolve_slot(&[None, Some(ts(300))], Some(ts(100))),
            SlotOutcome::Satisfied(ts(300))
        );
        // Both set & after prev → earliest qualifying chosen (loosest next bar).
        assert_eq!(
            resolve_slot(&[Some(ts(400)), Some(ts(250))], Some(ts(100))),
            SlotOutcome::Satisfied(ts(250))
        );
    }

    #[test]
    fn resolve_any_slot_ordering_within_group() {
        // First alt set but too early, second alt set and after prev → satisfied
        // by the second. A stale alternative doesn't sink a fresh one.
        assert_eq!(
            resolve_slot(&[Some(ts(50)), Some(ts(300))], Some(ts(100))),
            SlotOutcome::Satisfied(ts(300))
        );
        // Both set but both before/at prev → out of order (something is set, but
        // nothing qualifies).
        assert_eq!(
            resolve_slot(&[Some(ts(50)), Some(ts(100))], Some(ts(100))),
            SlotOutcome::OutOfOrder
        );
    }

    #[test]
    fn contains_step_and_slice_helpers() {
        let reqs = [
            PrepReq::All("break-and-close".into()),
            PrepReq::Any(vec!["retest".into(), "pullback".into()]),
        ];
        assert!(reqs.requires_step("break-and-close"));
        assert!(reqs.requires_step("retest"));
        assert!(reqs.requires_step("pullback"));
        assert!(!reqs.requires_step("news-window"));
        assert_eq!(
            reqs.all_step_names(),
            vec!["break-and-close", "retest", "pullback"]
        );
    }
}
