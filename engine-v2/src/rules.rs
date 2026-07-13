//! The rule implementations, plus the shared rule-classification helpers.
//!
//! Slice 1 ships exactly one rule — [`BreakAndClose`]. The classifiers
//! ([`is_break_and_close`], [`resolved_kind`]) are ported from the old engine so
//! the driver picks out the same break-and-close rule the old engine does,
//! including the legacy `rule_id`-substring fallback for plans signed before the
//! typed `RuleKind` field existed.

mod break_and_close;
pub use break_and_close::BreakAndClose;

use trade_control_core::intent::Action;
use trade_control_core::trade_plan::{ConditionRule, RuleKind};

/// Substring identifying the break-and-close rule's role from its `rule_id`
/// (the alert basename, e.g. `03-prep-break-and-close`). Matched by `contains`
/// so the numeric prefix and any suffix don't matter — mirrors the old engine's
/// `ROLE_BREAK_AND_CLOSE`.
const ROLE_BREAK_AND_CLOSE: &str = "prep-break-and-close";
const ROLE_RETEST: &str = "prep-retest";

/// Is this rule the break-and-close prep? Port of the old engine's
/// `is_break_and_close`.
pub fn is_break_and_close(rule: &ConditionRule) -> bool {
    matches!(resolved_kind(rule), RuleKind::PrepBreakAndClose)
}

/// The rule's behaviour class — the typed [`RuleKind`] `tv-arm` stamps, with the
/// legacy fallback for plans signed before that field existed. Port of the old
/// engine's `resolved_kind`.
fn resolved_kind(rule: &ConditionRule) -> RuleKind {
    match rule.kind {
        RuleKind::Unspecified => legacy_kind(rule),
        kind => kind,
    }
}

/// Legacy classification for a plan signed before the `kind` field. Port of the
/// old engine's `legacy_kind` — order matters: the `rule_id` role-checks
/// (break-and-close / retest) win over the coarser `Action` split.
fn legacy_kind(rule: &ConditionRule) -> RuleKind {
    if rule.rule_id.contains(ROLE_BREAK_AND_CLOSE) {
        return RuleKind::PrepBreakAndClose;
    }
    if rule.rule_id.contains(ROLE_RETEST) {
        return RuleKind::PrepRetest;
    }
    match rule.intent.action {
        Action::Veto | Action::Invalidate => RuleKind::SetupInvalidation,
        Action::Close => RuleKind::PerTradeExit,
        Action::Pause | Action::Resume | Action::NewsStart | Action::NewsEnd => RuleKind::Control,
        Action::Enter => RuleKind::Enter,
        _ => RuleKind::Unspecified,
    }
}
