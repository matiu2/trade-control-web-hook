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
//! Slice 1 uses two kinds on the break-and-close line: `"break_close"` (the
//! `At(time)` stamp of the genuine close-through) and `"last_close"` (the
//! per-rule prior-close bookkeeping an `OnClose` cross measures against — v1 held
//! this in `PlanState.last_close`; here it's just another fact, so the fact
//! blackboard is the *only* state).

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

/// The per-plan fact blackboard, keyed by `(line, kind)`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Facts {
    // A HashMap keyed by a `(String, String)` tuple doesn't serialize as a JSON
    // object (JSON keys must be strings), so store as a Vec of entries. The
    // ordering is not semantically meaningful; lookups go through the methods.
    entries: Vec<FactEntry>,
}

/// One `(line, kind) -> value` entry. Flat so it serializes as a JSON array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct FactEntry {
    line: String,
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
}
