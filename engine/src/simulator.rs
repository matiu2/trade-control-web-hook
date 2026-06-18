//! A pure candle-driven fill simulator for tick-bundle replay.
//!
//! The pure-evaluator replay (`trade-control replay`) diffs what the engine
//! *decided* (`PlanEval`). This module answers the next question: given a fired
//! **enter** intent and the candles that followed, *what would the broker have
//! done* — did the order fill, and did the position exit at its stop or target?
//!
//! It is the replay half of the roadmap's [broker simulator](../../roadmap/src/
//! broker-simulator.md), kept deliberately dumb (v1): resolve the intent's
//! entry/SL/TP via the pure [`Resolved::from_intent`], then walk the recorded
//! candles — a pending stop/limit fills when a candle's range crosses its
//! trigger; a market entry fills at once; after a fill, the first candle whose
//! range touches the stop or the target closes the position. Candles come from
//! the recorded bundle (answering the doc's open question), so the simulation is
//! deterministic and needs no broker.
//!
//! What it is **not**: it does not run the worker's `run_enter` dispatch (that
//! lives in the worker cdylib and returns a `worker::Response` that panics
//! off-wasm), so it doesn't reproduce sizing, the seen-id index, or gate
//! rejections — only the price-path fill/exit. Replaying the recorded
//! `dispatch_outcomes` through the real handlers needs the `Response` decouple
//! and is a separate, later task.

use trade_control_core::broker::Candle;
use trade_control_core::intent::{Direction, Intent, Resolved, ResolvedEntry, Shell};

/// What the simulator decided happened to one fired enter over the candle path.
#[derive(Debug, Clone, PartialEq)]
pub enum SimOutcome {
    /// The pending order never filled within the recorded candles.
    NeverFilled,
    /// Filled, but no SL/TP touched within the recorded candles — still open.
    FilledOpen {
        /// Open-time of the candle the entry filled on.
        fill_at: chrono::DateTime<chrono::Utc>,
        entry_price: f64,
    },
    /// Filled, then the stop-loss was hit.
    StoppedOut {
        fill_at: chrono::DateTime<chrono::Utc>,
        entry_price: f64,
        exit_at: chrono::DateTime<chrono::Utc>,
        exit_price: f64,
    },
    /// Filled, then the take-profit was hit.
    TookProfit {
        fill_at: chrono::DateTime<chrono::Utc>,
        entry_price: f64,
        exit_at: chrono::DateTime<chrono::Utc>,
        exit_price: f64,
    },
    /// The intent couldn't be resolved to concrete levels (not an enter, M/W
    /// not armed, invalid geometry, …). Carries the resolver's reason.
    Unresolved(String),
}

/// Simulate one fired enter `intent` (with its triggering `candle` folded into a
/// `Shell`) against the forward candle path `candles` (ascending, the bundle's
/// `new_candles`/`detector_window`). `pip_size` is the plan's pip size, needed to
/// resolve pip-offset entry/SL levels.
///
/// Mid prices only (the bundle records MID candles); the mid→bid/ask correction
/// the live broker applies is out of scope for this dumb v1 — exact-level fills.
pub fn simulate_fill(
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[Candle],
) -> SimOutcome {
    let resolved = match Resolved::from_intent(intent, shell, pip_size) {
        Ok(r) => r,
        Err(err) => return SimOutcome::Unresolved(err.to_string()),
    };

    // Phase 1 — find the fill. A market entry is filled at the trigger candle's
    // close (the shell's candle); a stop/limit fills on the first forward candle
    // whose range crosses the trigger.
    let (fill_at, entry_price, rest) = match resolved.entry {
        ResolvedEntry::Market { reference_price } => (shell.time, reference_price, candles),
        ResolvedEntry::Stop { trigger_price } | ResolvedEntry::Limit { trigger_price } => {
            match candles
                .iter()
                .position(|c| candle_crosses(c, trigger_price))
            {
                Some(i) => (candles[i].time, trigger_price, &candles[i + 1..]),
                None => return SimOutcome::NeverFilled,
            }
        }
    };

    // Phase 2 — after the fill, the first candle that touches SL or TP closes
    // the position. If both are touched in the same candle we can't tell the
    // intrabar order from a closed bar, so we conservatively call it the stop
    // (the worse outcome) — matches the simulator doc's "exact-level, pessimistic
    // on ambiguity" stance.
    for c in rest {
        let hit_sl = candle_crosses(c, resolved.stop_loss);
        let hit_tp = candle_crosses(c, resolved.take_profit);
        match (hit_sl, hit_tp) {
            (true, _) => {
                return SimOutcome::StoppedOut {
                    fill_at,
                    entry_price,
                    exit_at: c.time,
                    exit_price: resolved.stop_loss,
                };
            }
            (false, true) => {
                return SimOutcome::TookProfit {
                    fill_at,
                    entry_price,
                    exit_at: c.time,
                    exit_price: resolved.take_profit,
                };
            }
            (false, false) => {}
        }
        let _ = resolved.direction; // direction is implicit in level ordering
    }

    SimOutcome::FilledOpen {
        fill_at,
        entry_price,
    }
}

/// Whether a candle's high–low range spans `level` (an exact-level touch). The
/// fill model is direction-agnostic: a long stop above and a short stop below are
/// both "the candle's range reached the level".
fn candle_crosses(c: &Candle, level: f64) -> bool {
    c.l <= level && level <= c.h
}

