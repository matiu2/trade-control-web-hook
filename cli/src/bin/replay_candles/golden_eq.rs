//! Tolerant equality for the golden fixture snapshot.
//!
//! `ReplayOutcome` and its economics derive `PartialEq`, which on `f64` is **bit
//! equality**. That is the wrong predicate for a corpus whose numbers arrive by
//! two different arithmetic routes:
//!
//! - the **capture** path writes `expected.json` from the run that pulled the
//!   candles (`--save`),
//! - the **check** path recomputes from the frozen candles (`--check`, and the
//!   `all_fixtures_match_expected` test).
//!
//! Those agree to every digit anyone cares about and can still disagree in the
//! last one or two bits. Observed on the EUR/USD 2026-07-22 corpus: a leg's
//! `stop_loss` read `1.1402000000000003` on one path and `1.1402000000000005` on
//! the other — **2 ULP**, ~2e-16 relative, with `net_r` and every other field
//! byte-identical. Four of six cells failed `--check` on that alone.
//!
//! This is the fix [`super::economics::ReplayEconomics::account`]'s doc comment
//! already prescribed ("*the fix is a tolerance-based comparison for
//! `ReplayOutcome`, not another derived field*"). `account` was deleted from the
//! snapshot for the same class of flake — it was the longest dependent chain, so
//! it broke first, but removing it only narrowed the surface. `net_r` (a plain
//! left fold, and float addition is not associative) and `legs[].r` carry the
//! same exposure.
//!
//! ## Why tolerance and not rounding-on-write
//!
//! Rounding stored prices to 6dp would also make these four cells pass, and for
//! FX and indices 6dp is below any real tick. It is rejected because it **edits
//! the recorded measurement** to fix a comparison bug — the same objection that
//! keeps an implausible R un-clamped and a non-finite price loud (see
//! `economics.rs`, and `[[no_silent_degrade_prefer_loud_failure]]`). It would
//! also silently truncate any future instrument quoted finer than 1e-6, and it
//! makes no sense applied to `r`, which is a dimensionless ratio, not a price.
//! Comparing loosely leaves the data exactly as measured and puts the
//! looseness in the one place that was actually wrong: the equality test.
//!
//! ## The tolerance is RELATIVE
//!
//! `1e-9` of the larger magnitude, floored so exact zeros compare equal. It must
//! be relative for the reason `economics::realized_r`'s zero-risk floor is
//! relative: `1e-6` absolute is far below an FX pip (1e-5) but *far above* the
//! ULP at index scale — UK100 at ~10500 has adjacent doubles ~1.8e-12 apart, so
//! a fixed absolute epsilon is simultaneously too loose for FX and arbitrary for
//! indices. At 1e-9 relative, a 1.1402 stop tolerates ~1.1e-9 (2 ULP is 4e-16,
//! so ~6 orders of margin) while still catching any difference a tick could
//! produce — the finest real quote is a 1e-6 fractional-pip, four orders
//! *above* the tolerance. A genuine one-tick regression cannot hide in here.

use super::economics::{Leg, ReplayEconomics};
use super::fixture::ReplayOutcome;

/// Relative tolerance for a stored float: `1e-9` of the larger magnitude.
///
/// Sized to sit far above float noise (2 ULP ≈ 4e-16 relative) and far below the
/// smallest meaningful price move (a 1e-6 fractional-pip tick, i.e. ~1e-6
/// relative at FX scale). Anything a real behaviour change moves is orders
/// bigger; anything codegen moves is orders smaller.
const REL_TOL: f64 = 1e-9;

/// Are two stored floats equal for golden-comparison purposes?
///
/// Relative to the larger magnitude, so it holds at FX (1.14) and index (10500)
/// scale alike. `NaN` is never equal to anything — a `NaN` in a golden is a bug
/// upstream (`economics::finite_or_zero` exists to prevent it) and must not be
/// smoothed over here.
fn close(a: f64, b: f64) -> bool {
    if a == b {
        // Covers exact equality including ±0.0 and equal infinities, and is the
        // fast path for the overwhelmingly common case.
        return true;
    }
    if !a.is_finite() || !b.is_finite() {
        // Differing non-finites (NaN vs anything, +inf vs 1.0) are a real
        // mismatch — no tolerance applies.
        return false;
    }
    (a - b).abs() <= REL_TOL * a.abs().max(b.abs())
}

