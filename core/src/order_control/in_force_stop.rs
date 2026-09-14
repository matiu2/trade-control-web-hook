//! Which stop the broker actually holds, bar by bar, once break-even and the
//! spread-hour widen are both in play.
//!
//! # The two rules this module owns
//!
//! Both were decided by the operator on 2026-09-14, and both are stated here
//! **explicitly** rather than one being left implied by the other — they are
//! independent rules that happen to interact (see *Interaction*, below).
//!
//! ## Rule 1 — a widen moves the stop that is IN FORCE, and restores to it
//!
//! > *"Allow SL to be widened and reset even after it has been set to BE, but we
//! > need to make sure that it gets set back to BE."*
//!
//! A spread hour is a reason to give the **current** stop more room for the
//! duration of the turbulence — whatever that stop is. If break-even already
//! moved the stop to the entry price, the widen widens **from break-even** and
//! the restore puts it **back to break-even**. The banked scratch is given back
//! at the end of the episode, not silently discarded.
//!
//! This is a **rule** difference, not a resolution difference. The replay used
//! to take each episode's `original_stop` from `resolved.stop_loss` — the stop
//! the *order was placed with*, frozen at placement — which has no idea
//! break-even ever ran. No amount of finer bar granularity or sub-bar zoom fixes
//! reading the wrong source number: the replay would still widen from a stop the
//! broker stopped holding hours earlier, and restore to it.
//!
//! Measured on `gbp-zar-h1-2026-07-27` (strategy-v2 short, filled 22.260):
//! break-even armed 2026-07-29T01:00 moving the stop to 22.260; a transient
//! System-2 widen fired at 06:30. The replay widened from **22.350** (the
//! placement stop) and restored to 22.350 — handing back a stop ~9 pips wider
//! than the one the operator had banked.
//!
//! ## Rule 2 — break-even does NOT arm off a bar inside an active widen
//!
//! If price runs past the 50%-to-TP level *while a widen is in force*, that
//! crossing does **not** arm break-even. The reading is retaken after the widen
//! is restored: a qualifying bar after the restore arms normally.
//!
//! The rejected alternative was to remember the mid-widen crossing and apply
//! break-even at restore time.
//!
//! ### Why
//!
//! The operator's stated reason is that it is simpler. There is a stronger one,
//! and it is the one a future reader should weigh: **the bars inside a
//! spread-hour widen are the same bars the engine already treats as
//! untradeable.** `suppress_on_spread_hour_bar_seconds` suppresses entries,
//! pattern detection and level crosses on exactly these bars — the replay
//! journal calls them "rubbish candle — entry/detection/crosses suppressed".
//! Arming break-even off one would mean trusting, for the single purpose of
//! giving up a trade's remaining upside, a bar the system distrusts for every
//! other purpose. A spread spike that prints a 50%-to-TP close is much more
//! likely to be the spread than the market.
//!
//! ### Honesty about the evidence
//!
//! There is **no statistical evidence** for this choice. The operator noted
//! explicitly that we likely have no real-world examples of the situation, and
//! certainly nothing approaching significance. It is a reasoned default chosen
//! for consistency with how the rest of the system treats these bars — not a
//! measured one. If a future corpus ever contains enough mid-widen crossings to
//! test it, test it; do not treat the present rule as settled by data.
//!
//! # Interaction between the two rules
//!
//! Rule 2 makes the in-force stop **constant for the duration of any one
//! episode**: break-even is the only thing that can move the unshielded stop
//! after placement, and Rule 2 forbids it from arming while an episode is
//! active. So `original_stop` captured at an episode's start is still correct at
//! that episode's restore, and a single forward pass — decide the widen, then
//! (only if unshielded) consider arming break-even — is well-founded with no
//! circularity between the two.
//!
//! That does **not** make Rule 1 redundant, and the narrowing matters:
//!
//! - Rule 1 is what makes an episode that starts *after* break-even armed widen
//!   from 22.260 rather than 22.350. That is the GBP/ZAR case, and Rule 2 has
//!   nothing to say about it — break-even armed at 01:00, five hours before the
//!   widen existed.
//! - Rule 2 only removes the *mid-episode* arm. Without Rule 1, a break-even
//!   armed between two episodes would still be discarded by the second.
//!
//! So Rule 2 simplifies Rule 1's implementation (no re-capture mid-episode) but
//! does not subsume it. Both are stated, and both are tested independently.
//!
//! # The fixture corpus is BLIND to both rules (measured, 2026-09-14)
//!
//! Implementing both rules moved **zero** of the 2847 corpus cells, net R
//! unchanged at +1071.50. That is not evidence the change is a no-op — it was
//! checked the only way worth checking, by **mutating the source**: reverting
//! Rule 1 (widen from `resolved.stop_loss` again) *also* moves zero cells. The
//! corpus cannot distinguish the two behaviours at all.
//!
//! The reason is mechanical. Both rules change **which stop the broker holds**,
//! and a stop only changes an *outcome* when some bar reaches between the old
//! level and the new one. On the motivating trade
//! (`gbp-zar-h1-2026-07-27-strategy-v2-*`) the fix visibly corrects the journal —
//! `SL widened → 22.357 … from 22.260` and `SL restored → 22.260`, where before
//! it read `22.448 … from 22.350` / restored `22.350` — but the position exits
//! at trade expiry with no bar anywhere near either level, so the scored R is
//! identical either way.
//!
//! **So no fixture is evidence about either rule**, the same way no fixture is
//! evidence about a timing-sensitive gate or about the break-even cron. The
//! unit tests at `simulate_fill`, `widen_episodes_at_resolved`,
//! `breakeven_armed_at` and `breakeven_watch::watch_one` are the whole safety
//! net here; a green corpus adds nothing. If you change this module, do not
//! read "corpus unchanged" as confirmation.
//!
//! # Why this lives in `core`
//!
//! The live worker and the offline replay must answer "which stop is the broker
//! holding on this bar" identically, or the fixture corpus quietly contradicts
//! production (`[[strategy_changes_in_both_replayer_and_worker]]`). The replay
//! drives this walk directly. The live side enforces the same two rules at its
//! own two call sites — `blackout_apply` reads the broker's **live** stop (so it
//! gets Rule 1 for free) and `breakeven_watch` applies Rule 2 via
//! [`BreakevenArmGate`] — but the definitions of both rules live here, in one
//! place, so neither half can drift from a rule stated only in the other.

