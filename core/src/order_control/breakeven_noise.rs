//! The break-even **noise floor** — one definition, shared by the live cron and
//! the offline replay.
//!
//! # What this is, and what it is emphatically not
//!
//! It is a **tripwire for absurdity, not a tuning knob.** A legitimately-armed
//! break-even sits roughly 50% of the way to TP from current price, which is
//! many multiples of ATR — orders of magnitude clear of this line. The floor
//! should therefore **never** fire on a correctly-derived target, in either
//! half of the system. If it starts firing routinely, that is a report about a
//! bug upstream in how the target was derived; it is not an invitation to tune
//! the constant.
//!
//! The repo already holds the underlying principle: a stop dominated by
//! transaction cost is not a stop. `intent::sl_spread_floor` rejects an *entry*
//! whose SL sits within `10 ×` the live spread. A break-even amend is the same
//! act — it replaces a stop — and had no such check, which is how the incident
//! in `BUG-breakeven-arms-off-pre-fill-history.md` produced a stop **1.6% of
//! ATR** from market that filled inside the same broker batch that created it.
//!
//! # Why it lives in `core` rather than in the cron
//!
//! It began life inside `trade-control-cron::breakeven_decision`, reachable only
//! from the live worker. The offline replay's fill simulator arms break-even on
//! the very same rule (`Breakeven::close_arms` → move the stop to the fill) but
//! applied **no** floor, so a mis-derived target was refused loudly on live and
//! applied silently offline — where it books a ~0.00R scratch on the next bar's
//! noise instead of surfacing. That is exactly the shape
//! `[[strategy_changes_in_both_replayer_and_worker]]` exists to prevent, so the
//! predicate moved here and both halves now call it. Do not re-implement the
//! comparison at either call site.
//!
//! # Fail-open, deliberately
//!
//! When the ATR cannot be computed — a window shorter than [`atr_length_for`],
//! or a degenerate (non-finite, non-positive) value — the floor is **not**
//! applied. Same discipline as `sl_spread_floor_violation`, which treats a
//! degenerate spread as unjudgeable rather than fabricating a rejection. This
//! floor is defence-in-depth *behind* the post-fill window bound and the
//! fill-priced target; it must never become the thing that silently suppresses
//! a correct break-even. An unjudgeable ATR therefore means "no opinion", which
//! is [`NoiseFloor::Clear`] — never a block.
//!
//! # No fixture is evidence about this
//!
//! The whole replay-fixture corpus is expected to be **byte-identical** with
//! this floor wired in, because every fixture's break-even is correctly derived
//! and so clears the floor by orders of magnitude. A green corpus therefore
//! proves nothing at all about this code — it is the *absence* of a signal, and
//! that is the intended steady state. The evidence that this works is the unit
//! tests below and at the two call sites, which construct deliberately absurd
//! targets. If you change this module, do not reach for the corpus to validate
//! it; mutate the code and watch a unit test go red.

use crate::broker::{Candle, Granularity};
use crate::signals::{atr_length_for, atr_length_for_bar_minutes, wilder_atr};

/// How close to current price a break-even stop may land before the amend is
/// refused as noise, expressed as a fraction of the trade's own ATR.
///
/// `0.1 × ATR` is the value `BUG-breakeven-arms-off-pre-fill-history.md`
/// proposed and is deliberately permissive — see the module docs for why a
/// correct target is nowhere near it.
pub const BREAKEVEN_MIN_ATR_FRACTION: f64 = 0.1;

/// The floor's verdict on one proposed break-even stop.
///
/// A two-variant enum rather than a `bool` or an `Option`, so neither call site
/// can read "no block" and "unjudgeable" as different things by accident: both
/// are [`NoiseFloor::Clear`], because fail-open means an unjudgeable ATR is
/// *deliberately* the same answer as a comfortable clearance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoiseFloor {
    /// The stop clears the floor, **or** the ATR was unjudgeable (fail-open).
    /// The amend may proceed.
    Clear,
    /// The stop sits within [`BREAKEVEN_MIN_ATR_FRACTION`] × ATR of the latest
    /// close. The amend must be refused and the ORIGINAL stop kept.
    ///
    /// Carries the numbers so the live log line can be read without a debugger,
    /// and so the replay can say the same thing in its journal.
    Inside {
        new_stop: f64,
        reference_price: f64,
        distance: f64,
        floor: f64,
    },
}

impl NoiseFloor {
    /// `true` when the amend must be refused. Named for the decision rather
    /// than the variant so a call site reads as a rule, not a match.
    pub fn blocks(&self) -> bool {
        matches!(self, NoiseFloor::Inside { .. })
    }
}

