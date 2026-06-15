//! Per-bar evolution of an M/W setup's live geometry.
//!
//! M/W setups arm in real time with only the **left shoulder** and
//! **neckline** known; the right tower forms bar by bar. The book reads
//! two things off a *finished* chart that we must recover live:
//!
//! 1. a **higher right shoulder** — the book measures the stop from the
//!    higher of the two shoulders;
//! 2. a **deeper neckline** — a deeper pullback between the towers reshapes
//!    the neckline (and the whole pattern's invalidation level).
//!
//! [`plan_mw_update`] is the pure decision that, given the baked anchors,
//! the prior [`MwState`] (if any) and the just-closed bar's body extremes,
//! returns what to do this bar. It is KV-free and side-effect-free so it
//! unit-tests natively; the worker wraps it with the KV read/write.
//!
//! ## Body extremes, not wicks (rogue-wick handling)
//!
//! Every comparison uses the candle **body** (`max(open,close)` /
//! `min(open,close)`), never the wick high/low. A lone rogue wick poking
//! below the validity floor or above the prior shoulder therefore can't
//! cancel the trade or move the shoulder — exactly the book's "ignore an
//! isolated wick" rule, applied live. If the bar didn't carry `open`
//! (a chart still on the pre-`open` Pine), there are no bodies to read and
//! the plan is [`MwUpdate::NoChange`] — the setup rides its baked geometry.
//!
//! ## The 60% validity floor
//!
//! The pattern stays alive only while price holds in the upper 60% of the
//! runup→shoulder leg. For an M (short), with the runup running *up* from
//! `runup_start` (A) to `left_shoulder` (B):
//!
//! ```text
//!   floor = runup_start + 0.60 × (left_shoulder − runup_start)
//!   body_low ≥ floor  → still valid (and, if below the neckline, revise it)
//!   body_low <  floor → cancel: the pullback ate too much of the runup
//! ```
//!
//! For a W (long) it mirrors: the runup runs *down*, the floor sits 60% of
//! the way down from `runup_start` to `left_shoulder`, and the bar's
//! **body_high** is tested against it.
//!
//! All prices are **MID** (same basis as [`MwParams`][crate::intent::MwParams]
//! and [`MwState`]); the mid→bid/ask correction stays in `mw_resolution`.

use chrono::{DateTime, Utc};

use super::{Direction, Shell};
use crate::state::MwState;

/// Fraction of the runup→shoulder leg that price must hold to keep the
/// pattern alive. A body beyond this floor cancels the setup. See the
/// module docs.
const VALIDITY_FLOOR_FRAC: f64 = 0.60;

/// Upper clamp for recording a higher right shoulder, as a fraction of the
/// neckline→shoulder (C→B) leg past the neckline — the 1.3 extension, the
/// same level the `mw-cancel` veto guards. A body beyond it isn't a second
/// shoulder, it's an invalidation, so we don't record it as one.
const CANCEL_EXT_FRAC: f64 = 1.3;

/// What [`plan_mw_update`] decided for this bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MwUpdate {
    /// The setup is still valid. `state` is the (possibly-updated) geometry
    /// to persist and to resolve the entry against this bar. `changed` is
    /// true iff the neckline or right shoulder actually moved (the worker
    /// can skip the KV write when false).
    Proceed { state: MwState, changed: bool },
    /// A body breached the 60% validity floor — the pattern is dead. The
    /// worker cancels any pending order + blocks future entries (never
    /// closes an open position) and clears the state row.
    Cancel,
    /// The bar carried no `open`, so bodies can't be computed. Keep the
    /// prior state untouched and resolve against baked geometry this bar.
    NoChange,
}

/// The baked, immutable anchors of an M/W setup — the seed geometry the
/// per-bar update evolves on top of. Grouped so [`plan_mw_update`] takes a
/// single geometry argument rather than three loose `f64`s. All MID prices
/// straight off [`MwParams`][crate::intent::MwParams].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MwAnchors {
    pub direction: Direction,
    /// `A` — runup start.
    pub runup_start: f64,
    /// `B` — left shoulder (first peak for M / first trough for W).
    pub left_shoulder: f64,
    /// `C` — baked neckline (the seed; a revised neckline lives in
    /// [`MwState`]).
    pub baked_neckline: f64,
}

