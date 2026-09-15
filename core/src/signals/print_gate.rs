//! When may a **plain** (non-`needs_confirmed`) Pine enter fire off a latched
//! signal — on the signal's own print bar, or one bar later?
//!
//! # Why this is not just `signal_bar_time == candle.time`
//!
//! Every plain enter used to fire on the bar its pattern PRINTED. For most
//! pattern kinds that is still right. A **pinbar** is the exception, because
//! the operator's rule makes a pinbar a *pivot*:
//!
//! > "with the pinbars, they should only be marked if they are a pivot point.
//! > For a long pinbar, the low must be lower than both the left and right
//! > bars. If it is the last bar, it's considered 'pending' still"
//!
//! The **left** half (`low < low[1]`) is a print-time test and lives in the
//! detector. The **right** half cannot be: bar `N+1` has not closed when the
//! pinbar prints. So the pinbar is not yet known to be a pivot on its own print
//! bar — it is, in the operator's word, *pending*. Entering there would be
//! entering off a pattern that may be refuted by the very next bar.
//!
//! Hence the entry timing the operator asked for:
//!
//! > "In the old way, you'd just need the left bar, the pin bar, and enter
//! > straight on the pin-bar close. In the new way, you'd need the left bar
//! > close, the pin bar close, and the 2nd bar close (confirming the pinbar)."
//!
//! So a plain pinbar enter fires **one bar later**, on bar `N+1`, and only if
//! the right-hand pivot held.
//!
//! # Where the pivot verdict comes from
//!
//! **Not from a comparison recomputed here.** The right-hand pivot is resolved
//! exactly once, in [`super::state_machine`]'s `update_tracked` at
//! `bars_elapsed == 1`, which sets [`SigState::Invalid`] when it fails. This
//! gate reads that verdict off [`LatchedSignal::state`] instead of comparing
//! `candle.l` against `sig.signal_low` again. A second derivation of the same
//! rule at a call site is the documented recurring bug shape in this repo (two
//! hand-written copies of one predicate that must agree by hand), so there is
//! deliberately only one place the comparison is written.
//!
//! Reading the state also means the gate inherits, for free, every *other*
//! reason the machine can refute a pinbar inside its first bar — a breach of
//! its own extreme, or a golden-unprotected opposing signal printing. All of
//! those are reasons not to enter, and all of them arrive as `Invalid`.
//!
//! # Why `Pending` is a PASS and not a wait
//!
//! At bar `N+1` a surviving pinbar reads [`SigState::Pending`], not `Valid`:
//! `confirm_bars` is 2, so the confirmation window does not resolve until bar
//! `N+2`. Requiring `Valid` here would defer the entry by two bars and make the
//! plain enter confirmation-gated — which is the `needs_confirmed`
//! (strategy-v2 / quasimodo) path's job, not this one. The operator asked for
//! **one** extra bar close, so the gate asks only "has the pivot been refuted
//! yet?" — i.e. `state != Invalid`.
//!
//! # Scope: pinbars only
//!
//! The operator's rule names pinbars, and the other kinds have no unresolved
//! right-hand question to wait for:
//!
//! - **Tweezers** already carry their own pivot test (`twin_pivot`), which
//!   compares the pattern's shared extreme against the bar *before* the
//!   pattern — a left-side test, fully resolved at print time.
//! - **Engulfers** (regular and floating) have no pivot concept at all.
//!
//! So every non-pinbar kind keeps firing on its own print bar, byte-identically
//! to before this gate existed.

use super::state_machine::{LatchedSignal, SigState};
use crate::intent::SignalKind;

/// How many bars after its print bar a kind's plain enter fires on.
///
/// `0` for every kind whose geometry is fully settled at print time; `1` for a
/// pinbar, whose right-hand pivot needs the next bar's close (see the module
/// docs). This is the single place the deferral length is written.
fn print_delay_bars(kind: SignalKind) -> i64 {
    match kind {
        SignalKind::Pinbar => 1,
        SignalKind::Tweezer
        | SignalKind::DoubleTweezer
        | SignalKind::RegularEngulfer
        | SignalKind::FloatingEngulfer => 0,
    }
}

/// Why a plain enter declined this bar, for the `RUST_LOG=debug` trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintGate {
    /// Fire: this is the bar this kind's plain enter is due on.
    Fire,
    /// Not the due bar — too early (the pinbar's own print bar), or too late
    /// (an earlier signal merely validating here).
    WrongBar,
    /// The due bar arrived but the signal has been refuted in the meantime —
    /// for a pinbar at `N+1` that is the right-hand pivot having failed.
    Refuted,
}