use chrono::{DateTime, Utc};

use crate::intent::{Breakeven, Direction};

/// Whether a bar may arm break-even, given whether a widen is in force on it.
///
/// A one-variant-per-reason enum rather than a bare `bool` so the reason a bar
/// was refused survives to the log line — a silently-skipped break-even arm is
/// exactly the kind of thing that took months to notice last time
/// (`BUG-breakeven-arms-off-pre-fill-history.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakevenArmGate {
    /// No widen covers this bar: it is an ordinary bar and may arm break-even.
    Armable,
    /// A spread-hour widen is in force on this bar, so it is one of the
    /// "rubbish candles" the engine suppresses entries and crosses on. Rule 2:
    /// do not arm break-even off it; take a fresh reading after the restore.
    InsideWiden,
}

impl BreakevenArmGate {
    /// May a bar under this gate arm break-even?
    pub fn armable(self) -> bool {
        matches!(self, Self::Armable)
    }
}

/// Is the bar opening at `bar` inside an active widen episode?
///
/// The half-open convention is [`crate::order_control::WidenEpisode::covers`]'s
/// and is deliberately shared: the restore bar is **not** shielded (the live
/// cron has already amended the stop back by then), so the restore bar is an
/// ordinary bar and **may** arm break-even. That is the "take a fresh reading
/// after the restore" half of Rule 2, and it is why the required test's third
/// step ("the widen ends") is separable from its second ("price ran past 50%
/// during the widen") — different bars, different answers.
pub fn breakeven_arm_gate(
    episodes: &crate::order_control::WidenEpisodes,
    bar: DateTime<Utc>,
) -> BreakevenArmGate {
    if episodes.as_slice().iter().any(|e| e.covers(bar)) {
        BreakevenArmGate::InsideWiden
    } else {
        BreakevenArmGate::Armable
    }
}

