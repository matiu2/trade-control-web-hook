//! Resolving an **H&S / iH&S** setup into a signed trade spec, and the
//! validation gates that guard it.
//!
//! Extracted from `pipeline.rs` unchanged. Everything here reads
//! [`PlanGeometry`] and never `Roles`, which is the property `--spec-in` needs:
//! no chart drawing is touched below this line.
//!
//! ## Two wrong-direction bugs shaped this
//!
//! Direction comes from the **fib** — specifically its head↔neckline pair, where
//! the head is the `0`-reading resolved via TradingView's `reverse` flag. Not
//! from the pattern label, and *not* from raw point order:
//!
//! - reading it from the **invalidation line** picked up a stale `too-high` left
//!   over from a different trade and armed the wrong way. That line is now
//!   validated to sit inside the fib's range (which catches the stale leftover)
//!   but no longer decides anything.
//! - reading it from **raw point order** was a second, independent
//!   wrong-direction bug (AUD/CAD 2026-07, head at `points[1]`).
//!
//! ## `close_on_news` arrives separately, on purpose
//!
//! News windows are calendar-derived rather than drawn, so the news fact is
//! passed in as a parameter instead of being read from the geometry. A frozen
//! spec supplies geometry and **re-reads** the calendar (see `frozen_setup`), so
//! the two must stay apart or a spec-in arm would carry a stale news decision.

use chrono::{DateTime, Utc};
use color_eyre::eyre::eyre;
use tracing::warn;
use trade_control_cli as cli;
use trade_control_conventions::{Broker, Direction};

use crate::args::Args;
use crate::broker_kind::broker_to_kind;
use crate::calendar::read_trade_expiry;
use crate::geometry::pcl_exhausted_price;
use crate::plan_geometry::PlanGeometry;
use crate::resolve_error::ResolveError;

#[allow(clippy::too_many_arguments)]
pub fn resolve_hs_trade(
    args: &Args,
    geom: &PlanGeometry,
    close_on_news: bool,
    instrument: &str,
    account: &str,
    broker: Broker,
    catalog_pip: f64,
    catalog_tick: f64,
) -> std::result::Result<(Direction, cli::TradeSpec), ResolveError> {
    if let Err(msg) = check_required(geom, args) {
        return Err(ResolveError::Reject(msg));
    }
    // A future prep-expiry cutoff with no matching prep drawing would
    // arm a setup that can never enter; a past cutoff is just a re-arm.
    if let Err(msg) = check_prep_expiries(geom, Utc::now()) {
        return Err(ResolveError::Reject(msg));
    }
    // Rule 1: the FIB gives us the trade direction. Resolve which end is the
    // head (the fib's `0`-reading) via TradingView's `reverse` flag — NOT the
    // raw point order, which is unreliable (AUD/CAD 2026-07 had its head at
    // `points[1]`, so point-order armed a long instead of the correct short).
    // Head above neckline → short (H&S); below → long (iH&S). We no longer read
    // direction off the `too-high`/`too-low` invalidation label either, which
    // could be a stale line from a *different* trade and silently flip it.
    let (head, neckline) = geom.fib_head_neckline.ok_or_else(|| {
        eyre!("cannot read the fib's two anchors (need two finite prices to set direction)")
    })?;
    let direction =
        crate::geometry::direction_from_head_neckline(head, neckline).ok_or_else(|| {
            eyre!(
                "cannot read trade direction from the fib — its head and neckline are equal \
                 (draw the fib spanning head→neckline)"
            )
        })?;
    // Rule 2: the invalidation (`too-low`/`too-high`) horizontal must sit
    // inside the fib's head↔neckline range. A line outside that band belongs
    // to a different, larger pattern — reject it rather than bake a poison
    // level / mismatched setup.
    if let Some(inv_price) = geom.invalidation
        && !crate::geometry::price_within_fib_range(inv_price, head, neckline)
    {
        let (lo, hi) = (head.min(neckline), head.max(neckline));
        return Err(ResolveError::Reject(format!(
            "invalidation line at {inv_price} is outside the fib range [{lo}, {hi}] — it \
             looks like a stale `too-high`/`too-low` from a different trade; redraw or \
             remove it"
        )));
    }
    let tp = crate::geometry::tp_price(head, neckline);
    // Continuous at-entry level vetos (Bug #12): the pcl-exhausted (`too-low`)
    // and invalidation (`too-high`) levels, baked onto the enter so the worker
    // rejects an entry already past either — independent of the cross-guard.
    let entry_level_vetos = hs_entry_level_vetos(geom, direction);
    let expiry = read_trade_expiry(geom)?;
    // --pip-size / --tick-size override the canonical catalog values when set.
    let pip_size = args.pip_size.unwrap_or(catalog_pip);
    let tick_size = args.tick_size.unwrap_or(catalog_tick);
    let spec = build_trade_spec(
        args,
        instrument,
        account,
        broker,
        direction,
        expiry,
        tp,
        geom,
        close_on_news,
        pip_size,
        tick_size,
        entry_level_vetos,
    );
    Ok((direction, spec))
}

