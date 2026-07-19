//! Behaviour classification for a plan rule — the *role* a rule plays in the
//! engine spine, resolved once from its [`AlertBasename`].
//!
//! Motivation (v73 bug, `invalidation_vetos_arm_pre_break`): the engine used to
//! answer "what kind of guard is this?" independently in six places keyed three
//! different ways (`Action` enum, `rule_id.contains(..)`, terminality-of-action)
//! with nothing forcing them to agree — and they disagreed, silently
//! mis-journaling a live trade. [`RuleKind`] collapses that to one typed answer
//! carried on the rule, derived here from the already-typed [`AlertBasename`].
//!
//! This module is pure classification — it names behaviour classes and the
//! basename → class map. The engine reads the class; it does not live here.

use crate::AlertBasename;

/// The behaviour class of a plan rule in the engine spine. Resolved once at arm
/// time (via [`From<AlertBasename>`]) and carried on the rule so every engine
/// classifier reads *this* rather than re-deriving from the `rule_id` string or
/// the resolved `Action`.
///
/// [`From<AlertBasename>`]: RuleKind#impl-From<AlertBasename>-for-RuleKind
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    /// too-high / too-low / trade-expiry / mw-cancel / mw-abort / mw-overshoot.
    /// A *setup* invalidation: firing **retires the plan** (terminal), and it is
    /// armed in **every** phase — a setup can be invalidated before it ever
    /// breaks-and-closes (price runs up away from a short and the H&S is void).
    SetupInvalidation,
    /// 06-close-on-reversal / 07-close-on-sr-reversal. A per-*position* close:
    /// it **closes the open position** but is **non-terminal** (the plan lives
    /// on and may re-enter), and it needs a position to act on so it is armed
    /// from `AwaitEntry` onward only. This is a CLOSE (flatten one position),
    /// **not** a VETO/INVALIDATE (which stops future entries but leaves an open
    /// position running) — see the vocabulary glossary in CLAUDE.md. Renamed
    /// from `PerTradeExit` (2026-07-19): "close" is the right word (it closes
    /// the existing position) and the scope is per-*position*, not per-trade.
    #[serde(alias = "per_trade_exit")]
    PerPositionClose,
    /// pause / resume / news-start / news-end. Sets the worker's blackout /
    /// news-window state on a wall-clock fire. Always-armed, non-terminal —
    /// never touches the trade's spine.
    Control,
    /// 03-prep-break-and-close. The neckline break-and-close spine prep: on fire
    /// it advances the phase off `AwaitBreakAndClose`.
    PrepBreakAndClose,
    /// 04-prep-retest. The neckline retest spine prep, gating the entry.
    PrepRetest,
    /// 08-prep-expire-<step>. Blocks a further `prep` for its step past a
    /// cutoff. Not evaluated by the engine spine (worker-side gate); kept as its
    /// own class so it is unambiguously neither a guard, a control, nor a prep
    /// the spine acts on.
    PrepExpire,
    /// 05-enter / 09-enter-qm. The entry rule(s).
    Enter,
    /// No `kind` was carried — a plan signed before this field existed. The
    /// engine falls back to the legacy `rule_id`/`Action` derivation for these
    /// (see the migration note in the scoping doc). This is the `#[serde(default)]`
    /// variant *precisely so* an absent field never silently misclassifies an old
    /// plan's rule as a real kind.
    #[default]
    Unspecified,
}

impl From<AlertBasename> for RuleKind {
    fn from(basename: AlertBasename) -> Self {
        RuleKind::from(&basename)
    }
}

impl From<&AlertBasename> for RuleKind {
    fn from(basename: &AlertBasename) -> Self {
        match basename {
            AlertBasename::VetoTooHigh
            | AlertBasename::VetoTooLow
            | AlertBasename::VetoTradeExpiry
            | AlertBasename::VetoMwCancel
            | AlertBasename::VetoMwAbort
            | AlertBasename::VetoMwOvershoot => RuleKind::SetupInvalidation,
            AlertBasename::CloseOnReversal | AlertBasename::CloseOnSrReversal => {
                RuleKind::PerPositionClose
            }
            AlertBasename::PauseStart(_)
            | AlertBasename::PauseResume(_)
            | AlertBasename::NewsStart(_)
            | AlertBasename::NewsEnd(_) => RuleKind::Control,
            AlertBasename::PrepBreakAndClose => RuleKind::PrepBreakAndClose,
            AlertBasename::PrepRetest => RuleKind::PrepRetest,
            AlertBasename::PrepExpire(_) => RuleKind::PrepExpire,
            AlertBasename::Enter | AlertBasename::EnterQm => RuleKind::Enter,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    /// Every basename resolves to a role, and every terminal-guard basename maps
    /// to `SetupInvalidation` (the class whose mis-arming was the v73 bug).
    #[test]
    fn terminal_guards_are_setup_invalidation() {
        for b in [
            AlertBasename::VetoTooHigh,
            AlertBasename::VetoTooLow,
            AlertBasename::VetoTradeExpiry,
            AlertBasename::VetoMwCancel,
            AlertBasename::VetoMwAbort,
            AlertBasename::VetoMwOvershoot,
        ] {
            assert_eq!(RuleKind::from(&b), RuleKind::SetupInvalidation, "{b:?}");
        }
    }

    #[test]
    fn close_guards_are_per_position_close() {
        assert_eq!(
            RuleKind::from(&AlertBasename::CloseOnReversal),
            RuleKind::PerPositionClose
        );
        assert_eq!(
            RuleKind::from(&AlertBasename::CloseOnSrReversal),
            RuleKind::PerPositionClose
        );
    }

    #[test]
    fn control_bars_are_control() {
        for b in [
            AlertBasename::PauseStart(String::from("x")),
            AlertBasename::PauseResume(String::from("x")),
            AlertBasename::NewsStart(String::from("x")),
            AlertBasename::NewsEnd(String::from("x")),
        ] {
            assert_eq!(RuleKind::from(&b), RuleKind::Control, "{b:?}");
        }
    }

    #[test]
    fn preps_and_enters_map_distinctly() {
        assert_eq!(
            RuleKind::from(&AlertBasename::PrepBreakAndClose),
            RuleKind::PrepBreakAndClose
        );
        assert_eq!(
            RuleKind::from(&AlertBasename::PrepRetest),
            RuleKind::PrepRetest
        );
        assert_eq!(
            RuleKind::from(&AlertBasename::PrepExpire(String::from("retest"))),
            RuleKind::PrepExpire
        );
        assert_eq!(RuleKind::from(&AlertBasename::Enter), RuleKind::Enter);
        assert_eq!(RuleKind::from(&AlertBasename::EnterQm), RuleKind::Enter);
    }

    /// The serde default MUST be `Unspecified`, never a real kind — an absent
    /// field on a pre-field plan must fall back to legacy derivation, not
    /// silently claim a class. This is the migration footgun guard.
    #[test]
    fn default_is_unspecified() {
        assert_eq!(RuleKind::default(), RuleKind::Unspecified);
    }
}