/// The unshielded stop — the stop the broker would hold on this bar if no widen
/// were in force — as it evolves across the position's life.
///
/// "Unshielded" is [`crate::order_control::WidenEpisodes::stop_on_bar`]'s word
/// for the same thing: the placement stop until break-even arms, the break-even
/// target after. It is what Rule 1 says a widen must widen **from**, and what
/// the restore must put back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InForceStop {
    /// The stop in force right now, absent any widen.
    level: f64,
    /// Has break-even armed? Once set this never clears — break-even is latched
    /// and one-way (`intent::breakeven`), so the level only ever tightens.
    armed: bool,
}

impl InForceStop {
    /// Start at the stop the order was placed with.
    pub fn placed_at(stop_loss: f64) -> Self {
        Self {
            level: stop_loss,
            armed: false,
        }
    }

    /// The stop in force absent a widen — feed this to
    /// [`crate::order_control::WidenEpisodes::stop_on_bar`] as `unshielded`, and
    /// use it as an episode's `original_stop` (Rule 1).
    pub fn level(&self) -> f64 {
        self.level
    }

    /// Has break-even already armed?
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Consider arming break-even off a bar that closed at `close_price`.
    ///
    /// `gate` is [`breakeven_arm_gate`] for this bar: a bar inside an active
    /// widen is refused outright (Rule 2) **before** the close is even tested,
    /// so a mid-widen crossing leaves no trace — there is deliberately nothing
    /// remembered to apply at restore time. That is the rejected alternative,
    /// and keeping the refusal at the top of this function is what makes it
    /// unrepresentable rather than merely unimplemented.
    ///
    /// Returns `true` when this call armed break-even (for a log line); `false`
    /// when it was already armed, refused by the gate, or the close did not
    /// reach the level.
    pub fn consider_arm(
        &mut self,
        gate: BreakevenArmGate,
        rule: Breakeven,
        direction: Direction,
        arms_at: f64,
        close_price: f64,
        entry_price: f64,
    ) -> bool {
        if !gate.armable() || self.armed {
            return false;
        }
        if !rule.close_arms(direction, arms_at, close_price) {
            return false;
        }
        self.level = rule.target_stop(entry_price);
        self.armed = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order_control::{WidenEpisode, WidenEpisodes};

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .unwrap_or_else(|e| panic!("bad test timestamp {s}: {e}"))
            .with_timezone(&Utc)
    }

    /// One episode covering 06:00→16:00, the GBP/ZAR shape.
    fn one_episode() -> WidenEpisodes {
        WidenEpisodes::new(vec![WidenEpisode {
            effective_from: t("2026-07-29T06:00:00Z"),
            restored_at: Some(t("2026-07-29T16:00:00Z")),
            widened_stop: 22.448,
        }])
    }

    // ---- Rule 2: the gate ---------------------------------------------------

    /// The operator's required sequence, at the gate level: a widen happens, a
    /// bar inside it would qualify, the widen ends, and the bar after the
    /// restore is the one that may arm.
    ///
    /// Mutation check: make `breakeven_arm_gate` always return `Armable` and the
    /// mid-widen assertion goes red; make it return `InsideWiden` whenever any
    /// episode exists and the post-restore assertion goes red.
    #[test]
    fn the_gate_refuses_mid_widen_bars_and_admits_the_restore_bar() {
        let eps = one_episode();
        assert_eq!(
            breakeven_arm_gate(&eps, t("2026-07-29T05:00:00Z")),
            BreakevenArmGate::Armable,
            "before the widen is an ordinary bar"
        );
        assert_eq!(
            breakeven_arm_gate(&eps, t("2026-07-29T06:00:00Z")),
            BreakevenArmGate::InsideWiden,
            "the widen bar itself is inside the episode"
        );
        assert_eq!(
            breakeven_arm_gate(&eps, t("2026-07-29T10:00:00Z")),
            BreakevenArmGate::InsideWiden,
            "mid-episode"
        );
        assert_eq!(
            breakeven_arm_gate(&eps, t("2026-07-29T16:00:00Z")),
            BreakevenArmGate::Armable,
            "the restore bar is NOT shielded, so it is a fresh reading again"
        );
    }

