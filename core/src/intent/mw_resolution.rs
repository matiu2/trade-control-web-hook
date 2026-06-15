//! M / W (double-top / double-bottom) entry resolution.
//!
//! The standard [`Resolved::from_intent`] path reads `entry` /
//! `stop_loss` / `take_profit` straight off the intent. M/W setups
//! carry none of those — instead `tv-arm` bakes the path geometry into
//! [`MwParams`] (neckline, first point, runup start, arm-time spread +
//! pip size, all **MID** prices) and the worker derives the stop-entry
//! order, stop loss and take-profit here, per bar close, from those
//! baked params.
//!
//! ## Mid → bid/ask correction
//!
//! The book's M/W rules are written for **bid** charts; ours are
//! **mid**. We apply the *mid-correct* translation: `±½ spread` on every
//! level to reach the bid (sell) / ask (buy) the book assumed, on top of
//! the book's own pip buffers.
//!
//! With `half_spread = spread/2`, `half_pip = ½·pip`, `one_pip = pip`:
//!
//! ```text
//! W (long, fill @ ask):
//!   entry = neckline + half_spread + half_pip
//!   sl    = first_point + half_spread − (spread + one_pip)   // below the trough
//!   tp    = entry + (entry − sl)                             // exactly 1R
//!
//! M (short, fill @ bid):
//!   entry = neckline − half_spread − half_pip
//!   sl    = first_point − half_spread + (spread + one_pip)   // above the peak
//!   tp    = entry − (sl − entry)                             // exactly 1R
//! ```
//!
//! TP is exactly 1R off the *entry*, so the spread already embedded in
//! entry and SL carries straight through — no extra TP shift needed.
//!
//! ## Why a real-time arming gate at all
//!
//! The strategy book is a **post-hoc** method: the analyst looks at a
//! chart where *both* towers of the M are already printed and simply puts
//! a stop at the neckline ("entry on the break of the neckline … no
//! retest required"). We arm in **real time**, when only the **left
//! shoulder (B)** and the **neckline (C)** are complete — the right tower
//! hasn't formed yet. So we cannot just "enter on neckline break": at the
//! moment of a break we don't yet know whether a genuine right tower
//! formed. The two gates below are the live stand-ins for the validity
//! the book gets for free from a finished chart.
//!
//! ## Right-tower confirmation window
//!
//! Before arming the breakout stop, the bar must show a real second
//! peak/trough — price that has rallied (M) / dropped (W) back *into* the
//! pattern far enough to count as a right tower, the live equivalent of
//! the book's "the right side's top is close to the left side's top"
//! check. The bar's extreme (high for an M, low for a W) must reach
//! **within 30% of the left-shoulder high** — i.e. into the top 30% of the
//! neckline→first-point (C→B) leg — and stay below the 1.3 extension:
//!
//! ```text
//!   right_tower = neckline + 0.7 × (first_point − neckline)   // within 30% of B
//!   cancel      = neckline + 1.3 × (first_point − neckline)
//!   confirmed  ⇔  high (M) / low (W) ∈ [right_tower, cancel)
//! ```
//!
//! Below `right_tower` the second peak is too shallow; at/above `cancel`
//! the 1.3 extension is breached and the pattern is invalidated (the same
//! level the `mw-cancel` veto guards — checked here too as a safety net).
//! Either way the bar is declined and the setup stays armed. This fixes a
//! real AUD/CAD case where a bar closing just past the neckline but with a
//! high short of the second peak armed (and filled) a premature short.
//!
//! ## "Middle of the M" downward-cross trigger
//!
//! A confirmed right tower says the shape is valid; it does not say price
//! is *rolling back off* it yet. The arming trigger is the bar that
//! crosses back down (M) / up (W) through the **50% level of the C→B
//! leg** — the "middle of the M":
//!
//! ```text
//!   mid50 = neckline + 0.5 × (first_point − neckline)
//!   M (short): high ≥ mid50  AND  close < mid50   // crossed down through the middle
//!   W (long):  low  ≤ mid50  AND  close > mid50   // crossed up through the middle
//! ```
//!
//! Only once price has both confirmed a right tower *and* crossed back
//! through the middle do we arm the breakout stop. A bar that fails the
//! cross is declined and the setup stays armed for the next bar.
//!
//! ## "Stay armed" semantics
//!
//! The enter alert fires every bar close. A bar that fails the
//! confirmation window above, or whose close has *not* yet broken the
//! neckline (so the breakout stop would sit on the wrong side of the
//! current price), is declined here with
//! [`ResolveError::InvalidGeometry`]. Post the 2026-06 seen-id fix a
//! non-`Ok` resolve does **not** mark the intent id seen, so the next
//! bar's fire is allowed through — i.e. the setup stays armed until a
//! bar actually breaks out (or a cancel / abort / expiry veto ends it).

