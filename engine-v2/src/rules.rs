//! The rule implementations.
//!
//! Slice 1 ships exactly one rule — [`BreakAndClose`]. A v2 [`PlanRule`]'s role
//! is its typed [`RuleKind`] directly (no v1-style `rule_id`-substring
//! classification — v2 plans are always freshly baked with the typed field), so
//! selecting break-and-close rules is a plain `kind == RuleKind::BreakAndClose`.

mod break_and_close;
pub use break_and_close::BreakAndClose;

use crate::plan::{PlanRule, RuleKind};

/// Is this rule the break-and-close prep?
pub fn is_break_and_close(rule: &PlanRule) -> bool {
    rule.kind == RuleKind::BreakAndClose
}