    /// No episodes at all — the overwhelmingly common case — must not gate
    /// anything. A gate that fired on an empty set would disable break-even for
    /// every trade that never saw a spread hour.
    #[test]
    fn no_episodes_means_every_bar_is_armable() {
        let none = WidenEpisodes::none();
        assert_eq!(
            breakeven_arm_gate(&none, t("2026-07-29T10:00:00Z")),
            BreakevenArmGate::Armable
        );
    }

    // ---- Rule 2: the arm itself --------------------------------------------

    /// **The operator's required test.** The exact three-step sequence:
    ///
    /// 1. a widen happens,
    /// 2. price goes more than half way to TP DURING the widen,
    /// 3. the widen ends (restore).
    ///
    /// Break-even must NOT have armed from the mid-widen bar, and MUST arm from
    /// a qualifying bar after the restore.
    ///
    /// The two qualifying closes are deliberately **different numbers** (22.150
    /// mid-widen, 22.140 after) so neither assertion can pass by reading the
    /// other's bar, and both clear the 22.179 arming level on their own.
    #[test]
    fn breakeven_does_not_arm_mid_widen_but_does_after_the_restore() {
        let eps = one_episode();
        let rule = Breakeven::at_half();
        // Short: entry 22.260, TP 22.098 ⇒ 50% level = 22.179.
        let (entry, tp) = (22.260_f64, 22.098_f64);
        let arms_at = rule.arms_at(entry, tp);
        assert!(
            (arms_at - 22.179).abs() < 1e-9,
            "fixture geometry: 50%-to-TP is 22.179, got {arms_at}"
        );

        let mut stop = InForceStop::placed_at(22.350);

        // (1)+(2) A bar INSIDE the widen closes at 22.150 — comfortably past the
        // 22.179 level for a short. Rule 2: it must not arm.
        let armed_mid = stop.consider_arm(
            breakeven_arm_gate(&eps, t("2026-07-29T10:00:00Z")),
            rule,
            Direction::Short,
            arms_at,
            22.150,
            entry,
        );
        assert!(!armed_mid, "a mid-widen bar must not arm break-even");
        assert!(!stop.is_armed(), "and must leave no armed state behind");
        assert_eq!(
            stop.level(),
            22.350,
            "the unshielded stop is still the placement stop"
        );

        // (3) The widen ends. A qualifying bar AFTER the restore — a different
        // close from the mid-widen one, so this cannot pass off that bar.
        let armed_after = stop.consider_arm(
            breakeven_arm_gate(&eps, t("2026-07-29T16:00:00Z")),
            rule,
            Direction::Short,
            arms_at,
            22.140,
            entry,
        );
        assert!(
            armed_after,
            "a qualifying bar after the restore takes a fresh reading and arms"
        );
        assert_eq!(stop.level(), entry, "break-even targets the fill exactly");
    }

    /// The mid-widen crossing must leave **nothing remembered**. This is the
    /// rejected alternative (apply the remembered crossing at restore time), and
    /// the distinguishing evidence is a post-restore bar that does NOT qualify:
    /// under the rejected design it would still arm; under Rule 2 it must not.
    ///
    /// Without this test, a "remember and apply at restore" implementation would
    /// pass `breakeven_does_not_arm_mid_widen_but_does_after_the_restore`
    /// verbatim, because that test's post-restore bar qualifies on its own.
    #[test]
    fn a_mid_widen_crossing_is_forgotten_not_deferred_to_the_restore() {
        let eps = one_episode();
        let rule = Breakeven::at_half();
        let (entry, tp) = (22.260_f64, 22.098_f64);
        let arms_at = rule.arms_at(entry, tp);
        let mut stop = InForceStop::placed_at(22.350);

        // Deep mid-widen crossing.
        stop.consider_arm(
            breakeven_arm_gate(&eps, t("2026-07-29T10:00:00Z")),
            rule,
            Direction::Short,
            arms_at,
            22.120,
            entry,
        );
        // Post-restore bar that does NOT qualify (22.240 > 22.179 for a short).
        let armed = stop.consider_arm(
            breakeven_arm_gate(&eps, t("2026-07-29T16:00:00Z")),
            rule,
            Direction::Short,
            arms_at,
            22.240,
            entry,
        );
        assert!(
            !armed && !stop.is_armed(),
            "the mid-widen crossing must be forgotten, not banked for the restore"
        );
        assert_eq!(stop.level(), 22.350);
    }