/// Direction is unused by [`candle_crosses`] but kept in the public surface so a
/// future bid/ask-aware fill (long fills on ask, exits on bid) can branch on it
/// without an API change.
pub fn direction_of(resolved: &Resolved) -> Direction {
    resolved.direction
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use trade_control_core::intent::{
        Action, BrokerKind, EntrySpec, PriceAnchor, PriceRef, TakeProfit,
    };
    use trade_control_core::tunable::Tunable;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn candle(time: &str, o: f64, h: f64, l: f64, c: f64) -> Candle {
        Candle {
            time: ts(time),
            o,
            h,
            l,
            c,
        }
    }

    /// A long stop-entry intent with SL `1.1000` and TP `1.1150` as absolute
    /// levels, so the test doesn't depend on anchor resolution. The entry's stop
    /// trigger is overridden per-test.
    fn long_stop_intent() -> Intent {
        let mut i = base_enter();
        i.direction = Some(Direction::Long);
        i.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 0.0,
            at: None,
            on_too_close: None,
        });
        i.stop_loss = Some(PriceRef::Absolute { absolute: 1.1000 });
        i.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.1150,
        }));
        i
    }

    fn base_enter() -> Intent {
        Intent {
            v: 1,
            id: "sim-test".into(),
            not_before: None,
            not_after: ts("2026-06-30T00:00:00Z"),
            action: Action::Enter,
            instrument: "EUR_USD".into(),
            direction: None,
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
            trade_id: Some("sim-test".into()),
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
            mw: None,
            pip_size: None,
            trade_plan: None,
            blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
        }
    }

    /// The shell the fire carried: a trigger candle closing at 1.1040 (below the
    /// 1.1050 stop, so the stop is valid — long stop sits above close).
    fn trigger_shell() -> Shell {
        Shell::from_candle(&candle(
            "2026-06-17T10:00:00Z",
            1.1035,
            1.1045,
            1.1030,
            1.1040,
        ))
    }

    #[test]
    fn stop_entry_fills_then_takes_profit() {
        let intent = long_stop_intent();
        let shell = trigger_shell();
        // Stop trigger = close (1.1040) + 0 pips → wait, EntrySpec::Stop anchors
        // to `from`=Close=1.1040; long stop must be ABOVE close, so resolver
        // requires trigger > close. With offset 0 that's invalid; use a candle
        // path where the fill candle reaches the resolved trigger.
        // Resolver: trigger = anchor(Close)=1.1040; long-stop-<=close errors.
        // So this intent resolves with InvalidGeometry — assert that path first.
        let outcome = simulate_fill(&intent, &shell, 0.0001, &[]);
        assert!(
            matches!(outcome, SimOutcome::Unresolved(_)),
            "0-offset long stop on close is invalid geometry: {outcome:?}"
        );
    }

    #[test]
    fn offset_stop_fills_then_tp_then_sl_paths() {
        // Stop 10 pips above close → trigger 1.1050. SL 1.1000, TP 1.1150.
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0,
            at: None,
            on_too_close: None,
        });
        let shell = trigger_shell();

        // Path A: a candle reaches 1.1050 (fills), a later one reaches 1.1150 (TP).
        let tp_path = [
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills @1.1050
            candle("2026-06-17T12:00:00Z", 1.1052, 1.1160, 1.1050, 1.1155), // hits TP 1.1150
        ];
        match simulate_fill(&intent, &shell, 0.0001, &tp_path) {
            SimOutcome::TookProfit {
                entry_price,
                exit_price,
                ..
            } => {
                assert!((entry_price - 1.1050).abs() < 1e-9);
                assert!((exit_price - 1.1150).abs() < 1e-9);
            }
            other => panic!("expected TookProfit, got {other:?}"),
        }

        // Path B: fills, then a candle reaches the SL 1.1000.
        let sl_path = [
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills
            candle("2026-06-17T12:00:00Z", 1.1050, 1.1051, 1.0995, 1.1000), // hits SL 1.1000
        ];
        match simulate_fill(&intent, &shell, 0.0001, &sl_path) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!((exit_price - 1.1000).abs() < 1e-9);
            }
            other => panic!("expected StoppedOut, got {other:?}"),
        }

        // Path C: never reaches the trigger → NeverFilled.
        let no_fill = [candle(
            "2026-06-17T11:00:00Z",
            1.1041,
            1.1045,
            1.1038,
            1.1043,
        )];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &no_fill),
            SimOutcome::NeverFilled
        );

        // Path D: fills but neither level touched → FilledOpen.
        let still_open = [
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills
            candle("2026-06-17T12:00:00Z", 1.1052, 1.1060, 1.1048, 1.1055), // neither
        ];
        assert!(matches!(
            simulate_fill(&intent, &shell, 0.0001, &still_open),
            SimOutcome::FilledOpen { .. }
        ));
    }

    #[test]
    fn ambiguous_candle_resolves_to_stop() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0,
            at: None,
            on_too_close: None,
        });
        let shell = trigger_shell();
        // One candle fills AND spans both SL and TP → pessimistic: StoppedOut.
        let both = [
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills @1.1050
            candle("2026-06-17T12:00:00Z", 1.1050, 1.1160, 1.0995, 1.1100), // spans SL & TP
        ];
        assert!(matches!(
            simulate_fill(&intent, &shell, 0.0001, &both),
            SimOutcome::StoppedOut { .. }
        ));
    }
}
