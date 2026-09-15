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
//!
//! # Scope: RE-ENTRIES only (the measured variant)
//!
//! Applying the deferral to *every* pinbar enter was measured over the 2847-cell
//! fixture corpus and **lost** (−46.03R overall; −9.47R on the operator's live
//! column, with the entire loss coming out of winning R and zero losses
//! avoided). Decomposed by mechanism, the sign was carried by the FIRST entry of
//! each trade: delaying it by a bar either filled worse (−38.16R over 127 cells)
//! or missed the setup entirely (−30.75R over 52 cells). The one bucket that
//! *gained* was re-entries being suppressed (+60.94R over 49 cells) — multi-shot
//! setups that fired repeatedly on pinbars which were never pivots, which is the
//! failure the operator asked to fix.
//!
//! So the deferral is scoped to a **re-entry**: the first entry of a trade still
//! fires on the pinbar's own print bar, and only the second-and-later entry
//! waits for the pivot bar. [`Shot::First`] / [`Shot::Reentry`] carries that.
//!
//! ## Why a re-entry gets the DEFERRAL and not "the pivot must already have held"
//!
//! Those two are not interchangeable, and only one of them is implementable. On
//! the pinbar'"'"'s own print bar the right-hand pivot is *unresolved*, not failed:
//! the signal reads [`SigState::Pending`]. There is no verdict to require yet —
//! asking "has the pivot held?" there can only ever be answered "unknown". The
//! verdict exists exactly one bar later, so requiring it IS the deferral. The
//! gate therefore defers, and reads the state on the bar the state means
//! something.

use super::state_machine::{LatchedSignal, SigState};
use crate::intent::SignalKind;

/// Is a fire the trade'"'"'s **first** entry, or a **re-entry** (a later placement
/// by the same multi-shot enter)?
///
/// Only a re-entry is pivot-deferred — see the module docs for the measurement
/// that scoped it that way. A newtype-ish enum rather than a `bool` so a call
/// site cannot silently pass the wrong polarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shot {
    /// No fire of this enter has happened yet for this trade.
    First,
    /// This enter has already fired at least once for this trade.
    Reentry,
}