    /// Long mirror, so the gate is not accidentally short-only. (A
    /// `Direction::Long` hardcode is a failure mode this codebase has actually
    /// shipped.)
    #[test]
    fn the_rule_is_direction_agnostic() {
        let eps = one_episode();
        let rule = Breakeven::at_half();
        // Long: entry 1.1000, TP 1.1200 ⇒ 50% level = 1.1100.
        let (entry, tp) = (1.1000_f64, 1.1200_f64);
        let arms_at = rule.arms_at(entry, tp);
        let mut stop = InForceStop::placed_at(1.0900);

        assert!(!stop.consider_arm(
            breakeven_arm_gate(&eps, t("2026-07-29T10:00:00Z")),
            rule,
            Direction::Long,
            arms_at,
            1.1150,
            entry,
        ));
        assert!(stop.consider_arm(
            breakeven_arm_gate(&eps, t("2026-07-29T16:00:00Z")),
            rule,
            Direction::Long,
            arms_at,
            1.1120,
            entry,
        ));
        assert_eq!(stop.level(), entry);
    }

    // ---- Rule 1: the in-force stop is what a widen widens from --------------

    /// Break-even arming changes what a LATER episode must widen from. This is
    /// the GBP/ZAR case in miniature and is the assertion the replay's
    /// `original_stop = resolved.stop_loss` cannot satisfy.
    #[test]
    fn a_later_episode_widens_from_break_even_not_the_placement_stop() {
        let rule = Breakeven::at_half();
        let (entry, tp) = (22.260_f64, 22.098_f64);
        let arms_at = rule.arms_at(entry, tp);
        let mut stop = InForceStop::placed_at(22.350);

        // Break-even arms at 01:00, five hours before any widen exists — so the
        // arm is gated by an EMPTY episode set, exactly as the replay sees it
        // when it walks bars in order.
        let armed = stop.consider_arm(
            breakeven_arm_gate(&WidenEpisodes::none(), t("2026-07-29T01:00:00Z")),
            rule,
            Direction::Short,
            arms_at,
            22.159,
            entry,
        );
        assert!(armed, "the 01:00 bar closed past 50%-to-TP");
        assert_eq!(
            stop.level(),
            22.260,
            "Rule 1: the stop IN FORCE from here is break-even, not 22.350 — this \
             is the number a 06:30 widen must widen from, and restore to"
        );
    }

    /// Break-even is latched: a second qualifying bar does not re-arm, and a
    /// retrace does not un-arm.
    #[test]
    fn break_even_is_latched_once_armed() {
        let rule = Breakeven::at_half();
        let (entry, tp) = (22.260_f64, 22.098_f64);
        let arms_at = rule.arms_at(entry, tp);
        let mut stop = InForceStop::placed_at(22.350);
        let none = WidenEpisodes::none();

        assert!(stop.consider_arm(
            breakeven_arm_gate(&none, t("2026-07-29T01:00:00Z")),
            rule,
            Direction::Short,
            arms_at,
            22.159,
            entry,
        ));
        // A second qualifying bar: already armed, so no second arm reported.
        assert!(!stop.consider_arm(
            breakeven_arm_gate(&none, t("2026-07-29T02:00:00Z")),
            rule,
            Direction::Short,
            arms_at,
            22.120,
            entry,
        ));
        // A retrace far above the level does not move the stop back.
        assert!(!stop.consider_arm(
            breakeven_arm_gate(&none, t("2026-07-29T03:00:00Z")),
            rule,
            Direction::Short,
            arms_at,
            22.400,
            entry,
        ));
        assert_eq!(stop.level(), entry, "latched — the stop never widens back");
    }
}