/// Judge a proposed break-even stop against the noise floor, for a caller that
/// holds a [`Granularity`] — the live cron, whose `BreakevenSnapshot` carries
/// one.
///
/// `window` is the post-fill candle window, ascending, mid prices — the same
/// bars the arm itself was decided from. Both the ATR and the reference price
/// come from it: the reference is the **latest** close (the most recent price
/// we hold), not the arming close, which may sit far from current price.
///
/// Returns [`NoiseFloor::Clear`] when the window is empty or too short to warm
/// the ATR — see the module docs on fail-open.
pub fn judge_breakeven_stop(
    window: &[Candle],
    new_stop: f64,
    granularity: Granularity,
) -> NoiseFloor {
    judge_breakeven_stop_at_length(window, new_stop, atr_length_for(granularity))
}

/// [`judge_breakeven_stop`] for a caller that holds a bar **duration** rather
/// than a [`Granularity`] — the offline replay's fill simulator, which infers
/// its bar cadence from the candle series it was handed.
///
/// Delegates through [`atr_length_for_bar_minutes`], the same cut-off table the
/// `Granularity` form uses, so the two halves cannot disagree about how long the
/// ATR is on a given timeframe.
pub fn judge_breakeven_stop_at_bar_minutes(
    window: &[Candle],
    new_stop: f64,
    bar_minutes: i64,
) -> NoiseFloor {
    judge_breakeven_stop_at_length(window, new_stop, atr_length_for_bar_minutes(bar_minutes))
}

