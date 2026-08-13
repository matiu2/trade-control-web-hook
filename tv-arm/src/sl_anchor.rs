//! Where the stop-loss sits: the signal candle's wick, or a drawn structural
//! level.
//!
//! # Why this exists
//!
//! The default stop is **not** structural. `TradePattern::Hs` anchors it to
//! [`PriceAnchor::SignalHigh`] (`cli/src/trade_patterns.rs`) — the latched
//! *signal candle's own wick*, plus a 0.5%-of-ATR buffer, resolved live against
//! the Pine shell at fire time. It has no connection to the head, the shoulders,
//! or the fib. That is already a **tight** stop, tighter than the structural
//! levels a discretionary trader would pick.
//!
//! The standing claim (from the strategy's original author) is that anchoring
//! the stop at the head or shoulder is a beginner's habit and a tighter stop is
//! more profitable overall. Our default is *already* the tight one, so the open
//! question is the mirror image: **is the tight default being noise-clipped by
//! the very wick it's anchored to?** A stop resting on the signal bar's extreme
//! is, definitionally, at the level the market just proved it can reach.
//!
//! Answering that needs the same setup armed with the stop in different places
//! and nothing else changed, which is what this enum plus `--sl-matrix` provide.
//!
//! # The two structural anchors are NOT interchangeable with their veto names
//!
//! ⚠ `too-high` / `too-low` do **not** name fixed roles — they swap with the
//! trade's direction (see the repo's CLAUDE.md):
//!
//! | | invalidation (a legal stop) | pcl-exhausted (**never** a stop) |
//! |---|---|---|
//! | H&S **short** | `too-high` | `too-low` |
//! | iH&S **long** | `too-low` | `too-high` |
//!
//! Only the *invalidation* line is a stop: it's the level that says the setup is
//! dead. The pcl-exhausted level is a computed fib ~80% of the way to target —
//! it sits on the **profit** side, so anchoring a stop there would place it past
//! the take-profit, and the trade would close for a "loss" the instant it went
//! right. Nothing would error: both are just prices.
//!
//! [`SlAnchor::Invalidation`] therefore resolves from
//! [`PlanGeometry::invalidation`], which is already the direction-correct
//! **drawn** line, and never from a veto name. Resolution is additionally
//! re-checked against [`crate::geometry::sl_on_protective_side`], the same guard
//! a drawn `sl` Note passes.
//!
//! # Missing geometry declines; it never falls back
//!
//! Both structural levels are `Option` on the geometry. When the requested one
//! is absent, [`resolve_sl_anchor`] returns an error and the arm is rejected.
//!
//! Silently falling back to [`SlAnchor::Signal`] would be worse than useless
//! here: in a matrix run it produces a column that is secretly a duplicate of
//! the control, so the grid reads as three anchors compared when it is really
//! two — and the conclusion drawn from it would be wrong with nothing to show
//! for it. `save_matrix::summarise` already names cells that failed to arm.

use color_eyre::eyre::{Result, eyre};
use trade_control_conventions::Direction;

use crate::plan_geometry::PlanGeometry;

/// Which level the enter's stop-loss is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SlAnchor {
    /// The latched signal candle's own wick + 0.5%·ATR, resolved live. The
    /// shipped default — a tight, non-structural stop.
    #[default]
    Signal,
    /// The drawn invalidation horizontal (`too-high` on a short, `too-low` on a
    /// long): the level that says the pattern has failed.
    Invalidation,
    /// The fib's head — the pattern's extreme (the head of the H&S). The widest
    /// of the three, and the "beginner's" stop the author warns against.
    FibTop,
}

impl SlAnchor {
    /// Short label for logs, fixture directory names, and the matrix grid.
    ///
    /// Must stay stable: the matrix builds fixture directory names from it, so
    /// renaming one silently splits a grid column in two.
    pub fn label(self) -> &'static str {
        match self {
            SlAnchor::Signal => "sl-signal",
            SlAnchor::Invalidation => "sl-invalidation",
            SlAnchor::FibTop => "sl-fib-top",
        }
    }

    /// Does this anchor bake an absolute price at arm time?
    ///
    /// [`SlAnchor::Signal`] does not — it stays a live-resolved
    /// `PriceRef::Anchored`, which is why the default path must remain
    /// byte-identical to before this enum existed.
    pub fn is_structural(self) -> bool {
        !matches!(self, SlAnchor::Signal)
    }
}