use super::resolution::{ResolveError, Resolved, ResolvedEntry};
use super::{Direction, Intent, MwParams, Shell};

/// Minimum right-tower retracement, as a fraction of the neckline→first-point
/// (C→B) leg, that a bar must reach before the setup can arm. `0.7` means the
/// bar's extreme (high for M, low for W) must come **within 30% of the
/// left-shoulder high** — into the top 30% of the C→B leg. A bar that hasn't
/// pulled this far back into the pattern is declined and the setup stays armed.
/// See [`Resolved::from_mw_intent`].
const RIGHT_TOWER_MIN_FRAC: f64 = 0.7;

/// The "middle of the M": the 50% level of the neckline→first-point (C→B) leg.
/// The arming trigger is the bar that crosses back down (M) / up (W) through
/// this level. See [`Resolved::from_mw_intent`].
const MID_CROSS_FRAC: f64 = 0.5;

/// Upper clamp on the second peak, as a fraction of the C→B leg: the 1.3
/// extension past the neckline. A bar reaching this far has invalidated the
/// pattern (same level the `mw-cancel` veto guards) — declined here as a
/// safety net in case that veto hasn't fired yet. Mirrors `cancel_level`
/// in `tv-arm`'s `mw_geometry`.
const CANCEL_EXT_FRAC: f64 = 1.3;