/// Validate the chart has every drawing the bundle will need.
/// Mirrors `tv_arm_hs.py:1614-1629`.
pub fn check_required(geom: &PlanGeometry, args: &Args) -> std::result::Result<(), String> {
    let mut missing = Vec::new();
    if geom.invalidation.is_none() {
        missing.push("horizontal_line labeled 'too-high' or 'too-low'");
    }
    if geom.neckline.is_none() && !args.skip_break_and_close {
        missing.push("trend_line labeled 'neckline' (or 'break-and-close')");
    }
    // The retest reuses the neckline drawing (`resolve_retest` assigns
    // `roles.retest = break_and_close.clone()`), so `geom.neckline` covers both
    // roles — it is only *independently* missing when the retest is wanted but the
    // neckline is skipped, leaving nothing to derive it from.
    if geom.neckline.is_none() && !args.skip_retest && args.skip_break_and_close {
        missing.push("trend_line labeled 'neckline' (needed for the retest)");
    }
    if geom.fib_head_neckline.is_none() {
        missing.push("fib_retracement (TP)");
    }
    if geom.trade_expiry_epoch.is_none() {
        missing.push("vertical_line labeled 'trade-expiry'");
    }
    if missing.is_empty() {
        return Ok(());
    }
    let mut msg = String::from("missing required drawings:\n");
    for m in missing {
        msg.push_str("  - ");
        msg.push_str(m);
        msg.push('\n');
    }
    Err(msg)
}

/// Canonical prep-step names that have a `<prep>-expiry` cutoff line on
/// the chart — fed into `cli::TradeSpec.prep_expiries` so the CLI emits
/// one `08-prep-expire-<step>` alert per line.
///
/// A step that is *also* being skipped (`skip_preps`, e.g. `--quasimodo`
/// drops `break-and-close`) is filtered out: there is no prep left to
/// expire, so a stale drawn `<step>-expiry` line on the chart is just
/// context, not a cutoff to arm. Emitting it anyway would put the same
/// name in both `skip_preps` and `prep_expiries`, which the CLI validator
/// rejects (`can't expire a prep that's been dropped`).
pub fn prep_expiry_steps(geom: &PlanGeometry, skip_preps: &[String]) -> Vec<String> {
    geom.prep_expiry_epochs
        .iter()
        .map(|(step, _)| step.clone())
        .filter(|step| !skip_preps.iter().any(|s| s == step))
        .collect()
}

/// Validate each `<prep>-expiry` cutoff line against the prep it guards.
///
/// - **Future cutoff, no matching prep drawing** → hard error. The line
///   would block the prep before it could ever land, so the setup could
///   never enter — almost certainly the operator drew the cutoff but
///   forgot the neckline / retest trend line.
/// - **Past cutoff** → warn only. We're re-arming a setup later in time
///   (the cutoff already lapsed); the line is harmless context, not a
///   reason to abort.
///
/// `now` is injected so the rule is unit-testable without a clock.
pub fn check_prep_expiries(
    geom: &PlanGeometry,
    now: DateTime<Utc>,
) -> std::result::Result<(), String> {
    let now_unix = now.timestamp();
    let mut errors = Vec::new();
    // The epochs come from `PlanGeometry` (`points.first().time`) rather than
    // `Drawing::anchor_time_seconds` (`min` over all points). Identical here:
    // prep-expiry roles are VERTICAL lines, which carry exactly one anchor.
    for (step, line_unix) in &geom.prep_expiry_epochs {
        let line_unix = *line_unix;
        let prep_present = match step.as_str() {
            // Both prep roles are served by the one neckline (see `check_required`).
            trade_control_conventions::PREP_BREAK_AND_CLOSE
            | trade_control_conventions::PREP_RETEST => geom.neckline.is_some(),
            // Unknown step shouldn't occur (classify only emits known
            // prep names), but treat it as "prep absent" defensively.
            _ => false,
        };
        if line_unix > now_unix {
            if !prep_present {
                errors.push(format!(
                    "  - '{step}-expiry' cutoff line is in the future but no '{step}' \
                     trend line is on the chart — this setup could never enter \
                     (draw the '{step}' line, or remove the expiry cutoff)"
                ));
            }
        } else {
            warn!(
                step = %step,
                "'{step}-expiry' cutoff line is in the past — assuming a re-arm later in time"
            );
        }
    }
    if errors.is_empty() {
        return Ok(());
    }
    Err(format!(
        "prep-expiry validation failed:\n{}\n",
        errors.join("\n")
    ))
}