/// Decide whether a **plain** (print-only) Pine enter may fire off `sig` on the
/// bar at window index `bar_idx`.
///
/// `print_idx` is the window index of `sig`'s own print bar. Both indices are
/// into the same detector window, so their difference is a bar count that a
/// session gap cannot inflate — which is why this takes indices rather than
/// timestamps.
///
/// See the module docs for the rule and its scope.
pub fn plain_enter_gate(sig: &LatchedSignal, print_idx: usize, bar_idx: usize) -> PrintGate {
    let elapsed = bar_idx as i64 - print_idx as i64;
    if elapsed != print_delay_bars(sig.kind) {
        return PrintGate::WrongBar;
    }
    // The pivot verdict, read — never recomputed. `Pending` passes: at `N+1` a
    // surviving pinbar is still inside its confirm window, and a plain enter is
    // not confirmation-gated (module docs).
    if sig.state == SigState::Invalid {
        return PrintGate::Refuted;
    }
    PrintGate::Fire
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn sig(kind: SignalKind, state: SigState) -> LatchedSignal {
        // Only `kind` and `state` are read by the gate; the rest is inert
        // filler. Built through a real `LatchedSignal` (not a bespoke pair of
        // params) so the gate is tested against the type it actually consumes.
        let t: DateTime<Utc> = "2026-01-01T00:00:00Z".parse().unwrap();
        LatchedSignal {
            direction: crate::intent::Direction::Long,
            kind,
            signal_high: 105.0,
            signal_low: 99.0,
            signal_range: 6.0,
            signal_start_time: t,
            signal_bar_time: t,
            golden: true,
            signal_confirmed: state == SigState::Valid,
            band_anchor: 100.0,
            atr: Some(1.0),
            recent_high: Some(106.0),
            recent_low: Some(98.0),
            state,
            fires: false,
        }
    }

    /// A pinbar's own print bar is NOT the due bar — the right-hand pivot is
    /// unresolved there, which is the operator's "if it is the last bar, it's
    /// considered pending still".
    #[test]
    fn a_pinbar_does_not_fire_on_its_own_print_bar() {
        let s = sig(SignalKind::Pinbar, SigState::Pending);
        assert_eq!(plain_enter_gate(&s, 4, 4), PrintGate::WrongBar);
    }

    /// One bar later, with the pivot unrefuted, it fires.
    #[test]
    fn a_pinbar_fires_one_bar_after_its_print_bar() {
        let s = sig(SignalKind::Pinbar, SigState::Pending);
        assert_eq!(plain_enter_gate(&s, 4, 5), PrintGate::Fire);
    }

    /// The pivot failed (or the pinbar was otherwise refuted inside its first
    /// bar), so the due bar declines. This is the whole point of the rule.
    #[test]
    fn a_pinbar_whose_pivot_failed_does_not_fire() {
        let s = sig(SignalKind::Pinbar, SigState::Invalid);
        assert_eq!(plain_enter_gate(&s, 4, 5), PrintGate::Refuted);
    }

    /// `Valid` at `N+1` is impossible with `confirm_bars >= 2`, but if a future
    /// config shortened the window it must still fire — the gate asks "not
    /// refuted", never "confirmed" (a plain enter is not confirmation-gated).
    #[test]
    fn a_pinbar_already_valid_at_the_due_bar_still_fires() {
        let s = sig(SignalKind::Pinbar, SigState::Valid);
        assert_eq!(plain_enter_gate(&s, 4, 5), PrintGate::Fire);
    }

    /// Two bars after the print is past the due bar — the pinbar's window is
    /// resolving and this is a confirmation, not an occurrence. Declined, the
    /// same way a plain enter has always declined a retroactive confirmation.
    #[test]
    fn a_pinbar_does_not_fire_two_bars_after_its_print_bar() {
        let s = sig(SignalKind::Pinbar, SigState::Valid);
        assert_eq!(plain_enter_gate(&s, 4, 6), PrintGate::WrongBar);
    }

    /// Every NON-pinbar kind is untouched: it fires on its own print bar, and
    /// is NOT deferred to the bar after. Table-driven over the whole enum so a
    /// newly-added kind cannot silently inherit the pinbar deferral — the match
    /// in `print_delay_bars` is exhaustive, so a new variant is a compile error
    /// there, and this test then pins whichever answer was chosen.
    #[test]
    fn non_pinbar_kinds_still_fire_on_their_print_bar() {
        for kind in [
            SignalKind::Tweezer,
            SignalKind::DoubleTweezer,
            SignalKind::RegularEngulfer,
            SignalKind::FloatingEngulfer,
        ] {
            let s = sig(kind, SigState::Pending);
            assert_eq!(
                plain_enter_gate(&s, 4, 4),
                PrintGate::Fire,
                "{kind:?} must fire on its own print bar"
            );
            assert_eq!(
                plain_enter_gate(&s, 4, 5),
                PrintGate::WrongBar,
                "{kind:?} must NOT be deferred to the bar after"
            );
        }
    }

    /// A non-pinbar that has been refuted by its print bar is still declined —
    /// the `Invalid` check is not pinbar-scoped, and must not become so: an
    /// engulfer whose own extreme was taken out on its print bar is no entry
    /// either.
    #[test]
    fn a_refuted_non_pinbar_does_not_fire_on_its_print_bar() {
        let s = sig(SignalKind::RegularEngulfer, SigState::Invalid);
        assert_eq!(plain_enter_gate(&s, 4, 4), PrintGate::Refuted);
    }
}
