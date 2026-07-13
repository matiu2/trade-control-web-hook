//! The rule implementations.
//!
//! Slice 1 ships [`BreakAndClose`] (the first prep, a fact *producer*) and
//! [`Retest`] (the first fact *consumer* — it gates on the break-and-close fact
//! then writes its own). A v2 [`PlanRule`]'s role is its typed [`RuleKind`]
//! directly (no v1-style `rule_id`-substring classification — v2 plans are
//! always freshly baked with the typed field), so selecting a rule kind is a
//! plain `kind == RuleKind::...` check.

mod break_and_close;
mod retest;
pub use break_and_close::BreakAndClose;
pub use retest::Retest;

use crate::plan::{PlanRule, RuleKind};

/// Is this rule the break-and-close prep?
pub fn is_break_and_close(rule: &PlanRule) -> bool {
    rule.kind == RuleKind::BreakAndClose
}

/// Is this rule the retest prep?
pub fn is_retest(rule: &PlanRule) -> bool {
    rule.kind == RuleKind::Retest
}