/// Compare two `Option<f64>` — `Some`/`None` must agree, values within tolerance.
fn close_opt(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => close(x, y),
        _ => false,
    }
}

/// Tolerant equality for one booked leg: every non-float field must match
/// **exactly** (times, exit reason, and the presence-or-absence of an exit); only
/// the prices and the R multiple get the tolerance.
fn legs_match(a: &Leg, b: &Leg) -> bool {
    a.entry_time == b.entry_time
        && a.exit_time == b.exit_time
        && a.exit_reason == b.exit_reason
        && close(a.entry_price, b.entry_price)
        && close(a.stop_loss, b.stop_loss)
        && close(a.take_profit, b.take_profit)
        && close_opt(a.exit_price, b.exit_price)
        && close(a.r, b.r)
}

/// Tolerant equality for a run's economics. Counters are integers and compare
/// exactly — a changed `sl_hits` is a behaviour change, never noise. Leg order is
/// significant (fire order, which is the order the account compounds in).
fn economics_match(a: &ReplayEconomics, b: &ReplayEconomics) -> bool {
    a.tp_hits == b.tp_hits
        && a.sl_hits == b.sl_hits
        && a.reversal_closes == b.reversal_closes
        && a.expiry_closes == b.expiry_closes
        && a.invalidation_closes == b.invalidation_closes
        && a.open_at_end == b.open_at_end
        && close(a.net_r, b.net_r)
        && a.legs.len() == b.legs.len()
        && a.legs.iter().zip(&b.legs).all(|(x, y)| legs_match(x, y))
}