/// Decide this bar's M/W geometry update.
///
/// - `anchors` are the baked MID anchors from [`MwParams`][crate::intent::MwParams].
/// - `prior` is the persisted [`MwState`] from earlier bars, or `None` on
///   the first update (we seed from the baked neckline).
/// - `shell` is the just-closed bar; only its body extremes are read.
/// - `now` stamps the returned state's `updated_at`; `expires_at` is the
///   safety TTL the caller already computed (alert window + grace).
pub fn plan_mw_update(
    anchors: MwAnchors,
    prior: Option<MwState>,
    shell: &Shell,
    now: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> MwUpdate {
    let MwAnchors {
        direction,
        runup_start,
        left_shoulder,
        baked_neckline,
    } = anchors;

    // No bodies → can't evolve; ride baked geometry this bar.
    let (Some(body_high), Some(body_low)) = (shell.body_high(), shell.body_low()) else {
        return MwUpdate::NoChange;
    };

    let neckline = prior.map(|p| p.neckline).unwrap_or(baked_neckline);
    let right_shoulder = prior.and_then(|p| p.right_shoulder);

    // 60% validity floor on the runup→shoulder (A→B) leg.
    let floor = runup_start + VALIDITY_FLOOR_FRAC * (left_shoulder - runup_start);
    // 1.3 cancel extension on the neckline→shoulder (C→B) leg (uses the
    // *baked* neckline so a revised neckline can't drift the ceiling).
    let cancel = baked_neckline + CANCEL_EXT_FRAC * (left_shoulder - baked_neckline);

    match direction {
        // M (short): runup runs up A→B, neckline below B. Test body_low
        // against the floor (down) and the neckline; body_high for the
        // right shoulder (up, capped at the cancel ext).
        Direction::Short => {
            if body_low < floor {
                return MwUpdate::Cancel;
            }
            let mut new_neckline = neckline;
            let mut changed = false;
            // Deeper pullback that's still valid → lower the neckline.
            if body_low < new_neckline {
                new_neckline = body_low;
                changed = true;
            }
            // Higher right shoulder (below the cancel ext) → record it.
            let mut new_rs = right_shoulder;
            if body_high < cancel && right_shoulder.is_none_or(|rs| body_high > rs) {
                new_rs = Some(body_high);
                changed = true;
            }
            MwUpdate::Proceed {
                state: MwState {
                    neckline: new_neckline,
                    right_shoulder: new_rs,
                    updated_at: now,
                    expires_at,
                },
                changed,
            }
        }
        // W (long): mirror. Runup runs down A→B, neckline above B. Test
        // body_high against the floor (up) and the neckline; body_low for
        // the right shoulder (down, capped at the cancel ext below).
        Direction::Long => {
            if body_high > floor {
                return MwUpdate::Cancel;
            }
            let mut new_neckline = neckline;
            let mut changed = false;
            if body_high > new_neckline {
                new_neckline = body_high;
                changed = true;
            }
            let mut new_rs = right_shoulder;
            if body_low > cancel && right_shoulder.is_none_or(|rs| body_low < rs) {
                new_rs = Some(body_low);
                changed = true;
            }
            MwUpdate::Proceed {
                state: MwState {
                    neckline: new_neckline,
                    right_shoulder: new_rs,
                    updated_at: now,
                    expires_at,
                },
                changed,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::SignalKind;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn now() -> DateTime<Utc> {
        ts("2026-05-13T12:00:00Z")
    }

    fn exp() -> DateTime<Utc> {
        ts("2026-05-13T20:00:00Z")
    }

    /// A shell with explicit open/close so body extremes are defined.
    /// high/low are set wide so they never accidentally drive logic
    /// (proving body-based, not wick-based, decisions).
    fn shell_oc(open: f64, close: f64) -> Shell {
        Shell {
            close,
            high: open.max(close) + 1.0,
            low: open.min(close) - 1.0,
            open: Some(open),
            time: now(),
            signal_high: None,
            signal_low: None,
            signal_range: None,
            signal_start_time: None,
            signal_kind: None::<SignalKind>,
            golden: None,
            atr: None,
            signal_confirmed: None,
            recent_high: None,
            recent_low: None,
            next_candle_timestamp_1: None,
            next_candle_timestamp_2: None,
            next_candle_timestamp_3: None,
            next_candle_timestamp_4: None,
            next_candle_timestamp_5: None,
        }
    }

    // M worked anchors: A = 1.1000, B = 1.1200, C = 1.1120.
    //   runup leg A→B = 0.0200; floor = A + 0.6×0.0200 = 1.1120.
    //   C→B leg = 0.0080; cancel = C + 1.3×0.0080 = 1.1224.
    const M_A: f64 = 1.1000;
    const M_B: f64 = 1.1200;
    const M_C: f64 = 1.1120;

    fn plan_m(prior: Option<MwState>, shell: &Shell) -> MwUpdate {
        let anchors = MwAnchors {
            direction: Direction::Short,
            runup_start: M_A,
            left_shoulder: M_B,
            baked_neckline: M_C,
        };
        plan_mw_update(anchors, prior, shell, now(), exp())
    }

    #[test]
    fn no_open_is_no_change() {
        let mut s = shell_oc(1.1150, 1.1150);
        s.open = None;
        assert_eq!(plan_m(None, &s), MwUpdate::NoChange);
    }

    #[test]
    fn body_low_below_floor_cancels() {
        // body_low = min(open,close) = 1.1110 < floor 1.1120 → cancel.
        // (Note high/low are wide, proving we test the body, not the wick.)
        let s = shell_oc(1.1160, 1.1110);
        assert_eq!(plan_m(None, &s), MwUpdate::Cancel);
    }

    #[test]
    fn rogue_wick_below_floor_does_not_cancel() {
        // Body holds at/above the floor (open=close=1.1130 ≥ 1.1120) but the
        // wick low is 1.1130-1.0 way below — must NOT cancel.
        let s = shell_oc(1.1130, 1.1130);
        assert!(matches!(plan_m(None, &s), MwUpdate::Proceed { .. }));
    }

    #[test]
    fn deeper_body_low_revises_neckline() {
        // body_low 1.1125 is above the floor (1.1120) but below the neckline
        // (1.1120? no — neckline is 1.1120, 1.1125 > it). Use a bar whose
        // body_low sits between floor and neckline: floor=neckline=1.1120
        // here, so pick a setup where they differ. (See note below.)
        // For this anchor set floor == neckline == 1.1120, so a revision
        // requires body_low in [1.1120, 1.1120) — empty. Use a bar exactly
        // at a deeper-but-valid level by lowering via a prior state instead.
        let prior = MwState {
            neckline: 1.1140,
            right_shoulder: None,
            updated_at: now(),
            expires_at: exp(),
        };
        // body_low 1.1125 < prior neckline 1.1140 and ≥ floor 1.1120 → revise.
        let s = shell_oc(1.1160, 1.1125);
        match plan_m(Some(prior), &s) {
            MwUpdate::Proceed { state, changed } => {
                assert!(changed);
                assert!((state.neckline - 1.1125).abs() < 1e-9, "{}", state.neckline);
            }
            other => panic!("expected Proceed, got {other:?}"),
        }
    }

    #[test]
    fn higher_body_high_records_right_shoulder() {
        // body_high 1.1190 (< cancel 1.1224) and no prior shoulder → record.
        // body_low 1.1160 ≥ floor so no cancel.
        let s = shell_oc(1.1190, 1.1160);
        match plan_m(None, &s) {
            MwUpdate::Proceed { state, changed } => {
                assert!(changed);
                assert_eq!(state.right_shoulder, Some(1.1190));
            }
            other => panic!("expected Proceed, got {other:?}"),
        }
    }

    #[test]
    fn rogue_wick_above_cancel_does_not_record_shoulder() {
        // Body high 1.1200 (< cancel 1.1224) records; but a bar whose body
        // high is ABOVE cancel must not record a shoulder. open=close=1.1230
        // > cancel 1.1224.
        let s = shell_oc(1.1230, 1.1230);
        // body_low 1.1230 ≥ floor so not a cancel; shoulder not recorded.
        match plan_m(None, &s) {
            MwUpdate::Proceed { state, changed } => {
                assert!(!changed);
                assert_eq!(state.right_shoulder, None);
            }
            other => panic!("expected Proceed, got {other:?}"),
        }
    }

    #[test]
    fn lower_body_high_does_not_lower_existing_shoulder() {
        let prior = MwState {
            neckline: M_C,
            right_shoulder: Some(1.1190),
            updated_at: now(),
            expires_at: exp(),
        };
        // body_high 1.1175 < existing 1.1190 → no change to the shoulder.
        let s = shell_oc(1.1175, 1.1160);
        match plan_m(Some(prior), &s) {
            MwUpdate::Proceed { state, changed } => {
                assert!(!changed);
                assert_eq!(state.right_shoulder, Some(1.1190));
            }
            other => panic!("expected Proceed, got {other:?}"),
        }
    }

    // W mirror anchors: A = 1.1200, B = 1.1000, C = 1.1080.
    //   runup leg = 0.0200; floor = A − 0.6×0.0200 = 1.1080.
    //   C→B leg = 0.0080 (down); cancel = C − 1.3×0.0080 = 1.0976.
    const W_A: f64 = 1.1200;
    const W_B: f64 = 1.1000;
    const W_C: f64 = 1.1080;

    fn plan_w(prior: Option<MwState>, shell: &Shell) -> MwUpdate {
        let anchors = MwAnchors {
            direction: Direction::Long,
            runup_start: W_A,
            left_shoulder: W_B,
            baked_neckline: W_C,
        };
        plan_mw_update(anchors, prior, shell, now(), exp())
    }

    #[test]
    fn w_body_high_above_floor_cancels() {
        // body_high 1.1090 > floor 1.1080 → cancel (pullback too shallow up).
        let s = shell_oc(1.1090, 1.1085);
        assert_eq!(plan_w(None, &s), MwUpdate::Cancel);
    }

    #[test]
    fn w_lower_body_high_records_right_trough() {
        // W shoulder is a trough: body_low 1.1010 (> cancel 1.0976) recorded.
        // body_high 1.1070 ≤ floor 1.1080 so no cancel.
        let s = shell_oc(1.1070, 1.1010);
        match plan_w(None, &s) {
            MwUpdate::Proceed { state, changed } => {
                assert!(changed);
                assert_eq!(state.right_shoulder, Some(1.1010));
            }
            other => panic!("expected Proceed, got {other:?}"),
        }
    }
}
