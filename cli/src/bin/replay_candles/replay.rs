//! Drive the pure FSM over a candle window, one closed bar per tick.
//!
//! This mirrors what the worker's `run_engine_tick` does each cron fire —
//! seed-without-firing on the first tick, then feed newly-closed candles through
//! `evaluate_plan` and thread the advanced `PlanState` forward — but natively,
//! with no KV, no broker, and no worker runtime. Each fired intent is captured
//! together with the candles that followed it, so the caller can simulate the
//! fill.

use chrono::{DateTime, Utc};
use trade_control_engine::Candle as EngineCandle;
use trade_control_engine::{FiredIntent, PlanState, TradePlan, evaluate_plan, seed_plan_state};

/// One fired intent plus the forward candle path needed to simulate its fill.
pub struct Fire {
    pub fired: FiredIntent,
    /// Candles at or after the firing bar (ascending) — the simulator's input.
    pub forward: Vec<EngineCandle>,
}

/// The outcome of replaying a plan over a candle series.
pub struct Replay {
    pub fires: Vec<Fire>,
    pub final_state: PlanState,
    /// True if the plan reached its terminal `Done` phase during the window.
    pub done: bool,
    /// Distinct warnings surfaced by the evaluator (deduped, in first-seen order).
    pub warnings: Vec<String>,
}

/// Number of leading candles used to seed the FSM without firing. A small
/// fixed back-window matching the worker's `SEED_BARS`, enough for `OnClose`
/// rules to have a `last_close` reference before the first live tick.
const SEED_BARS: usize = 10;

/// Replay `plan` over `candles` (ascending, the pulled window). `expires_at`
/// stamps the state TTL on each tick (kept past the window so nothing expires
/// mid-replay).
pub fn run(plan: &TradePlan, candles: &[EngineCandle], expires_at: DateTime<Utc>) -> Replay {
    if candles.is_empty() {
        return Replay {
            fires: Vec::new(),
            final_state: seed_plan_state(plan, candles, expires_at),
            done: false,
            warnings: Vec::new(),
        };
    }

    let seed_end = SEED_BARS.min(candles.len());
    let mut state = seed_plan_state(plan, &candles[..seed_end], expires_at);

    let mut fires = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut done = false;

    // The detector window grows with each tick: Pine / trendline triggers need
    // the full back-window of closed candles, not just the single new bar.
    for i in seed_end..candles.len() {
        let new = &candles[i..=i];
        let detector_window = &candles[..=i];
        let now = candles[i].time;

        let eval = evaluate_plan(plan, &state, new, detector_window, now, expires_at);
        state = eval.new_state;

        for warning in eval.warnings {
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }

        for fired in eval.fired {
            // The fill simulator walks candles at/after the firing bar.
            let forward = candles[i..].to_vec();
            fires.push(Fire { fired, forward });
        }

        if eval.done {
            done = true;
            break;
        }
    }

    Replay {
        fires,
        final_state: state,
        done,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use trade_control_engine::Candle;

    fn candle(epoch: i64, c: f64) -> Candle {
        Candle {
            time: Utc.timestamp_opt(epoch, 0).unwrap(),
            o: c,
            h: c + 0.5,
            l: c - 0.5,
            c,
        }
    }

    // A plan with no rules never fires and never finishes; it just advances the
    // watermark across the window. Confirms the seed/loop wiring is sound
    // without needing geometry.
    #[test]
    fn empty_rule_plan_fires_nothing_and_advances() {
        let plan = TradePlan {
            trade_id: "t-empty".into(),
            instrument: "EUR_CAD".into(),
            direction: trade_control_engine::intent::Direction::Short,
            granularity: trade_control_engine::Granularity::H1,
            pip_size: 0.0001,
            rules: Vec::new(),
            shadow: false,
        };
        let candles: Vec<Candle> = (0..20)
            .map(|i| candle(i * 3600, 1.30 + i as f64 * 0.001))
            .collect();
        let expires = Utc.timestamp_opt(99 * 3600, 0).unwrap();

        let replay = run(&plan, &candles, expires);

        assert!(replay.fires.is_empty(), "no rules → no fires");
        assert!(!replay.done);
        assert_eq!(
            replay.final_state.watermark,
            Some(candles.last().unwrap().time)
        );
    }

    #[test]
    fn empty_candles_seed_only() {
        let plan = TradePlan {
            trade_id: "t".into(),
            instrument: "EUR_CAD".into(),
            direction: trade_control_engine::intent::Direction::Long,
            granularity: trade_control_engine::Granularity::H1,
            pip_size: 0.0001,
            rules: Vec::new(),
            shadow: false,
        };
        let replay = run(&plan, &[], Utc.timestamp_opt(0, 0).unwrap());
        assert!(replay.fires.is_empty());
    }
}