#[allow(clippy::too_many_arguments)]
pub fn build_trade_spec(
    args: &Args,
    instrument: &str,
    account: &str,
    broker: Broker,
    direction: Direction,
    expiry: DateTime<Utc>,
    tp: f64,
    geom: &PlanGeometry,
    close_on_news: bool,
    pip_size: f64,
    tick_size: f64,
    entry_level_vetos: Vec<trade_control_core::intent::EntryLevelVeto>,
) -> cli::TradeSpec {
    use cli::TradePattern;
    let pattern = match direction {
        Direction::Short => TradePattern::Hs,
        Direction::Long => TradePattern::Ihs,
    };
    let mut skip_preps = Vec::new();
    if args.skip_break_and_close {
        skip_preps.push("break-and-close".to_string());
    }
    if args.skip_retest {
        skip_preps.push("retest".to_string());
    }
    // Borrow `skip_preps` before it's moved into the struct literal below.
    let prep_expiries = prep_expiry_steps(geom, &skip_preps);
    let mut spec =
        cli::TradeSpec {
            pattern,
            instrument: instrument.to_string(),
            account: account.to_string(),
            broker: broker_to_kind(broker),
            trade_expiry: expiry,
            risk_pct: args.risk_pct.unwrap_or(1.0),
            risk_amount: args.risk_amount,
            dry_run: args.broker_dry_run,
            // strategy-v2 needs a non-zero max_retries on both enters: it's the
            // multi_shot flag that keeps the engine plan alive after the first
            // enter fires, so the worker retry gate can cancel the sibling's
            // resting order. Floor to 1 (a `--max-retries 0` with `--strategy-v2`
            // is rejected by validate_args, so this floor is just belt-and-braces).
            max_retries: if args.strategy_v2 {
                args.max_retries.unwrap_or(5).max(1)
            } else {
                args.max_retries.unwrap_or(5)
            },
            expiry_bars: args.expiry_bars,
            skip_preps,
            pull_back: args.pull_back,
            entry_offset_pips: None,
            sl_offset_pips: None,
            // Both offset forms None → the shared builder applies the ATR-pct
            // default (DEFAULT_BUFFER_ATR_PCT). Unused on the M/W path (worker
            // computes geometry); the H&S enter inherits the volatility-scaled buffer.
            entry_offset_atr_pct: None,
            sl_offset_atr_pct: None,
            sl_anchor: None,
            tp_price: round5(tp),
            // H&S anchors SL to the pattern extreme, not an absolute price.
            sl_price: None,
            entry_deadline_pct: 80,
            allow_entry: args.entry_filter_script.clone(),
            // Pattern-path entry order type: explicit `--entry-{market,stop,limit}`
            // wins; default is stop.
            entry_mode: match args.pattern_entry_mode() {
                Some(crate::args::PatternEntry::Market) => cli::EntryMode::Market,
                Some(crate::args::PatternEntry::Limit) => cli::EntryMode::Limit,
                Some(crate::args::PatternEntry::Stop) | None => cli::EntryMode::Stop,
            },
            needs_golden: !args.skip_golden,
            needs_confirmed: args.require_confirmation,
            close_on_news,
            // Chart-drawn S/R bands, plus (default-on) a one-sided band pinned
            // to the take-profit so a reversal near TP flattens for a partial win
            // rather than round-tripping to the stop. `07-close-on-sr-reversal`
            // OR-fires across every band. H&S only — see `tp_resistance_band`.
            sr_reversal_ranges: {
                let mut bands = build_sr_ranges(geom, args.reversal_band_pct);
                if !args.skip_tp_resistance {
                    bands.push(tp_resistance_band(tp, direction, args.tp_resistance_pct));
                }
                bands
            },
            veto_on_reversal: args.veto_on_reversal,
            needs_confirmed_close: false,
            // Populated from the chart's `<prep>-expiry` vertical lines —
            // see `prep_expiry_steps`. Skipped preps (e.g. `--quasimodo`
            // drops break-and-close) are filtered out so a stale expiry line
            // doesn't collide with `skip_preps`.
            prep_expiries,
            // H&S path: no M/W static geometry. The M/W branch (commit 9)
            // builds its spec separately, keyed on `roles.mw_path`.
            mw: None,
            // Baked from instrument-lookup (or --pip-size) so the worker scales
            // the entry/SL offset_pips with the right pip, not its forex default.
            pip_size: Some(pip_size),
            // Baked from instrument-lookup (or --tick-size) so the worker snaps
            // entry/SL/TP onto the broker's price grid before placement.
            tick_size: Some(tick_size),
            blackout_close: args.blackout_close.into_core(),
            entry_level_vetos,
            // Wrong-side recovery (H&S / iH&S). Explicit `--recover-entry` wins.
            // Otherwise the default depends on the entry mode:
            //  - `--entry-limit`: a wrong-side limit recovers to a **stop** at the
            //    same level (the operator's rule; `limit_recover_action`).
            //  - stop entry + `--require-confirmation`: defaults to `limit` (the
            //    confirmation lag is what strands the stop).
            //  - everything else: today's drop (`skip`).
            recover_entry: match args.pattern_entry_mode() {
                Some(crate::args::PatternEntry::Limit) => args.limit_recover_action(),
                _ => args.recover_entry.map(|r| r.into_core()).unwrap_or(
                    if args.require_confirmation {
                        trade_control_core::intent::RecoverEntryAction::Limit
                    } else {
                        trade_control_core::intent::RecoverEntryAction::Skip
                    },
                ),
            },
            strategy_v2: args.strategy_v2,
            // QM leg (`09-enter-qm`) entry order type — `--qm-entry`, default
            // Limit (rest at the signal level, recover to a stop when price has
            // already crossed it). Independent of the BCR leg's `entry_mode`.
            qm_entry_mode: match args.qm_entry {
                Some(crate::args::QmEntry::Market) => cli::EntryMode::Market,
                Some(crate::args::QmEntry::Stop) => cli::EntryMode::Stop,
                Some(crate::args::QmEntry::Limit) | None => cli::EntryMode::Limit,
            },
            // Break-even on at 50% by default; `--no-breakeven` opts out,
            // `--breakeven-pct` overrides the threshold.
            breakeven_pct: if args.no_breakeven {
                None
            } else {
                Some(args.breakeven_pct.unwrap_or(0.5))
            },
            // Entry SL-spread floor window baked onto the enter; `None` → worker default (5).
            spread_window: args.spread_window,
        };
    if args.sl_from_recent {
        spec.sl_anchor = Some(match direction {
            Direction::Short => cli::PriceAnchor::RecentHigh,
            Direction::Long => cli::PriceAnchor::RecentLow,
        });
    }
    spec
}