/// Does a recomputed replay outcome match its golden `expected.json`?
///
/// **The only equality the fixture gate should use** — prefer this over `==` on
/// [`ReplayOutcome`], which is bit-exact on floats and therefore flaky across
/// the capture and check paths (see the module doc).
///
/// Everything structural is exact: which rules fired, in what order, on which
/// bar, with what action and blackout suppression; the terminal flag and phase;
/// the warnings. Only the *measured* floats are compared with tolerance, and
/// `Some`/`None` on the economics must still agree — a fixture that lost its
/// economics entirely is a real regression (that's the gate `--rebless
/// --simulate false` refuses to remove).
pub fn outcome_matches(expected: &ReplayOutcome, got: &ReplayOutcome) -> bool {
    if expected.done != got.done
        || expected.final_phase != got.final_phase
        || expected.warnings != got.warnings
        || expected.fires.len() != got.fires.len()
    {
        return false;
    }
    // A fire carries one float — the triggering candle's close. It comes off a
    // frozen candle either way (no arithmetic), so it should be bit-identical;
    // compare it tolerantly anyway for consistency, since nothing is gained by
    // being strict on one float and loose on the rest.
    let fires_ok = expected.fires.iter().zip(&got.fires).all(|(e, g)| {
        e.rule_id == g.rule_id
            && e.action == g.action
            && e.candle_time == g.candle_time
            && e.suppressed_by == g.suppressed_by
            && close(e.candle_close, g.candle_close)
    });
    if !fires_ok {
        return false;
    }
    match (&expected.outcome, &got.outcome) {
        (None, None) => true,
        (Some(e), Some(g)) => economics_match(e, g),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::economics::ExitReason;
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 22, hour, 0, 0)
            .single()
            .unwrap_or_default()
    }

    fn leg() -> Leg {
        Leg {
            entry_time: at(0),
            entry_price: 1.14176,
            stop_loss: 1.1402000000000003,
            take_profit: 1.14342,
            exit_time: Some(at(3)),
            exit_price: Some(1.14342),
            exit_reason: ExitReason::TookProfit,
            r: 1.0641025641028596,
        }
    }

    /// THE bug this module exists for: the real EUR/USD divergence — a
    /// `stop_loss` differing by 2 ULP, everything else identical — must compare
    /// equal. These are the literal values from
    /// `replay-fixtures/eur-usd-h1-2026-07-22-skip-bcr-news-off`.
    #[test]
    fn two_ulp_stop_loss_difference_is_equal() {
        let a = leg();
        let mut b = leg();
        b.stop_loss = 1.1402000000000005;
        assert_ne!(a.stop_loss, b.stop_loss, "the inputs must actually differ");
        assert!(legs_match(&a, &b));
    }

    /// A one-tick move is a REAL change and must fail. The finest quote in the
    /// catalog is a 1e-6 fractional pip; at FX scale that is ~1e-6 relative,
    /// three orders above the tolerance.
    #[test]
    fn one_fractional_pip_difference_is_not_equal() {
        let a = leg();
        let mut b = leg();
        b.stop_loss += 0.000001;
        assert!(!legs_match(&a, &b));
    }

    /// The tolerance must hold at index scale too, where the ULP is ~1.8e-12 —
    /// a fixed 1e-6 absolute epsilon would be arbitrary here, and bit equality
    /// would flake.
    #[test]
    fn index_scale_ulp_difference_is_equal_but_one_point_is_not() {
        let entry = 10_500.0_f64;
        let nudged = f64::from_bits(entry.to_bits() + 2);
        assert!(close(entry, nudged), "2 ULP at index scale must be equal");
        assert!(
            !close(entry, entry + 1.0),
            "a whole index point must not be equal"
        );
    }

    /// Non-finites are never smoothed over: a `NaN` in a golden means an upstream
    /// guard failed, and equality must not hide it.
    #[test]
    fn non_finite_never_compares_equal() {
        assert!(!close(f64::NAN, f64::NAN));
        assert!(!close(f64::NAN, 1.0));
        assert!(!close(f64::INFINITY, 1.0));
        // Equal infinities do compare equal, via the `a == b` fast path.
        assert!(close(f64::INFINITY, f64::INFINITY));
    }

    /// Exact zero compares equal to itself (the relative tolerance would be zero
    /// there, so this depends on the `a == b` fast path) but not to a real value.
    #[test]
    fn zero_compares_equal_to_zero_only() {
        assert!(close(0.0, 0.0));
        assert!(close(0.0, -0.0));
        assert!(!close(0.0, 1e-6));
    }

    /// Counters are behaviour, not noise — a differing `sl_hits` must fail even
    /// though every float matches.
    #[test]
    fn counter_difference_is_not_equal() {
        let a = ReplayEconomics {
            sl_hits: 1,
            ..Default::default()
        };
        let b = ReplayEconomics {
            sl_hits: 2,
            ..Default::default()
        };
        assert!(!economics_match(&a, &b));
    }

    /// A leg's non-float fields are structural and compare exactly: the same
    /// prices exited for a different REASON is a real divergence.
    #[test]
    fn exit_reason_difference_is_not_equal() {
        let a = leg();
        let mut b = leg();
        b.exit_reason = ExitReason::StoppedOut;
        assert!(!legs_match(&a, &b));
    }

    /// Losing the exit entirely (still-open vs closed) must fail, even though
    /// `close_opt`'s tolerance would otherwise not be consulted.
    #[test]
    fn missing_exit_price_is_not_equal() {
        let a = leg();
        let mut b = leg();
        b.exit_price = None;
        assert!(!legs_match(&a, &b));
        assert!(!close_opt(Some(1.0), None));
        assert!(close_opt(None, None));
    }

    /// A fixture that lost its economics is a real regression, not a rounding
    /// artefact — `Some` vs `None` must never compare equal.
    #[test]
    fn economics_presence_must_agree() {
        let with = ReplayOutcome {
            fires: Vec::new(),
            done: true,
            final_phase: trade_control_engine::Phase::Done,
            warnings: Vec::new(),
            outcome: Some(ReplayEconomics::default()),
        };
        let without = ReplayOutcome {
            outcome: None,
            ..with.clone()
        };
        assert!(!outcome_matches(&with, &without));
        assert!(outcome_matches(&with, &with.clone()));
    }

    /// A differing net R beyond tolerance must fail — this is the number the
    /// corpus exists to gate.
    #[test]
    fn net_r_regression_is_not_equal() {
        let a = ReplayEconomics {
            net_r: 1.06,
            ..Default::default()
        };
        let b = ReplayEconomics {
            net_r: 0.53,
            ..Default::default()
        };
        assert!(!economics_match(&a, &b));
        let c = ReplayEconomics {
            net_r: 1.06 + 1e-15,
            ..Default::default()
        };
        assert!(economics_match(&a, &c));
    }
}