/// The one comparison. Both public forms funnel through here so there is a
/// single place the floor is computed and a single place the fail-open branches
/// live.
fn judge_breakeven_stop_at_length(
    window: &[Candle],
    new_stop: f64,
    atr_length: usize,
) -> NoiseFloor {
    let Some(latest) = window.last() else {
        return NoiseFloor::Clear;
    };
    let Some(atr) = wilder_atr(window, atr_length) else {
        return NoiseFloor::Clear;
    };
    if !atr.is_finite() || atr <= 0.0 {
        return NoiseFloor::Clear;
    }
    let floor = BREAKEVEN_MIN_ATR_FRACTION * atr;
    let distance = (new_stop - latest.c).abs();
    if distance < floor {
        NoiseFloor::Inside {
            new_stop,
            reference_price: latest.c,
            distance,
            floor,
        }
    } else {
        NoiseFloor::Clear
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration, Utc};

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    /// `n` H4 bars, each with a `range`-wide high-low so the ATR is a known
    /// number, all closing at `close`. 40 bars clears the H4 ATR warmup (36).
    fn bars(n: i64, close: f64, range: f64) -> Vec<Candle> {
        let t0 = ts("2026-08-10T00:00:00Z");
        (0..n)
            .map(|i| Candle {
                time: t0 + Duration::hours(4 * i),
                o: close,
                h: close + range / 2.0,
                l: close - range / 2.0,
                c: close,
            })
            .collect()
    }

    /// The floor's whole purpose: an absurd target — one sitting essentially on
    /// top of the last close — is refused, with the numbers attached.
    #[test]
    fn a_stop_on_top_of_the_last_close_is_inside_the_floor() {
        // Range 0.0020 on every bar ⇒ ATR 0.0020 ⇒ floor 0.00020.
        let window = bars(40, 0.8200, 0.0020);
        // 0.00003 away — the incident's ~1.6% of ATR.
        let verdict = judge_breakeven_stop(&window, 0.82003, Granularity::H4);
        let NoiseFloor::Inside {
            new_stop,
            reference_price,
            distance,
            floor,
        } = verdict
        else {
            panic!("expected Inside, got {verdict:?}");
        };
        assert!(verdict.blocks());
        assert!((new_stop - 0.82003).abs() < 1e-12);
        assert!((reference_price - 0.8200).abs() < 1e-12);
        assert!((distance - 0.00003).abs() < 1e-9, "distance was {distance}");
        assert!((floor - 0.0002).abs() < 1e-9, "floor was {floor}");
    }

    /// The steady state: a correctly-derived break-even sits ~50% of the way to
    /// TP, which is many multiples of ATR. It must sail through.
    #[test]
    fn a_realistically_distant_stop_clears_the_floor() {
        let window = bars(40, 0.8200, 0.0020);
        // 0.0050 away = 2.5 × ATR = 25 × the floor.
        assert_eq!(
            judge_breakeven_stop(&window, 0.8250, Granularity::H4),
            NoiseFloor::Clear
        );
    }

    /// The comparison is `distance < floor`, so the verdict must flip across the
    /// floor and only there. Asserted a hair either side rather than exactly on
    /// it: the floor is a product of f64 arithmetic and "exactly equal" is not a
    /// state a caller can construct reliably (`0.82 + 0.0002` lands 2 ULP below
    /// `0.1 × ATR`). Pinning the two sides is what an inverted comparison breaks;
    /// pinning the exact boundary would only pin float noise.
    #[test]
    fn the_verdict_flips_across_the_floor_and_only_there() {
        let window = bars(40, 0.8200, 0.0020); // floor ≈ 0.00020
        // Comfortably outside ⇒ clear.
        assert_eq!(
            judge_breakeven_stop(&window, 0.8200 + 0.00025, Granularity::H4),
            NoiseFloor::Clear
        );
        // Comfortably inside ⇒ blocks.
        assert!(judge_breakeven_stop(&window, 0.8200 + 0.00015, Granularity::H4).blocks());
        // Symmetric: the floor is a distance, so the other side of the close
        // behaves identically. An implementation that dropped the `abs()` would
        // pass the two assertions above and fail this one.
        assert_eq!(
            judge_breakeven_stop(&window, 0.8200 - 0.00025, Granularity::H4),
            NoiseFloor::Clear
        );
        assert!(judge_breakeven_stop(&window, 0.8200 - 0.00015, Granularity::H4).blocks());
    }

    /// FAIL-OPEN. A window too short to warm the ATR is unjudgeable, and
    /// unjudgeable means "no opinion" — never a block. The target here is
    /// absurdly close on purpose: if the floor were fail-*closed* it would
    /// block, so this test can only pass while the fail-open branch exists.
    #[test]
    fn an_unwarmed_window_fails_open_even_for_an_absurd_target() {
        let window = bars(5, 0.8200, 0.0020); // 5 < atr_length_for(H4) == 36
        assert_eq!(
            judge_breakeven_stop(&window, 0.82001, Granularity::H4),
            NoiseFloor::Clear
        );
    }

    /// An empty window is the same kind of unjudgeable, and must not panic.
    #[test]
    fn an_empty_window_fails_open() {
        assert_eq!(
            judge_breakeven_stop(&[], 0.8200, Granularity::H4),
            NoiseFloor::Clear
        );
    }

    /// A flat window (every bar a zero-range doji) yields ATR 0.0 — degenerate,
    /// so unjudgeable, so clear. Without the `atr <= 0.0` guard the floor would
    /// be 0.0 and `distance < 0.0` is never true anyway, but the guard states
    /// the intent rather than relying on that coincidence.
    #[test]
    fn a_degenerate_zero_atr_fails_open() {
        let window = bars(40, 0.8200, 0.0);
        assert_eq!(
            judge_breakeven_stop(&window, 0.8200, Granularity::H4),
            NoiseFloor::Clear
        );
    }

    /// The two public forms must be the SAME rule. This is the join that stops
    /// live (which holds a `Granularity`) and replay (which holds a bar
    /// duration) drifting into two different ATR lengths on the same timeframe.
    /// Checked at a target near enough to the floor that a *different* ATR
    /// length would flip the verdict, not just at an obviously-clear one.
    #[test]
    fn the_bar_minutes_form_agrees_with_the_granularity_form() {
        for (gran, mins) in [
            (Granularity::M15, 15),
            (Granularity::H1, 60),
            (Granularity::H4, 240),
            (Granularity::D1, 1440),
        ] {
            // 120 bars warms every length in the table (max 96).
            let window = bars(120, 0.8200, 0.0020);
            for stop in [0.82003, 0.8200 + 0.0002, 0.8250] {
                assert_eq!(
                    judge_breakeven_stop(&window, stop, gran),
                    judge_breakeven_stop_at_bar_minutes(&window, stop, mins),
                    "{gran:?} ({mins}m) disagreed at stop {stop}"
                );
            }
        }
    }

    /// The reference is the LATEST close, not the first or the arming one. The
    /// window walks away from its starting price so the two answers differ.
    #[test]
    fn the_reference_price_is_the_latest_close() {
        let t0 = ts("2026-08-10T00:00:00Z");
        let window: Vec<Candle> = (0..40)
            .map(|i| {
                let close = 0.8200 + 0.0001 * f64::from(i);
                Candle {
                    time: t0 + Duration::hours(4 * i64::from(i)),
                    o: close,
                    h: close + 0.0010,
                    l: close - 0.0010,
                    c: close,
                }
            })
            .collect();
        let latest = 0.8200 + 0.0001 * 39.0;
        let NoiseFloor::Inside {
            reference_price, ..
        } = judge_breakeven_stop(&window, latest, Granularity::H4)
        else {
            panic!("a stop exactly at the latest close must be inside the floor");
        };
        assert!(
            (reference_price - latest).abs() < 1e-12,
            "reference was {reference_price}, expected the last close {latest}"
        );
        // And the FIRST close is comfortably clear, proving the two differ.
        assert_eq!(
            judge_breakeven_stop(&window, 0.8200, Granularity::H4),
            NoiseFloor::Clear
        );
    }
}