/// The continuous at-entry level vetos for an H&S/IH&S setup (Bug #12).
///
/// Two levels, mirroring the `intent.vetos` name-list the enter already
/// carries:
/// - **pcl-exhausted** — `midpoint + 0.8 × (TP − midpoint)` from the fib;
///   the move is mostly done, a late entry's R:R no longer justifies opening.
///   For a short the entry is "past" when **at or below** it (`Below`); a long
///   mirrors (`Above`). Named `too-low` (short) / `too-high` (long).
/// - **invalidation** — the operator's horizontal at the right shoulder; the
///   thesis is dead once price runs back past it. For a short "past" is **at
///   or above** (`Above`); a long mirrors (`Below`). Named `too-high` (short)
///   / `too-low` (long).
///
/// A level that comes back `NaN` (drawing absent or malformed) is skipped so a
/// missing fib / invalidation can't bake a poison level. Direction picks both
/// the name and the side.
pub fn hs_entry_level_vetos(
    geom: &PlanGeometry,
    direction: Direction,
) -> Vec<trade_control_core::intent::EntryLevelVeto> {
    use trade_control_core::intent::{EntryLevelVeto, VetoSide};
    let mut out = Vec::new();

    // pcl-exhausted (the "ran most of the way to TP" gate). Resolve head/
    // neckline via the fib's `reverse` flag (not point order) so the level
    // lands on the correct side even when the operator drew it neckline-first.
    if let Some((head, neckline)) = geom.fib_head_neckline {
        let level = pcl_exhausted_price(head, neckline);
        if level.is_finite() {
            let (name, past) = match direction {
                Direction::Short => ("too-low", VetoSide::Below),
                Direction::Long => ("too-high", VetoSide::Above),
            };
            out.push(EntryLevelVeto {
                name: name.into(),
                level,
                past,
            });
        }
    }

    // invalidation (the right-shoulder horizontal; thesis dead past it).
    if let Some(level) = geom.invalidation
        && level.is_finite()
    {
        let (name, past) = match direction {
            Direction::Short => ("too-high", VetoSide::Above),
            Direction::Long => ("too-low", VetoSide::Below),
        };
        out.push(EntryLevelVeto {
            name: name.into(),
            level,
            past,
        });
    }

    out
}

