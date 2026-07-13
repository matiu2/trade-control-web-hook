//! [`Facts`] — the fact blackboard rules read and write.
//!
//! This is the substrate that replaces v1's `Phase` state machine. Rules never
//! call each other; they meet on facts keyed by `(line, kind)`. `trade_id` is
//! implicit — there is **one `Facts` per plan** (see
//! `SCOPING-rule-based-engine.md`, "Fact-based — NO central state machine").
//!
//! Example keyspace for one H&S trade:
//!
//! - break-and-close writes `("neckline", "break_close")`.
//! - retest reads that, and on its own cross writes `("neckline", "retest")`.
//! - enter reads both and decides.
//!
//! # Two namespaces: shared facts vs rule-private scratch
//!
//! There are two kinds of state, and mixing them is a latent bug:
//!
//! - **Shared facts**, keyed `(line, kind)` — semantic *trade* state one rule
//!   writes and other rules read: `("neckline", "break_close")`,
//!   `("neckline", "retest")`, later `blackout_active`, `stop_widened`, …. This
//!   is the blackboard rules coordinate through.
//! - **Rule-private scratch**, keyed `(rule_id, kind)` — a single rule's internal
//!   cross-detection bookkeeping that no other rule should ever read. Break-and-
//!   close's `last_close` (the prior close an `OnClose` cross measures against —
//!   v1 held it in `PlanState.last_close`) is the first example.
//!
//! **Why the split (Option A — separate scratch field, not a reserved prefix).**
//! `last_close` must NOT live in the `(line, kind)` map: it is keyed by line
//! there, so a future rule reading `("neckline", "last_close")` would treat one
//! rule's private bookkeeping as a shared trade fact. Namespacing scratch by
//! **`rule_id`** (whose bookkeeping it is) and holding it in its **own field**
//! means iterating or reading shared facts can *never* surface scratch — the two
//! are structurally separated, not separated by a naming convention a careless
//! `get` could bypass. Scratch still serializes (it's persisted state), just in
//! its own field. Option B (one map, underscore-prefixed private kinds) was
//! rejected: it keeps scratch keyed by `line`, still leaks through a raw `get`
//! unless every reader remembers the prefix rule, and is a weaker guarantee for
//! no real saving (the extra field is tiny).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A single fact's value. Kept deliberately small — a fact is either a
/// timestamp (when something happened) or a flag/number.
///
/// `Num` carries the per-rule `last_close` bookkeeping (a bar's close price) so
/// the whole blackboard is one uniform map; `Flag` is here for the boolean facts
/// later slices need (`blackout_active`, `stop_widened`, …). `At` is the common
/// case — "this happened at time T".
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum FactValue {
    /// A wall-clock instant — "this happened at T" (e.g. the break-close time).
    At(DateTime<Utc>),
    /// A boolean flag (e.g. `blackout_active`).
    Flag(bool),
    /// A number — slice 1 uses it for a rule's prior-close bookkeeping.
    Num(f64),
}

/// The per-plan fact blackboard: shared trade facts keyed `(line, kind)` plus a
/// separate rule-private scratch area keyed `(rule_id, kind)`.
///
/// See the module docs for why scratch is a distinct field, not a reserved kind
/// prefix inside `entries`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Facts {
    // A HashMap keyed by a `(String, String)` tuple doesn't serialize as a JSON
    // object (JSON keys must be strings), so store as a Vec of entries. The
    // ordering is not semantically meaningful; lookups go through the methods.
    entries: Vec<FactEntry>,
    // Rule-private scratch, kept out of `entries` so shared-fact reads/iteration
    // can never surface it. Same flat-Vec reasoning as `entries`.
    #[serde(default)]
    scratch: Vec<ScratchEntry>,
}

/// One `(line, kind) -> value` shared fact. Flat so it serializes as a JSON array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct FactEntry {
    line: String,
    kind: String,
    value: FactValue,
}

/// One `(rule_id, kind) -> value` rule-private scratch entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ScratchEntry {
    rule_id: String,
    kind: String,
    value: FactValue,
}

impl Facts {
    /// Empty blackboard.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set (or overwrite) the fact at `(line, kind)`.
    pub fn set(&mut self, line: &str, kind: &str, v: FactValue) {
        if let Some(e) = self
            .entries
            .iter_mut()
            .find(|e| e.line == line && e.kind == kind)
        {
            e.value = v;
        } else {
            self.entries.push(FactEntry {
                line: line.to_string(),
                kind: kind.to_string(),
                value: v,
            });
        }
    }

    /// Read the raw fact at `(line, kind)`.
    pub fn get(&self, line: &str, kind: &str) -> Option<&FactValue> {
        self.entries
            .iter()
            .find(|e| e.line == line && e.kind == kind)
            .map(|e| &e.value)
    }

    /// Convenience: the `At(time)` at `(line, kind)`, if set to a timestamp.
    pub fn at(&self, line: &str, kind: &str) -> Option<DateTime<Utc>> {
        match self.get(line, kind) {
            Some(FactValue::At(t)) => Some(*t),
            _ => None,
        }
    }

    /// Convenience: the `Num(n)` at `(line, kind)`, if set to a number.
    pub fn num(&self, line: &str, kind: &str) -> Option<f64> {
        match self.get(line, kind) {
            Some(FactValue::Num(n)) => Some(*n),
            _ => None,
        }
    }

    /// Convenience: is any fact set at `(line, kind)`?
    pub fn is_set(&self, line: &str, kind: &str) -> bool {
        self.get(line, kind).is_some()
    }

    // --- Rule-private scratch, keyed `(rule_id, kind)` -------------------------

    /// Set (or overwrite) the rule-private scratch value at `(rule_id, kind)`.
    /// Never surfaces through the shared-fact accessors above.
    pub fn set_scratch(&mut self, rule_id: &str, kind: &str, v: FactValue) {
        if let Some(e) = self
            .scratch
            .iter_mut()
            .find(|e| e.rule_id == rule_id && e.kind == kind)
        {
            e.value = v;
        } else {
            self.scratch.push(ScratchEntry {
                rule_id: rule_id.to_string(),
                kind: kind.to_string(),
                value: v,
            });
        }
    }

    /// Read the raw rule-private scratch value at `(rule_id, kind)`.
    pub fn get_scratch(&self, rule_id: &str, kind: &str) -> Option<&FactValue> {
        self.scratch
            .iter()
            .find(|e| e.rule_id == rule_id && e.kind == kind)
            .map(|e| &e.value)
    }

    /// Convenience: the `Num(n)` scratch at `(rule_id, kind)`, if a number.
    pub fn num_scratch(&self, rule_id: &str, kind: &str) -> Option<f64> {
        match self.get_scratch(rule_id, kind) {
            Some(FactValue::Num(n)) => Some(*n),
            _ => None,
        }
    }
}
