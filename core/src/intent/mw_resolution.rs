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
//! ## "Stay armed" semantics
//!
//! The enter alert fires every bar close. A bar whose close has *not*
//! yet broken the neckline (so the breakout stop would sit on the wrong
//! side of the current price) is declined here with
//! [`ResolveError::InvalidGeometry`]. Post the 2026-06 seen-id fix a
//! non-`Ok` resolve does **not** mark the intent id seen, so the next
//! bar's fire is allowed through — i.e. the setup stays armed until a
//! bar actually breaks out (or a cancel / abort / expiry veto ends it).

use super::resolution::{ResolveError, Resolved, ResolvedEntry};
use super::{Direction, Intent, MwParams, Shell};

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

    /// A bare shell at `close`; OHLC equal except the close (the M/W
    /// resolution only reads `close` for the stop-side check).
    fn shell_at(close: f64) -> Shell {
        Shell {
            close,
            high: close,
            low: close,
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
            reason: None,
            mw: Some(mw),
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
        let r = Resolved::from_mw_intent(&intent, &shell_at(1.1120), &m_params()).unwrap();
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
        let r = Resolved::from_mw_intent(&intent, &shell_at(1.1080), &w_params()).unwrap();
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
        let r = Resolved::from_mw_intent(&intent, &shell_at(1.1120), &mw).unwrap();
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
        let err = Resolved::from_mw_intent(&intent, &shell_at(1.1100), &m_params());
        assert!(matches!(err, Err(ResolveError::InvalidGeometry)), "{err:?}");
    }

    #[test]
    fn long_stop_on_wrong_side_is_declined() {
        let intent = mw_intent(Direction::Long, w_params());
        let err = Resolved::from_mw_intent(&intent, &shell_at(1.1090), &w_params());
        assert!(matches!(err, Err(ResolveError::InvalidGeometry)), "{err:?}");
    }
}