/// Widen each drawn S/R level into a `±band_pct` band.
///
/// Reads [`PlanGeometry::sr_levels`], not `roles.sr_levels`, so a spec-in re-arm
/// gets the operator's drawn levels rather than silently arming with only the
/// derived TP band — see that field's doc for why the failure is quiet.
pub fn build_sr_ranges(geom: &PlanGeometry, band_pct: f64) -> Vec<[f64; 2]> {
    let pct = band_pct / 100.0;
    geom.sr_levels
        .iter()
        .map(|price| [round5(price * (1.0 - pct)), round5(price * (1.0 + pct))])
        .collect()
}

/// S/R band pinned so its **far edge is the take-profit**, sitting on the
/// approach side (toward entry). A golden reversal candle whose band-anchor lands
/// inside it fires `07-close-on-sr-reversal`, closing the position for a partial
/// win instead of round-tripping to the stop. H&S / iH&S only — the M/W path
/// recomputes TP live and gets no auto band.
///
/// **Width matches a drawn S/R line** ([`build_sr_ranges`]). A drawn line is a
/// full `±pct` band (`2·pct` total) centred on the line; this band is the same
/// total width, but placed so its far edge is TP: the "line" (centre) sits one
/// `pct` step onto the approach side (`TP·(1+pct)` for a short, `TP·(1-pct)` for
/// a long) and the normal `±pct` band around it lands the near edge exactly on
/// TP. So the band never extends *past* TP (a clean run to TP is unaffected) yet
/// reaches a full drawn-band width up the approach side — twice the old
/// one-sided `[TP, TP+pct]`, which was accidentally half a drawn line's width.
/// Catching a reversal that turns further short of TP is a separate lever
/// (`--tp-resistance-pct`).
///
/// `pct` is a percent of the TP price (e.g. `0.1` = 0.1%). Returns `[lo, hi]`
/// with `lo <= hi` as required by the `sr_bands` validator.
pub fn tp_resistance_band(tp: f64, direction: Direction, pct: f64) -> [f64; 2] {
    let pct = pct / 100.0;
    // The S/R "line" (band centre) is one pct step onto the approach side of TP,
    // so the ±pct band's far edge lands back on TP.
    let center = match direction {
        // Long: price rises into TP from below → approach side (and the band) is
        // below TP, top edge = TP.
        Direction::Long => tp * (1.0 - pct),
        // Short: price falls into TP from above → approach side (and the band) is
        // above TP, bottom edge = TP.
        Direction::Short => tp * (1.0 + pct),
    };
    [round5(center * (1.0 - pct)), round5(center * (1.0 + pct))]
}

pub fn round5(v: f64) -> f64 {
    (v * 1e5).round() / 1e5
}

#[cfg(test)]
mod tests {
    //! Tests for the H&S half of trade-spec resolution: which drawings are
    //! required, how direction is read, and how the take-profit resistance
    //! band is derived.
    //!
    //! These lived in `pipeline`'s test module alongside the M/W tests and the
    //! plan-emission tests, all under a single `// ===== M / W trade-spec
    //! resolution` section marker that had stopped describing its contents.

    use super::*;
    use crate::args::Args;
    use crate::plan_geometry::PlanGeometry;
    use crate::roles::Roles;
    use crate::test_drawings::{fib, hline, now, two_point, vline};
    use clap::Parser;
    use trading_view::drawings::Drawing;

    /// Parse `Args` straight from an argv slice, as the binary would.
    fn mw_args(extra: &[&str]) -> Args {
        let mut argv = vec!["tv-arm"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).expect("parse args")
    }