/// How many bars after its print bar a kind'"'"'s plain enter fires on.
///
/// `0` for every kind whose geometry is fully settled at print time, and `0` for
/// **every first entry** — the deferral is re-entry-scoped (module docs). `1`
/// only for a pinbar re-entry, whose right-hand pivot needs the next bar'"'"'s
/// close. This is the single place the deferral length is written.
fn print_delay_bars(kind: SignalKind, shot: Shot) -> i64 {
    match (kind, shot) {
        // The measured variant: a pinbar RE-entry waits for its pivot bar.
        (SignalKind::Pinbar, Shot::Reentry) => 1,
        // A pinbar'"'"'s FIRST entry fires on its own print bar, as it always has.
        // Deferring it is what cost R on the corpus.
        (SignalKind::Pinbar, Shot::First) => 0,
        // No other kind has an unresolved right-hand question, on any shot.
        (
            SignalKind::Tweezer
            | SignalKind::DoubleTweezer
            | SignalKind::RegularEngulfer
            | SignalKind::FloatingEngulfer,
            _,
        ) => 0,
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
/// `shot` says whether a fire here would be the trade'"'"'s first entry or a
/// re-entry; only a re-entry is pivot-deferred (module docs).
///
/// See the module docs for the rule and its scope.
pub fn plain_enter_gate(
    sig: &LatchedSignal,
    print_idx: usize,
    bar_idx: usize,
    shot: Shot,
) -> PrintGate {
    let elapsed = bar_idx as i64 - print_idx as i64;
    if elapsed != print_delay_bars(sig.kind, shot) {
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

    /// **The variant'"'"'s defining behaviour.** A pinbar'"'"'s FIRST entry fires on its
    /// own print bar, exactly as it did before any pivot rule existed. Deferring
    /// this is what cost the corpus R (module docs), so this is the test that
    /// goes red if the deferral is widened back to every entry.
    #[test]
    fn a_first_entry_pinbar_fires_on_its_own_print_bar() {
        let s = sig(SignalKind::Pinbar, SigState::Pending);
        assert_eq!(plain_enter_gate(&s, 4, 4, Shot::First), PrintGate::Fire);
    }

    /// ...and a first entry is NOT deferred to the bar after. The pinbar'"'"'s pivot
    /// bar is not a second chance for a first entry — that bar is a
    /// confirmation, and a plain enter has always declined those.
    #[test]
    fn a_first_entry_pinbar_does_not_fire_on_the_bar_after_its_print() {
        let s = sig(SignalKind::Pinbar, SigState::Pending);
        assert_eq!(plain_enter_gate(&s, 4, 5, Shot::First), PrintGate::WrongBar);
    }

    /// A pinbar RE-entry'"'"'s own print bar is NOT the due bar — the right-hand
    /// pivot is unresolved there, which is the operator'"'"'s "if it is the last bar,
    /// it'"'"'s considered pending still". This is the test that goes red if the
    /// deferral is removed altogether.
    #[test]
    fn a_reentry_pinbar_does_not_fire_on_its_own_print_bar() {
        let s = sig(SignalKind::Pinbar, SigState::Pending);
        assert_eq!(
            plain_enter_gate(&s, 4, 4, Shot::Reentry),
            PrintGate::WrongBar
        );
    }

    /// One bar later, with the pivot unrefuted, the re-entry fires.
    #[test]
    fn a_reentry_pinbar_fires_one_bar_after_its_print_bar() {
        let s = sig(SignalKind::Pinbar, SigState::Pending);
        assert_eq!(plain_enter_gate(&s, 4, 5, Shot::Reentry), PrintGate::Fire);
    }

    /// The whole point of the variant: first and re-entry disagree on the SAME
    /// signal and the SAME bar. If the two shots ever answer alike, the rule
    /// has collapsed into one of the two measured extremes.
    #[test]
    fn first_and_reentry_disagree_on_the_pinbar_print_bar() {
        let s = sig(SignalKind::Pinbar, SigState::Pending);
        assert_ne!(
            plain_enter_gate(&s, 4, 4, Shot::First),
            plain_enter_gate(&s, 4, 4, Shot::Reentry),
            "the print bar must fire a first entry and defer a re-entry"
        );
        assert_ne!(
            plain_enter_gate(&s, 4, 5, Shot::First),
            plain_enter_gate(&s, 4, 5, Shot::Reentry),
            "the pivot bar must defer a first entry and fire a re-entry"
        );
    }

    /// The re-entry'"'"'s pivot FAILED (or the pinbar was otherwise refuted inside
    /// its first bar), so the due bar declines. This is the suppression the
    /// +60.94R bucket came from.
    #[test]
    fn a_reentry_pinbar_whose_pivot_failed_does_not_fire() {
        let s = sig(SignalKind::Pinbar, SigState::Invalid);
        assert_eq!(
            plain_enter_gate(&s, 4, 5, Shot::Reentry),
            PrintGate::Refuted
        );
    }

    /// `Valid` at `N+1` is impossible with `confirm_bars >= 2`, but if a future
    /// config shortened the window it must still fire — the gate asks "not
    /// refuted", never "confirmed" (a plain enter is not confirmation-gated).
    #[test]
    fn a_reentry_pinbar_already_valid_at_the_due_bar_still_fires() {
        let s = sig(SignalKind::Pinbar, SigState::Valid);
        assert_eq!(plain_enter_gate(&s, 4, 5, Shot::Reentry), PrintGate::Fire);
    }

    /// Two bars after the print is past the due bar for either shot — the
    /// pinbar'"'"'s window is resolving and this is a confirmation, not an
    /// occurrence. Declined, the same way a plain enter has always declined a
    /// retroactive confirmation.
    #[test]
    fn a_pinbar_does_not_fire_two_bars_after_its_print_bar() {
        let s = sig(SignalKind::Pinbar, SigState::Valid);
        for shot in [Shot::First, Shot::Reentry] {
            assert_eq!(
                plain_enter_gate(&s, 4, 6, shot),
                PrintGate::WrongBar,
                "{shot:?}"
            );
        }
    }

    /// Every NON-pinbar kind is untouched, **on either shot**: it fires on its
    /// own print bar and is NOT deferred to the bar after. Table-driven over the
    /// whole enum × both shots so a newly-added kind cannot silently inherit the
    /// pinbar deferral — the match in `print_delay_bars` is exhaustive, so a new
    /// variant is a compile error there, and this test then pins the answer.
    #[test]
    fn non_pinbar_kinds_still_fire_on_their_print_bar_on_either_shot() {
        for kind in [
            SignalKind::Tweezer,
            SignalKind::DoubleTweezer,
            SignalKind::RegularEngulfer,
            SignalKind::FloatingEngulfer,
        ] {
            for shot in [Shot::First, Shot::Reentry] {
                let s = sig(kind, SigState::Pending);
                assert_eq!(
                    plain_enter_gate(&s, 4, 4, shot),
                    PrintGate::Fire,
                    "{kind:?}/{shot:?} must fire on its own print bar"
                );
                assert_eq!(
                    plain_enter_gate(&s, 4, 5, shot),
                    PrintGate::WrongBar,
                    "{kind:?}/{shot:?} must NOT be deferred to the bar after"
                );
            }
        }
    }

    /// A non-pinbar that has been refuted by its print bar is still declined on
    /// either shot — the `Invalid` check is not pinbar- or shot-scoped, and must
    /// not become so: an engulfer whose own extreme was taken out on its print
    /// bar is no entry either.
    #[test]
    fn a_refuted_non_pinbar_does_not_fire_on_its_print_bar() {
        let s = sig(SignalKind::RegularEngulfer, SigState::Invalid);
        for shot in [Shot::First, Shot::Reentry] {
            assert_eq!(
                plain_enter_gate(&s, 4, 4, shot),
                PrintGate::Refuted,
                "{shot:?}"
            );
        }
    }

    /// A refuted pinbar FIRST entry on its print bar is also declined. The
    /// `Invalid` state on the print bar cannot be a pivot failure (unresolved
    /// there) — it is a breach of the signal'"'"'s own extreme, or a golden-
    /// unprotected opposing signal. Those were always reasons not to enter, and
    /// the re-entry scoping must not have quietly re-admitted them.
    #[test]
    fn a_refuted_pinbar_first_entry_does_not_fire_on_its_print_bar() {
        let s = sig(SignalKind::Pinbar, SigState::Invalid);
        assert_eq!(plain_enter_gate(&s, 4, 4, Shot::First), PrintGate::Refuted);
    }
}