/// Resolve `anchor` to an absolute stop price from the drawn geometry.
///
/// Returns:
///
/// - `Ok(None)` for [`SlAnchor::Signal`] — no absolute price; the caller keeps
///   the live-resolved anchored stop, exactly as before this feature existed.
/// - `Ok(Some(price))` for a structural anchor whose level is drawn and sits on
///   the protective side of the neckline.
/// - `Err(..)` when the level is missing, non-finite, or on the wrong side.
///
/// `neckline` is the reference for the side check rather than the entry price
/// because the entry isn't known at arm time (it resolves against the live
/// shell), whereas the neckline is fixed setup geometry — the same reasoning
/// [`crate::geometry::sl_on_protective_side`] documents for drawn `sl` Notes.
pub fn resolve_sl_anchor(
    anchor: SlAnchor,
    geom: &PlanGeometry,
    direction: Direction,
    neckline: f64,
) -> Result<Option<f64>> {
    let price = match anchor {
        SlAnchor::Signal => return Ok(None),
        // The DRAWN invalidation line — never a veto name, which would swap
        // roles with direction and can land on the profit side (module docs).
        SlAnchor::Invalidation => geom.invalidation.ok_or_else(|| {
            eyre!(
                "--sl-anchor invalidation needs the drawn `too-high`/`too-low` horizontal, \
                 and this chart has none; draw it or pick another anchor"
            )
        })?,
        // `.0` is the head — the fib's `0`-reading, resolved via TradingView's
        // `reverse` flag rather than point order (see `PlanGeometry`).
        SlAnchor::FibTop => {
            geom.fib_head_neckline
                .ok_or_else(|| {
                    eyre!(
                        "--sl-anchor fib-top needs the take-profit fib, and this chart has \
                         none; draw it or pick another anchor"
                    )
                })?
                .0
        }
    };

    if !price.is_finite() {
        return Err(eyre!(
            "the {} level read back as a non-finite price ({price}) — the drawing is degenerate",
            anchor.label()
        ));
    }
    // Same guard a drawn `sl` Note passes. Catches the case this module's docs
    // warn about: a "stop" resolved onto the profit side of the pattern, which
    // closes the trade for a loss the moment it goes right.
    if !crate::geometry::sl_on_protective_side(price, neckline, direction) {
        let side = match direction {
            Direction::Short => "above",
            Direction::Long => "below",
        };
        return Err(eyre!(
            "--sl-anchor {} resolves to {price}, which is on the wrong side of the neckline \
             ({neckline}) for a {direction:?} — a stop must sit {side} it",
            anchor.label()
        ));
    }
    Ok(Some(price))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short setup: head above the neckline, invalidation above it too.
    fn short_geom() -> PlanGeometry {
        PlanGeometry {
            invalidation: Some(1.1050),
            fib_head_neckline: Some((1.1080, 1.1000)),
            ..Default::default()
        }
    }

    /// The mirror: head below the neckline, invalidation below it.
    fn long_geom() -> PlanGeometry {
        PlanGeometry {
            invalidation: Some(1.0950),
            fib_head_neckline: Some((1.0920, 1.1000)),
            ..Default::default()
        }
    }

    /// The default must produce NO absolute price — the shipped behaviour is a
    /// live-resolved anchored stop, and this feature must not disturb it.
    #[test]
    fn signal_anchor_yields_no_absolute_price() {
        let got = resolve_sl_anchor(SlAnchor::Signal, &short_geom(), Direction::Short, 1.1000);
        assert_eq!(got.unwrap(), None);
    }

    /// A `Default::default()` SlAnchor is `Signal`, so an operator who passes
    /// no flag gets exactly the old behaviour.
    #[test]
    fn the_default_anchor_is_the_signal_wick() {
        assert_eq!(SlAnchor::default(), SlAnchor::Signal);
        assert!(!SlAnchor::default().is_structural());
    }

    #[test]
    fn invalidation_resolves_to_the_drawn_line_for_a_short() {
        let got = resolve_sl_anchor(
            SlAnchor::Invalidation,
            &short_geom(),
            Direction::Short,
            1.1000,
        );
        assert_eq!(got.unwrap(), Some(1.1050));
    }

    #[test]
    fn fib_top_resolves_to_the_head_not_the_neckline() {
        // `.0` is the head, `.1` the neckline. Reading the wrong element would
        // put the stop ON the neckline — a zero-risk "stop".
        let got = resolve_sl_anchor(SlAnchor::FibTop, &short_geom(), Direction::Short, 1.1000);
        assert_eq!(got.unwrap(), Some(1.1080));
    }

    /// The long mirror, for both structural anchors. This is the direction that
    /// the `too-high`/`too-low` name-swap makes dangerous, so it gets its own
    /// coverage rather than riding on the short case.
    #[test]
    fn both_structural_anchors_mirror_for_a_long() {
        let g = long_geom();
        assert_eq!(
            resolve_sl_anchor(SlAnchor::Invalidation, &g, Direction::Long, 1.1000).unwrap(),
            Some(1.0950)
        );
        assert_eq!(
            resolve_sl_anchor(SlAnchor::FibTop, &g, Direction::Long, 1.1000).unwrap(),
            Some(1.0920)
        );
    }

    /// The headline hazard: a level on the PROFIT side must be rejected, not
    /// armed. Here a short whose "invalidation" sits below the neckline — the
    /// shape a stale line from an opposite-direction setup produces.
    #[test]
    fn a_level_on_the_profit_side_is_rejected() {
        let geom = PlanGeometry {
            invalidation: Some(1.0900),
            fib_head_neckline: Some((1.1080, 1.1000)),
            ..Default::default()
        };
        let err = resolve_sl_anchor(SlAnchor::Invalidation, &geom, Direction::Short, 1.1000)
            .unwrap_err()
            .to_string();
        assert!(err.contains("wrong side"), "{err}");
        assert!(err.contains("above"), "{err}");
    }

    /// A stop exactly ON the neckline is zero-risk and must not arm.
    #[test]
    fn a_level_exactly_on_the_neckline_is_rejected() {
        let geom = PlanGeometry {
            invalidation: Some(1.1000),
            ..Default::default()
        };
        assert!(
            resolve_sl_anchor(SlAnchor::Invalidation, &geom, Direction::Short, 1.1000).is_err()
        );
    }

    /// Missing geometry DECLINES. It must never fall back to `Signal` — that
    /// would give a matrix column that is secretly a duplicate of the control.
    #[test]
    fn a_missing_level_is_an_error_not_a_silent_fallback() {
        let empty = PlanGeometry::default();
        for anchor in [SlAnchor::Invalidation, SlAnchor::FibTop] {
            let got = resolve_sl_anchor(anchor, &empty, Direction::Short, 1.1000);
            assert!(
                got.is_err(),
                "{anchor:?} must decline when its level is missing, got {got:?}"
            );
        }
    }

    /// The error names the flag value and says what to draw — an operator who
    /// gets a declined cell needs to know which drawing is absent.
    #[test]
    fn the_missing_level_error_names_the_drawing() {
        let empty = PlanGeometry::default();
        let inv = resolve_sl_anchor(SlAnchor::Invalidation, &empty, Direction::Short, 1.1000)
            .unwrap_err()
            .to_string();
        assert!(inv.contains("too-high"), "{inv}");
        let fib = resolve_sl_anchor(SlAnchor::FibTop, &empty, Direction::Short, 1.1000)
            .unwrap_err()
            .to_string();
        assert!(fib.contains("fib"), "{fib}");
    }

    #[test]
    fn a_non_finite_level_is_an_error() {
        let geom = PlanGeometry {
            invalidation: Some(f64::NAN),
            ..Default::default()
        };
        assert!(
            resolve_sl_anchor(SlAnchor::Invalidation, &geom, Direction::Short, 1.1000).is_err()
        );
    }

    /// Labels are load-bearing for fixture directory names: they must be
    /// distinct and stable.
    #[test]
    fn labels_are_distinct() {
        let labels: std::collections::HashSet<&str> =
            [SlAnchor::Signal, SlAnchor::Invalidation, SlAnchor::FibTop]
                .iter()
                .map(|a| a.label())
                .collect();
        assert_eq!(labels.len(), 3);
    }

    /// Only the two drawn anchors bake an absolute price; `Signal` stays live.
    #[test]
    fn only_the_drawn_anchors_are_structural() {
        assert!(!SlAnchor::Signal.is_structural());
        assert!(SlAnchor::Invalidation.is_structural());
        assert!(SlAnchor::FibTop.is_structural());
    }
}