    fn hs_roles(fib: Drawing, inv: Drawing) -> Roles {
        Roles {
            invalidation: Some(inv.clone()),
            invalidation_label: inv.properties.text.clone(),
            break_and_close: Some(two_point("neck", "neckline", 1.10, 1.10)),
            retest: Some(two_point("neck", "neckline", 1.10, 1.10)),
            tp_fib: Some(fib),
            trade_expiry: Some(vline("exp", now().timestamp() + 86_400)),
            ..Default::default()
        }
    }

    /// `check_required` is the guard between a malformed chart and an armed
    /// trade, and it had **zero** tests: replacing its whole body with
    /// `return Ok(())` left all 293 green (verified 2026-07-27). Worse, the
    /// commit that changed it to read `geom.neckline` — fixing a bug where a
    /// ONE-POINT drawing satisfied `roles.break_and_close.is_some()` — shipped
    /// with no regression test, so reverting that fix was free.
    ///
    /// One case per branch, each dropping exactly one role from an otherwise
    /// complete chart, asserting on the specific message rather than just "is
    /// Err" (so a branch can't pass by another's error).
    #[test]
    fn check_required_names_each_missing_drawing() {
        let complete = || hs_roles(fib("fib", 1.20, 1.10), hline("inv", "too-high", 1.15));
        let args = mw_args(&[]);
        assert!(
            check_required(&PlanGeometry::from_roles(&complete()), &args).is_ok(),
            "the complete chart must pass, or the cases below prove nothing"
        );

        // One case: the message `check_required` must produce, paired with the
        // mutation that provokes it. A plain `fn` pointer — none of these capture.
        type MissingCase = (&'static str, fn(&mut Roles));

        let cases: [MissingCase; 4] = [
            ("horizontal_line labeled 'too-high' or 'too-low'", |r| {
                r.invalidation = None
            }),
            (
                "trend_line labeled 'neckline' (or 'break-and-close')",
                |r| r.break_and_close = None,
            ),
            ("fib_retracement (TP)", |r| r.tp_fib = None),
            ("vertical_line labeled 'trade-expiry'", |r| {
                r.trade_expiry = None
            }),
        ];

        for (want, drop_one) in cases {
            let mut roles = complete();
            drop_one(&mut roles);
            let err = check_required(&PlanGeometry::from_roles(&roles), &args)
                .expect_err("a chart missing a required drawing must be rejected");
            assert!(
                err.contains(want),
                "expected the error to name {want:?}, got: {err}"
            );
        }
    }

    /// The `feaa019` fix, pinned: a ONE-POINT drawing in the neckline slot is
    /// **not** a usable neckline. `roles.break_and_close.is_some()` is true for
    /// it (which is what the guard used to test), but `PlanGeometry` needs two
    /// anchors to build a `Line`, so `geom.neckline` is `None` — and it is
    /// `geom` that plan-building actually reads. Reverting the guard to the
    /// `roles` form makes this test fail.
    #[test]
    fn check_required_rejects_a_one_point_neckline() {
        let mut roles = hs_roles(fib("fib", 1.20, 1.10), hline("inv", "too-high", 1.15));
        // Present as a drawing, unusable as a line.
        roles.break_and_close = Some(hline("neck", "neckline", 1.10));
        let err = check_required(&PlanGeometry::from_roles(&roles), &mw_args(&[]))
            .expect_err("a one-point neckline must be rejected");
        assert!(err.contains("trend_line labeled 'neckline'"), "got: {err}");
    }

    /// `--skip-break-and-close` waives the neckline requirement — but only when
    /// the retest is skipped too, since the retest derives from the same drawing.
    /// Both halves asserted, so neither `args` branch is a free mutation.
    #[test]
    fn check_required_waives_the_neckline_only_when_both_preps_are_skipped() {
        let mut roles = hs_roles(fib("fib", 1.20, 1.10), hline("inv", "too-high", 1.15));
        roles.break_and_close = None;
        roles.retest = None;
        let geom = PlanGeometry::from_roles(&roles);

        // Skipping only break-and-close still leaves the retest needing a line.
        let err = check_required(&geom, &mw_args(&["--skip-break-and-close"]))
            .expect_err("the retest still needs the neckline");
        assert!(err.contains("needed for the retest"), "got: {err}");

        // Skipping both waives it entirely.
        assert!(
            check_required(
                &geom,
                &mw_args(&["--skip-break-and-close", "--skip-retest"])
            )
            .is_ok(),
            "with both preps skipped no neckline is required"
        );
    }

    #[test]
    fn hs_direction_comes_from_fib_not_invalidation_label() {
        // Rule 1: the fib is authoritative. Head at 1.20 (0-reading, clicked
        // first) sits ABOVE neckline 1.10 → this is a SHORT (H&S), regardless
        // of the invalidation *label*. The invalidation sits inside the fib
        // range so it passes rule 2.
        let fib = fib("fib", 1.20, 1.10);
        let inv = hline("inv", "too-high", 1.18);
        let roles = hs_roles(fib, inv);
        let args = mw_args(&[]);
        let (dir, spec) = resolve_hs_trade(
            &args,
            &PlanGeometry::from_roles(&roles),
            false,
            "EUR_USD",
            "ms-oanda-1",
            Broker::Oanda,
            0.0001,
            0.0001,
        )
        .expect("valid HS resolves");
        assert_eq!(dir, Direction::Short);
        assert_eq!(spec.pattern, cli::TradePattern::Hs);
    }

    #[test]
    fn hs_inverse_direction_comes_from_fib() {
        // Mirror: head 1.00 BELOW neckline 1.10 → LONG (iH&S). too-low floor
        // at 1.05 sits inside the fib range.
        let fib = fib("fib", 1.00, 1.10);
        let inv = hline("inv", "too-low", 1.05);
        let roles = hs_roles(fib, inv);
        let args = mw_args(&[]);
        let (dir, _) = resolve_hs_trade(
            &args,
            &PlanGeometry::from_roles(&roles),
            false,
            "EUR_USD",
            "ms-oanda-1",
            Broker::Oanda,
            0.0001,
            0.0001,
        )
        .expect("valid iH&S resolves");
        assert_eq!(dir, Direction::Long);
    }

    #[test]
    fn hs_stale_invalidation_outside_fib_range_is_rejected() {
        // The bug: a `too-low` left over from a DIFFERENT trade at 0.95 sits
        // well outside this setup's fib range [1.10, 1.20]. Rule 2 rejects it
        // rather than baking a poison level / mismatched setup.
        let fib = fib("fib", 1.20, 1.10);
        let inv = hline("inv", "too-low", 0.95);
        let roles = hs_roles(fib, inv);
        let args = mw_args(&[]);
        match resolve_hs_trade(
            &args,
            &PlanGeometry::from_roles(&roles),
            false,
            "EUR_USD",
            "ms-oanda-1",
            Broker::Oanda,
            0.0001,
            0.0001,
        ) {
            Err(ResolveError::Reject(msg)) => {
                assert!(msg.contains("outside the fib range"), "msg = {msg}");
            }
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn hs_flat_fib_has_no_direction_and_errors() {
        // A degenerate flat fib (both anchors equal) carries no direction.
        let fib = fib("fib", 1.10, 1.10);
        let inv = hline("inv", "too-high", 1.10);
        let roles = hs_roles(fib, inv);
        let args = mw_args(&[]);
        match resolve_hs_trade(
            &args,
            &PlanGeometry::from_roles(&roles),
            false,
            "EUR_USD",
            "ms-oanda-1",
            Broker::Oanda,
            0.0001,
            0.0001,
        ) {
            Err(ResolveError::Fatal(e)) => {
                assert!(
                    format!("{e}").contains("direction from the fib"),
                    "err = {e}"
                );
            }
            other => panic!("expected Fatal, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn tp_resistance_band_long_far_edge_is_tp() {
        // Long: band hangs UNDER the TP (approach from below), TOP edge = TP.
        // Centre one pct step below TP → the +pct edge lands back on TP.
        let tp = 1.20;
        let pct = 0.1 / 100.0;
        let center = tp * (1.0 - pct);
        let [lo, hi] = tp_resistance_band(tp, Direction::Long, 0.1);
        assert_eq!(hi, round5(center * (1.0 + pct)), "far (top) edge is the TP");
        assert_eq!(hi, round5(tp), "top edge lands exactly on TP");
        assert_eq!(
            lo,
            round5(center * (1.0 - pct)),
            "near edge is 2·pct below TP"
        );
        assert!(lo < hi, "lo < hi for a valid band");
    }

    #[test]
    fn tp_resistance_band_short_far_edge_is_tp() {
        // Short: band sits OVER the TP (approach from above), BOTTOM edge = TP.
        // Centre one pct step above TP → the −pct edge lands back on TP.
        let tp = 1.00;
        let pct = 0.1 / 100.0;
        let center = tp * (1.0 + pct);
        let [lo, hi] = tp_resistance_band(tp, Direction::Short, 0.1);
        assert_eq!(
            lo,
            round5(center * (1.0 - pct)),
            "far (bottom) edge is the TP"
        );
        assert_eq!(lo, round5(tp), "bottom edge lands exactly on TP");
        assert_eq!(
            hi,
            round5(center * (1.0 + pct)),
            "near edge is 2·pct above TP"
        );
        assert!(lo < hi, "lo < hi for a valid band");
    }

    #[test]
    fn tp_resistance_band_matches_a_drawn_sr_line_width() {
        // The operator's requirement: the auto TP band must be the SAME total
        // width as a drawn S/R line for the same pct — not the old half-width.
        // A drawn line at price P → [P·(1−pct), P·(1+pct)]; its width is what the
        // TP band must match. Compare at the TP band's own centre.
        for (dir, tp) in [(Direction::Short, 1.00), (Direction::Long, 1.20)] {
            let [lo, hi] = tp_resistance_band(tp, dir, 0.1);
            let tp_width = hi - lo;
            // A drawn band centred at this TP band's centre (the S/R "line").
            let pct = 0.1 / 100.0;
            let center = match dir {
                Direction::Short => tp * (1.0 + pct),
                Direction::Long => tp * (1.0 - pct),
            };
            let drawn = [round5(center * (1.0 - pct)), round5(center * (1.0 + pct))];
            let drawn_width = drawn[1] - drawn[0];
            assert!(
                (tp_width - drawn_width).abs() < 1e-9,
                "{dir:?}: TP band width {tp_width} must equal a drawn S/R band width {drawn_width}"
            );
            // And it must be ~twice the old one-sided width (pct·tp), the bug.
            let old_one_sided = pct * tp;
            assert!(
                tp_width > old_one_sided * 1.9,
                "{dir:?}: new width {tp_width} must be ~2× the old one-sided {old_one_sided}"
            );
        }
    }

    #[test]
    fn hs_default_adds_tp_resistance_band() {
        // Short H&S: head 1.20 above neckline 1.10 → TP = 2·1.10 − 1.20 = 1.00.
        // No drawn S/R lines, default flags → exactly one auto band whose far
        // (lower, for a short) edge is the TP.
        let roles = hs_roles(fib("fib", 1.20, 1.10), hline("inv", "too-high", 1.15));
        let args = mw_args(&[]);
        let (_dir, spec) = resolve_hs_trade(
            &args,
            &PlanGeometry::from_roles(&roles),
            false,
            "EUR_USD",
            "ms-oanda-1",
            Broker::Oanda,
            0.0001,
            0.0001,
        )
        .expect("valid H&S resolves");
        assert_eq!(
            spec.sr_reversal_ranges.len(),
            1,
            "one auto band, no drawn S/R"
        );
        let [lo, _hi] = spec.sr_reversal_ranges[0];
        assert_eq!(lo, round5(spec.tp_price), "short far edge (lo) == TP");
    }

    #[test]
    fn hs_skip_tp_resistance_leaves_no_band() {
        // Same setup, but --skip-tp-resistance and no drawn S/R → no bands, so
        // no 07-close-on-sr-reversal alert gets emitted downstream.
        let roles = hs_roles(fib("fib", 1.20, 1.10), hline("inv", "too-high", 1.15));
        let args = mw_args(&["--skip-tp-resistance"]);
        let (_dir, spec) = resolve_hs_trade(
            &args,
            &PlanGeometry::from_roles(&roles),
            false,
            "EUR_USD",
            "ms-oanda-1",
            Broker::Oanda,
            0.0001,
            0.0001,
        )
        .expect("valid H&S resolves");
        assert!(
            spec.sr_reversal_ranges.is_empty(),
            "no drawn S/R and band skipped"
        );
    }

    #[test]
    fn hs_drawn_sr_plus_auto_band() {
        // A drawn support/resistance line contributes its own band; the auto TP
        // band is appended alongside it → two bands total.
        let mut roles = hs_roles(fib("fib", 1.20, 1.10), hline("inv", "too-high", 1.15));
        roles.sr_levels = vec![hline("sr", "support", 1.05)];
        let args = mw_args(&[]);
        let (_dir, spec) = resolve_hs_trade(
            &args,
            &PlanGeometry::from_roles(&roles),
            false,
            "EUR_USD",
            "ms-oanda-1",
            Broker::Oanda,
            0.0001,
            0.0001,
        )
        .expect("valid H&S resolves");
        assert_eq!(
            spec.sr_reversal_ranges.len(),
            2,
            "drawn band + auto TP band"
        );
    }
}