impl Resolved {
    /// Resolve an M/W `enter` intent. `mw` is the intent's baked
    /// [`MwParams`]; validation (Enter-only, finite fields, `pip_size >
    /// 0`) has already run in [`Intent::validate`].
    pub(in crate::intent) fn from_mw_intent(
        intent: &Intent,
        shell: &Shell,
        mw: &MwParams,
    ) -> Result<Self, ResolveError> {
        let direction = intent
            .direction
            .ok_or(ResolveError::MissingField("direction"))?;

        let pip = mw.pip_size;
        let spread = mw.spread_pips * pip;
        let half_spread = spread / 2.0;
        let half_pip = 0.5 * pip;
        let one_pip = pip;
        let neckline = mw.neckline;
        let peak = mw.first_point;

        let (entry, stop_loss, take_profit) = match direction {
            // W (long): break *up* through the neckline, fill at ask.
            Direction::Long => {
                let entry = neckline + half_spread + half_pip;
                let sl = peak + half_spread - (spread + one_pip);
                let tp = entry + (entry - sl);
                (entry, sl, tp)
            }
            // M (short): break *down* through the neckline, fill at bid.
            Direction::Short => {
                let entry = neckline - half_spread - half_pip;
                let sl = peak - half_spread + (spread + one_pip);
                let tp = entry - (sl - entry);
                (entry, sl, tp)
            }
        };

        // Right-tower confirmation *window*. Before arming the breakout stop
        // we require the price to have rallied (M) / dropped (W) back *into*
        // the pattern — far enough to count as a real right tower (within 30%
        // of the left-shoulder high), but not so far that it blew past the
        // pattern entirely.
        //
        // The window is expressed as fractions of the neckline→first-point
        // (C→B) leg:
        //
        //   lower = neckline + 0.7 × (first_point − neckline)   (within 30% of B)
        //   upper = neckline + 1.3 × (first_point − neckline)   (cancel level)
        //
        // For an M (short) B > C, so the window sits *above* the neckline and
        // we test the bar's `high`. For a W (long) B < C, the window sits
        // *below* the neckline and we test the bar's `low`. B, C and high/low
        // are all MID prices — no spread correction on this gate.
        //
        // - Below `lower`: the right tower isn't tall enough yet → decline,
        //   stay armed for the next bar.
        // - At/above `upper`: price reached the 1.3 extension — the pattern is
        //   invalidated (this is the same level the `mw-cancel` veto guards).
        //   Decline as a safety net in case that veto hasn't fired yet.
        // A bar inside [lower, upper) is a confirmed right tower → proceed to
        // the middle-of-the-M cross check below (see module docs).
        let right_tower = neckline + RIGHT_TOWER_MIN_FRAC * (peak - neckline);
        let cancel = neckline + CANCEL_EXT_FRAC * (peak - neckline);
        let right_tower_confirmed = match direction {
            // M: right tower is above the neckline. The high must reach the
            // right-tower level but stay below the 1.3 cancel extension.
            Direction::Short => shell.high >= right_tower && shell.high < cancel,
            // W: mirror — right trough below the neckline. The low must drop
            // to the right-tower level but stay above the cancel extension.
            Direction::Long => shell.low <= right_tower && shell.low > cancel,
        };
        if !right_tower_confirmed {
            return Err(ResolveError::InvalidGeometry);
        }

        // "Middle of the M" downward-cross trigger. A confirmed right tower
        // says the shape is valid; the arming trigger is the bar that rolls
        // back *off* it through the 50% level of the C→B leg:
        //
        //   mid50 = neckline + 0.5 × (first_point − neckline)
        //   M (short): high ≥ mid50 AND close < mid50   (crossed down through it)
        //   W (long):  low  ≤ mid50 AND close > mid50   (crossed up through it)
        //
        // The high/low condition proves the bar traded on the far side of the
        // middle (so it's a genuine crossing, not a bar already wholly past
        // it); the close condition proves it ended up back on the breakout
        // side. A bar that hasn't crossed is declined → stay armed.
        let mid50 = neckline + MID_CROSS_FRAC * (peak - neckline);
        let crossed_middle = match direction {
            Direction::Short => shell.high >= mid50 && shell.close < mid50,
            Direction::Long => shell.low <= mid50 && shell.close > mid50,
        };
        if !crossed_middle {
            return Err(ResolveError::InvalidGeometry);
        }

        // The entry is a breakout *stop*: it must sit on the far side of
        // the current close (above for a long, below for a short). If the
        // candle hasn't broken out yet — or gapped clean past the level —
        // decline this bar and stay armed for the next (see module docs).
        let stop_on_correct_side = match direction {
            Direction::Long => entry > shell.close,
            Direction::Short => entry < shell.close,
        };
        if !stop_on_correct_side {
            return Err(ResolveError::InvalidGeometry);
        }

        // Shared tail: SL..TP range check, geometry snapshot, min_r, and
        // risk sizing. `reference_price` for a stop entry is the trigger
        // price itself.
        Self::finish_with_sizing(
            intent,
            shell,
            pip,
            direction,
            ResolvedEntry::Stop {
                trigger_price: entry,
            },
            entry,
            stop_loss,
            take_profit,
            // M/W bakes its own static stop-entry geometry and carries no
            // `EntrySpec`, so there's no `on_too_close` fallback to thread.
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::{Action, BrokerKind};
    use crate::tunable::Tunable;
    use chrono::{DateTime, Utc};

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// A bare shell at `close` with explicit `high`/`low`. The M/W
    /// resolution reads `close` for the stop-side check and `high` (M) /
    /// `low` (W) for the second-peak confirmation window.
    fn shell_hlc(high: f64, low: f64, close: f64) -> Shell {
        Shell {
            close,
            high,
            low,
            time: ts("2026-05-13T12:00:00Z"),
            signal_high: None,
            signal_low: None,
            signal_range: None,
            signal_start_time: None,
            signal_kind: None,
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

    fn mw_intent(direction: Direction, mw: MwParams) -> Intent {
        Intent {
            v: 1,
            id: "mw-test".into(),
            not_before: None,
            not_after: ts("2026-05-13T20:00:00Z"),
            action: Action::Enter,
            instrument: "EUR_USD".into(),
            direction: Some(direction),
            entry: None,
            stop_loss: None,
            take_profit: None,
            risk_pct: Tunable::Static(1.0),
            risk_amount: None,
            size_units: None,
            dry_run: None,
            cooldown_hours: None,
            min_r: None,
            broker: BrokerKind::Oanda,
            account: None,
            step: None,
            name: None,
            ttl_hours: Tunable::Static(0),
            level: None,
            requires_preps: Vec::new(),
            vetos: Vec::new(),
            clears: Vec::new(),
            trade_id: None,
            max_retries: Tunable::Static(0),
            expiry_bars: None,
            allow_entry: None,
            allow_close: None,
            needs_golden: false,
            needs_confirmed: false,
            blackout_id: None,
            news_id: None,
            require_news_window: None,
            require_price_in_ranges: None,
            inside_window: Vec::new(),
            sr_bands: Vec::new(),
            veto_on_reversal: false,
            reason: None,
            mw: Some(mw),
            pip_size: Some(mw.pip_size),
        }
    }

    // Worked M (short): neckline C = 1.1120, first_point B = 1.1200.
    // spread = 0.8 pips, pip = 0.0001 → spread = 0.00008,
    //   half_spread = 0.00004, half_pip = 0.00005, one_pip = 0.0001.
    //   entry = 1.1120 − 0.00004 − 0.00005 = 1.11191
    //   sl    = 1.1200 − 0.00004 + (0.00008 + 0.0001) = 1.12014
    //   risk  = sl − entry = 0.00823
    //   tp    = entry − risk = 1.10368
    fn m_params() -> MwParams {
        MwParams {
            neckline: 1.1120,
            first_point: 1.1200,
            runup_start: 1.1000,
            spread_pips: 0.8,
            pip_size: 0.0001,
        }
    }

    // Worked W (long), mirror: neckline C = 1.1080, first_point B = 1.1000.
    //   entry = 1.1080 + 0.00004 + 0.00005 = 1.10809
    //   sl    = 1.1000 + 0.00004 − (0.00008 + 0.0001) = 1.09986
    //   risk  = entry − sl = 0.00823
    //   tp    = entry + risk = 1.11632
    fn w_params() -> MwParams {
        MwParams {
            neckline: 1.1080,
            first_point: 1.1000,
            runup_start: 1.1200,
            spread_pips: 0.8,
            pip_size: 0.0001,
        }
    }

    #[test]
    fn m_short_levels_match_hand_calc() {
        // Close above the entry (neckline not yet decisively broken from
        // below) → stop sits below close → placeable.
        let intent = mw_intent(Direction::Short, m_params());
        // high = 1.1200 (the peak) sits inside the [1.1176, 1.1224) window.
        let r = Resolved::from_mw_intent(&intent, &shell_hlc(1.1200, 1.1120, 1.1120), &m_params())
            .unwrap();
        let trigger = match r.entry {
            ResolvedEntry::Stop { trigger_price } => trigger_price,
            other => panic!("expected Stop, got {other:?}"),
        };
        assert!((trigger - 1.11191).abs() < 1e-9, "entry = {trigger}");
        assert!((r.stop_loss - 1.12014).abs() < 1e-9, "sl = {}", r.stop_loss);
        assert!(
            (r.take_profit - 1.10368).abs() < 1e-9,
            "tp = {}",
            r.take_profit
        );
        // Exactly 1R: |entry − tp| == |sl − entry|.
        assert!(((trigger - r.take_profit).abs() - (r.stop_loss - trigger).abs()).abs() < 1e-9);
    }

    #[test]
    fn w_long_levels_match_hand_calc() {
        let intent = mw_intent(Direction::Long, w_params());
        // low = 1.1000 (the trough) sits inside the (1.0976, 1.1024] window.
        let r = Resolved::from_mw_intent(&intent, &shell_hlc(1.1080, 1.1000, 1.1080), &w_params())
            .unwrap();
        let trigger = match r.entry {
            ResolvedEntry::Stop { trigger_price } => trigger_price,
            other => panic!("expected Stop, got {other:?}"),
        };
        assert!((trigger - 1.10809).abs() < 1e-9, "entry = {trigger}");
        assert!((r.stop_loss - 1.09986).abs() < 1e-9, "sl = {}", r.stop_loss);
        assert!(
            (r.take_profit - 1.11632).abs() < 1e-9,
            "tp = {}",
            r.take_profit
        );
        assert!(((r.take_profit - trigger).abs() - (trigger - r.stop_loss).abs()).abs() < 1e-9);
    }

    #[test]
    fn zero_spread_reduces_to_pure_pip() {
        // spread_pips = 0 → half_spread = 0; entry is exactly half a pip
        // off the neckline, sl exactly one pip beyond the peak.
        let mut mw = m_params();
        mw.spread_pips = 0.0;
        let intent = mw_intent(Direction::Short, mw);
        let r = Resolved::from_mw_intent(&intent, &shell_hlc(1.1200, 1.1120, 1.1120), &mw).unwrap();
        let trigger = match r.entry {
            ResolvedEntry::Stop { trigger_price } => trigger_price,
            other => panic!("expected Stop, got {other:?}"),
        };
        // entry = neckline − 0 − half_pip = 1.1120 − 0.00005 = 1.11195
        assert!((trigger - 1.11195).abs() < 1e-9, "entry = {trigger}");
        // sl = peak − 0 + (0 + one_pip) = 1.1200 + 0.0001 = 1.1201
        assert!((r.stop_loss - 1.1201).abs() < 1e-9, "sl = {}", r.stop_loss);
    }

    #[test]
    fn short_stop_on_wrong_side_is_declined() {
        // Close already below the entry (price gapped through) → the stop
        // can't be placed below the close → InvalidGeometry (stay armed).
        let intent = mw_intent(Direction::Short, m_params());
        // high passes the window; close 1.1100 is below entry → stop-side fail.
        let err =
            Resolved::from_mw_intent(&intent, &shell_hlc(1.1200, 1.1100, 1.1100), &m_params());
        assert!(matches!(err, Err(ResolveError::InvalidGeometry)), "{err:?}");
    }

    #[test]
    fn long_stop_on_wrong_side_is_declined() {
        let intent = mw_intent(Direction::Long, w_params());
        // low passes the window; close 1.1090 is above entry → stop-side fail.
        let err =
            Resolved::from_mw_intent(&intent, &shell_hlc(1.1090, 1.1000, 1.1090), &w_params());
        assert!(matches!(err, Err(ResolveError::InvalidGeometry)), "{err:?}");
    }

    // ---- Second-peak confirmation window (the 0.7 / 1.3 gate) ----
    //
    // The real AUD/CAD setup that motivated this gate:
    //   neckline C = 0.98339, first_point B = 0.98509, spread 1.7 pips.
    //   min_retrace = C + 0.7×(B−C) = 0.98339 + 0.7×0.00170 = 0.98458
    //   cancel      = C + 1.3×(B−C) = 0.98339 + 1.3×0.00170 = 0.98560
    //   The motivating bar had high 0.98430 (< 0.98458) yet a close above
    //   the entry, so the old code armed; the new gate declines it.
    fn audcad_m() -> MwParams {
        MwParams {
            neckline: 0.98339,
            first_point: 0.98509,
            runup_start: 0.97856,
            spread_pips: 1.7,
            pip_size: 0.0001,
        }
    }

    #[test]
    fn m_high_below_min_retrace_is_declined() {
        // The motivating bug: high 0.98430 < min_retrace 0.98458 → decline,
        // even though the close sits above the entry stop (~0.98326).
        let intent = mw_intent(Direction::Short, audcad_m());
        let err =
            Resolved::from_mw_intent(&intent, &shell_hlc(0.98430, 0.98300, 0.98400), &audcad_m());
        assert!(matches!(err, Err(ResolveError::InvalidGeometry)), "{err:?}");
    }

    #[test]
    fn m_high_inside_window_is_armed() {
        // high 0.98470 ≥ min_retrace 0.98458 and < cancel 0.98560 → armed.
        let intent = mw_intent(Direction::Short, audcad_m());
        let r =
            Resolved::from_mw_intent(&intent, &shell_hlc(0.98470, 0.98300, 0.98400), &audcad_m())
                .expect("bar inside window arms");
        assert!(matches!(r.entry, ResolvedEntry::Stop { .. }));
    }

    #[test]
    fn m_high_at_or_above_cancel_is_declined() {
        // high 0.98560 == cancel → past the 1.3 extension → decline (safety
        // net for the mw-cancel veto). Upper bound is exclusive.
        let intent = mw_intent(Direction::Short, audcad_m());
        let err =
            Resolved::from_mw_intent(&intent, &shell_hlc(0.98560, 0.98300, 0.98400), &audcad_m());
        assert!(matches!(err, Err(ResolveError::InvalidGeometry)), "{err:?}");
    }

    #[test]
    fn w_low_above_min_retrace_is_declined() {
        // W mirror: min_retrace = C + 0.7×(B−C), B < C so it's below C.
        //   neckline 1.1080, B 1.1000 → min_retrace 1.1024, cancel 1.0976.
        // A low of 1.1030 hasn't dropped to 1.1024 → decline.
        let intent = mw_intent(Direction::Long, w_params());
        let err =
            Resolved::from_mw_intent(&intent, &shell_hlc(1.1080, 1.1030, 1.1080), &w_params());
        assert!(matches!(err, Err(ResolveError::InvalidGeometry)), "{err:?}");
    }

    #[test]
    fn w_low_below_cancel_is_declined() {
        // low 1.0976 == cancel → past the 1.3 extension downward → decline.
        let intent = mw_intent(Direction::Long, w_params());
        let err =
            Resolved::from_mw_intent(&intent, &shell_hlc(1.1080, 1.0976, 1.1080), &w_params());
        assert!(matches!(err, Err(ResolveError::InvalidGeometry)), "{err:?}");
    }

    // ---- "Middle of the M" downward-cross trigger ----
    //
    // audcad_m: neckline 0.98339, peak 0.98509.
    //   right_tower = C + 0.7×(B−C) = 0.98458
    //   mid50       = C + 0.5×(B−C) = 0.98424
    //   cancel      = C + 1.3×(B−C) = 0.98560

    #[test]
    fn m_right_tower_confirmed_but_not_crossed_is_declined() {
        // high 0.98470 confirms the right tower (≥ 0.98458, < 0.98560), but
        // close 0.98450 is still ≥ mid50 0.98424 → price hasn't rolled back
        // through the middle of the M yet → decline, stay armed.
        let intent = mw_intent(Direction::Short, audcad_m());
        let err =
            Resolved::from_mw_intent(&intent, &shell_hlc(0.98470, 0.98300, 0.98450), &audcad_m());
        assert!(matches!(err, Err(ResolveError::InvalidGeometry)), "{err:?}");
    }

    #[test]
    fn m_crossed_middle_arms() {
        // high 0.98470 confirms the right tower; close 0.98400 < mid50 0.98424
        // → crossed down through the middle → armed.
        let intent = mw_intent(Direction::Short, audcad_m());
        let r =
            Resolved::from_mw_intent(&intent, &shell_hlc(0.98470, 0.98300, 0.98400), &audcad_m())
                .expect("right tower + downward cross arms");
        assert!(matches!(r.entry, ResolvedEntry::Stop { .. }));
    }

    #[test]
    fn m_close_at_mid50_is_declined() {
        // Boundary: close == mid50 0.98424 is not strictly below → not crossed.
        let intent = mw_intent(Direction::Short, audcad_m());
        let err =
            Resolved::from_mw_intent(&intent, &shell_hlc(0.98470, 0.98300, 0.98424), &audcad_m());
        assert!(matches!(err, Err(ResolveError::InvalidGeometry)), "{err:?}");
    }

    // W mirror: w_params neckline 1.1080, peak 1.1000.
    //   right_tower = 1.1024, mid50 = 1.1040, cancel = 1.0976.

    #[test]
    fn w_right_tower_confirmed_but_not_crossed_is_declined() {
        // low 1.1020 confirms the right trough (≤ 1.1024, > 1.0976), but close
        // 1.1030 is still ≤ mid50 1.1040 → hasn't crossed up through the middle
        // → decline, stay armed.
        let intent = mw_intent(Direction::Long, w_params());
        let err =
            Resolved::from_mw_intent(&intent, &shell_hlc(1.1080, 1.1020, 1.1030), &w_params());
        assert!(matches!(err, Err(ResolveError::InvalidGeometry)), "{err:?}");
    }

    #[test]
    fn w_crossed_middle_arms() {
        // low 1.1020 confirms the right trough; close 1.1080 > mid50 1.1040 →
        // crossed up through the middle; close < entry 1.10809 so the breakout
        // stop sits above the close → armed.
        let intent = mw_intent(Direction::Long, w_params());
        let r = Resolved::from_mw_intent(&intent, &shell_hlc(1.1080, 1.1020, 1.1080), &w_params())
            .expect("right trough + upward cross arms");
        assert!(matches!(r.entry, ResolvedEntry::Stop { .. }));
    }
}
