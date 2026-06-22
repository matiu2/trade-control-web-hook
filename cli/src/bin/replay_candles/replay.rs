//! Drive the pure FSM over a candle window, one closed bar per tick.
//!
//! This mirrors what the worker's `run_engine_tick` does each cron fire —
//! seed-without-firing on the first tick, then feed newly-closed candles through
//! `evaluate_plan` and thread the advanced `PlanState` forward — but natively,
//! with no KV, no broker, and no worker runtime. Each fired intent is captured
//! together with the candles that followed it, so the caller can simulate the
//! fill.

use chrono::{DateTime, Duration, Utc};
use trade_control_engine::Candle as EngineCandle;
use trade_control_engine::{
    FiredIntent, Granularity, PlanState, TradePlan, evaluate_plan, seed_plan_state,
};

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

/// Replay `plan` over `candles` (ascending, the pulled window). `granularity`
/// is the bar size, used to derive each tick's `now` (a closed bar's close time
/// = its open time + one bar). `expires_at` stamps the state TTL on each tick
/// (kept past the window so nothing expires mid-replay).
pub fn run(
    plan: &TradePlan,
    candles: &[EngineCandle],
    granularity: Granularity,
    expires_at: DateTime<Utc>,
) -> Replay {
    if candles.is_empty() {
        return Replay {
            fires: Vec::new(),
            final_state: seed_plan_state(plan, candles, expires_at),
            done: false,
            warnings: Vec::new(),
        };
    }

    let bar = Duration::seconds(granularity.seconds());

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
        // Candle timestamps are bar *open* times; the live cron tick that first
        // observes this closed bar fires at (or after) its *close*. Use the
        // close time as `now` so wall-clock-derived state (TTLs, logging) lines
        // up with the live worker, which ticks on wall-clock, not bar-open.
        let now = candles[i].time + bar;

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

        let replay = run(&plan, &candles, Granularity::H1, expires);

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
        let replay = run(
            &plan,
            &[],
            Granularity::H1,
            Utc.timestamp_opt(0, 0).unwrap(),
        );
        assert!(replay.fires.is_empty());
    }

    /// A trade-expiry `TimeReached` fires on the first candle whose *open* time
    /// is at-or-past the expiry epoch (the engine evaluates `TimeReached`
    /// against `candle.time`, the bar open). So the window must include a candle
    /// opening at-or-after the expiry — one bar past it — for the plan to
    /// finish. This is the NZD/CHF boundary case: pulling only up to the expiry
    /// left the last bar opening one bar *short*, so expiry never fired.
    #[test]
    fn trade_expiry_fires_when_a_bar_opens_at_expiry() {
        let expiry = 15 * 3600;
        let candles: Vec<Candle> = (0..=15).map(|i| candle(i * 3600, 1.30)).collect();
        // The last bar opens at exactly `expiry` → `candle.time >= expiry`.
        assert_eq!(candles.last().unwrap().time.timestamp(), expiry);
        let replay = run(&expiry_plan(expiry), &candles, Granularity::H1, expires());
        assert!(
            replay.done,
            "a bar opening at the expiry should finish the plan"
        );
        assert_eq!(replay.fires.len(), 1);
        assert_eq!(replay.fires[0].fired.rule_id, "02-veto-trade-expiry");
    }

    /// The converse: when the window stops one bar *short* of the expiry (last
    /// bar opens before it), nothing fires — which is exactly why the window
    /// resolver extends the pull end past the plan's trade-expiry.
    #[test]
    fn trade_expiry_does_not_fire_a_bar_short() {
        let expiry = 15 * 3600;
        let candles: Vec<Candle> = (0..15).map(|i| candle(i * 3600, 1.30)).collect();
        assert!(candles.last().unwrap().time.timestamp() < expiry);
        let replay = run(&expiry_plan(expiry), &candles, Granularity::H1, expires());
        assert!(!replay.done);
        assert!(replay.fires.is_empty());
    }

    fn expires() -> DateTime<Utc> {
        Utc.timestamp_opt(99 * 3600, 0).unwrap()
    }

    /// A plan carrying a single `02-veto-trade-expiry` `TimeReached` rule.
    fn expiry_plan(at_epoch: i64) -> TradePlan {
        serde_json::from_str(&format!(
            r#"{{
                "trade_id": "t-expiry",
                "instrument": "EUR_USD",
                "direction": "short",
                "granularity": "h1",
                "pip_size": 0.0001,
                "rules": [{{
                    "rule_id": "02-veto-trade-expiry",
                    "trigger": {{ "type": "time_reached", "at_epoch": {at_epoch} }},
                    "fire_mode": "once",
                    "intent": {{
                        "v": 1,
                        "id": "expiry-intent",
                        "not_after": "2099-01-01T00:00:00Z",
                        "action": "veto",
                        "instrument": "EUR_USD"
                    }}
                }}]
            }}"#
        ))
        .expect("parse plan")
    }
}
