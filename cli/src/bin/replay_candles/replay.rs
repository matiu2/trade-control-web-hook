//! Drive the pure FSM over a candle window, one closed bar per tick.
//!
//! This mirrors what the worker's `run_engine_tick` does each cron fire —
//! seed-without-firing on the first tick, then feed newly-closed candles through
//! `evaluate_plan` and thread the advanced `PlanState` forward — but natively,
//! with no KV, no broker, and no worker runtime. Each fired intent is captured
//! together with the candles that followed it, so the caller can simulate the
//! fill.

use chrono::{DateTime, Duration, Utc};
use trade_control_core::intent::{Action, Shell};
use trade_control_core::retry_gate::{self, RetryGateOutcome};
use trade_control_core::state::MemStateStore;
use trade_control_core::tunable::Tunable;
use trade_control_engine::BidAskCandle as EngineCandle;
use trade_control_engine::{
    Candle, FiredIntent, Granularity, PlanState, TradePlan, evaluate_plan, seed_plan_state,
};

use super::replay_broker::ReplayBroker;

/// One fired intent plus the forward candle path needed to simulate its fill.
pub struct Fire {
    pub fired: FiredIntent,
    /// Bid/ask candles at or after the firing bar (ascending) — the fill
    /// simulator's input (it fills each leg on the relevant book side).
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

/// Minimum number of leading candles used to seed the FSM without firing. A
/// small fixed back-window matching the worker's `SEED_BARS`, enough for
/// `OnClose` rules to have a `last_close` reference before the first live tick.
/// The actual seed span is usually larger — every warm-up bar before
/// `live_start` seeds silently (see [`run`]).
const SEED_BARS: usize = 10;

/// Replay `plan` over `candles` (ascending, the pulled window). `granularity`
/// is the bar size, used to derive each tick's `now` (a closed bar's close time
/// = its open time + one bar). `expires_at` stamps the state TTL on each tick
/// (kept past the window so nothing expires mid-replay).
///
/// `live_start` is the boundary between the **warm-up prefix** and the **live
/// window**: every candle whose open-time is `< live_start` only seeds the
/// detector / FSM state (warms ATR, gives patterns context, primes `last_close`)
/// and **fires nothing**. The plan goes live — vetos, preps, and the enter can
/// fire — on the first candle at or after `live_start`. This mirrors live
/// behaviour, where the plan is armed when the pattern forms and so cannot be
/// retired by a stale veto-level touch that happened earlier (which would
/// otherwise end the plan before the entry it exists to protect). The warm-up
/// bars are pulled by the caller (`--warmup-bars`).
pub async fn run(
    plan: &TradePlan,
    candles: &[EngineCandle],
    granularity: Granularity,
    live_start: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> Replay {
    // The engine evaluates on MID (matching the live worker, whose
    // `Broker::get_candles` is contractually mid); the bid/ask books are only
    // consulted by the fill simulator. Build the mid view once and feed it to
    // `evaluate_plan` / `seed_plan_state`; keep the bid/ask `candles` for the
    // broker + each fire's forward path.
    let mid: Vec<Candle> = candles.iter().map(|c| c.mid()).collect();

    if candles.is_empty() {
        return Replay {
            fires: Vec::new(),
            final_state: seed_plan_state(plan, &mid, expires_at),
            done: false,
            warnings: Vec::new(),
        };
    }

    let bar = Duration::seconds(granularity.seconds());

    // Seed every bar before `live_start` (the warm-up prefix), so the detector
    // and `last_close` are warm but nothing fires. Floor at `SEED_BARS` (so a
    // window starting exactly at `live_start` still gets a minimal seed) and cap
    // at `len - 1` so at least one live bar remains to evaluate.
    let warmup_end = candles.partition_point(|c| c.time < live_start);
    let last_live_floor = candles.len() - 1; // candles is non-empty here
    let seed_end = warmup_end
        .max(SEED_BARS.min(candles.len()))
        .min(last_live_floor);
    let mut state = seed_plan_state(plan, &mid[..seed_end], expires_at);

    let mut fires = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut done = false;

    // Multi-shot plumbing: the SAME async retry gate the worker runs, backed by
    // an offline `ReplayBroker` (resolves a prior attempt's state by simulating
    // it against the candle window up to the asking bar) and an in-memory store.
    // The engine fix (multi-shot enter stays AwaitEntry) lets the loop reach the
    // re-entry bars; the gate decides whether each enter fire actually places
    // (Proceed) or is blocked (a prior attempt still open / the cap reached) —
    // exactly as the live worker would. Single-shot enters never enter this gate
    // (the engine retires them after one fire).
    let replay_broker = ReplayBroker::new(candles.to_vec(), plan.pip_size);
    let store = MemStateStore::default();

    // The detector window grows with each tick: Pine / trendline triggers need
    // the full back-window of closed candles, not just the single new bar.
    for i in seed_end..candles.len() {
        // The engine sees MID; the fill simulator's forward path stays bid/ask.
        let new = &mid[i..=i];
        let detector_window = &mid[..=i];
        // Candle timestamps are bar *open* times; the live cron tick that first
        // observes this closed bar fires at (or after) its *close*. Use the
        // close time as `now` so wall-clock-derived state (TTLs, logging) lines
        // up with the live worker, which ticks on wall-clock, not bar-open.
        let now = candles[i].time + bar;

        tracing::debug!(
            bar = %candles[i].time,
            phase = ?state.phase,
            o = mid[i].o,
            h = mid[i].h,
            l = mid[i].l,
            c = mid[i].c,
            "tick: evaluating live bar"
        );

        let eval = evaluate_plan(plan, &state, new, detector_window, now, expires_at);
        state = eval.new_state;

        for warning in eval.warnings {
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }

        for fired in eval.fired {
            tracing::debug!(
                bar = %candles[i].time,
                rule = %fired.rule_id,
                action = ?fired.intent.action,
                "tick: rule fired"
            );
            // An enter fire on a multi-shot plan must clear the retry gate before
            // it counts as a placement; any other fire (veto/prep, or a
            // single-shot enter) records straight through.
            if fired.intent.action == Action::Enter && is_multi_shot(&fired.intent.max_retries) {
                match gate_enter(&replay_broker, &store, &fired, now, plan.pip_size).await {
                    Some(()) => {}    // Proceed — record the placement.
                    None => continue, // Rejected (open / cap) — no placement.
                }
            }
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

/// Is this enter opted into multi-shot (any non-default `max_retries`)? Mirrors
/// the worker's gate-entry check in `run_enter`.
fn is_multi_shot(max_retries: &Tunable<u32>) -> bool {
    !matches!(max_retries, Tunable::Static(0))
}

/// Run one multi-shot enter fire through the shared retry gate, backed by the
/// offline broker. On `Proceed`, register the placement with the broker (so a
/// later re-entry's gate sees this attempt) and the store, and return `Some(())`
/// to record the fire. On any `Rejected` (a prior attempt still open, the cap
/// reached, …) return `None` so the loop skips it — no placement happened.
async fn gate_enter(
    broker: &ReplayBroker,
    store: &MemStateStore,
    fired: &FiredIntent,
    now: DateTime<Utc>,
    pip_size: f64,
) -> Option<()> {
    let intent = &fired.intent;
    // Reconstruct the firing shell exactly as the worker's dispatch would, so the
    // gate's `max_retries` script (if any) resolves against the same anchors.
    let shell = match &fired.signal {
        Some(sig) => Shell::from_candle_and_signal(&fired.candle, sig),
        None => Shell::from_candle(&fired.candle),
    };

    // Point the broker's prior-attempt resolution at this bar (time-accurate).
    broker.set_as_of(fired.candle.time);

    match retry_gate::evaluate(broker, store, intent, &shell).await {
        RetryGateOutcome::Proceed { next_attempt_no } => {
            let order_id = format!("{}-{next_attempt_no}", intent.id);
            // Register with the broker so the next re-entry's gate can resolve
            // this attempt's state from the candles.
            broker.record_attempt(order_id.clone(), intent.clone(), shell.clone());
            // Persist the EntryAttempt + retry-fire-seen mark via the SAME
            // `record_placement` the worker uses, so the cap counts correctly.
            let resolved_sl =
                trade_control_core::intent::Resolved::from_intent(intent, &shell, pip_size)
                    .ok()
                    .map(|r| r.stop_loss)
                    .unwrap_or(0.0);
            retry_gate::record_placement(
                store,
                intent,
                shell.time,
                intent.not_after,
                now,
                next_attempt_no,
                &order_id,
                intent
                    .direction
                    .unwrap_or(trade_control_core::intent::Direction::Long),
                resolved_sl,
                None,
            )
            .await;
            Some(())
        }
        RetryGateOutcome::Rejected { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A bid==ask==mid bar (zero spread): the engine sees the mid OHLC, the fill
    /// simulator's books equal it, so these wiring tests need no spread.
    fn candle(epoch: i64, c: f64) -> EngineCandle {
        let (o, h, l) = (c, c + 0.5, c - 0.5);
        EngineCandle {
            time: Utc.timestamp_opt(epoch, 0).unwrap(),
            o,
            h,
            l,
            c,
            bid_o: o,
            bid_h: h,
            bid_l: l,
            bid_c: c,
            ask_o: o,
            ask_h: h,
            ask_l: l,
            ask_c: c,
        }
    }

    /// A `live_start` before any candle — the whole window is live (no silent
    /// warm-up prefix), matching the pre-warm-up `run` behaviour these tests
    /// were written against.
    fn all_live() -> DateTime<Utc> {
        Utc.timestamp_opt(0, 0).unwrap()
    }

    // A plan with no rules never fires and never finishes; it just advances the
    // watermark across the window. Confirms the seed/loop wiring is sound
    // without needing geometry.
    #[tokio::test]
    async fn empty_rule_plan_fires_nothing_and_advances() {
        let plan = TradePlan {
            trade_id: "t-empty".into(),
            instrument: "EUR_CAD".into(),
            direction: trade_control_engine::intent::Direction::Short,
            granularity: trade_control_engine::Granularity::H1,
            pip_size: 0.0001,
            rules: Vec::new(),
            shadow: false,
        };
        let candles: Vec<EngineCandle> = (0..20)
            .map(|i| candle(i * 3600, 1.30 + i as f64 * 0.001))
            .collect();
        let expires = Utc.timestamp_opt(99 * 3600, 0).unwrap();

        let replay = run(&plan, &candles, Granularity::H1, all_live(), expires).await;

        assert!(replay.fires.is_empty(), "no rules → no fires");
        assert!(!replay.done);
        assert_eq!(
            replay.final_state.watermark,
            Some(candles.last().unwrap().time)
        );
    }

    #[tokio::test]
    async fn empty_candles_seed_only() {
        let plan = TradePlan {
            trade_id: "t".into(),
            instrument: "EUR_CAD".into(),
            direction: trade_control_engine::intent::Direction::Long,
            granularity: trade_control_engine::Granularity::H1,
            pip_size: 0.0001,
            rules: Vec::new(),
            shadow: false,
        };
        let replay = run(&plan, &[], Granularity::H1, all_live(), all_live()).await;
        assert!(replay.fires.is_empty());
    }

    /// A trade-expiry `TimeReached` fires on the first candle whose *open* time
    /// is at-or-past the expiry epoch (the engine evaluates `TimeReached`
    /// against `candle.time`, the bar open). So the window must include a candle
    /// opening at-or-after the expiry — one bar past it — for the plan to
    /// finish. This is the NZD/CHF boundary case: pulling only up to the expiry
    /// left the last bar opening one bar *short*, so expiry never fired.
    #[tokio::test]
    async fn trade_expiry_fires_when_a_bar_opens_at_expiry() {
        let expiry = 15 * 3600;
        let candles: Vec<EngineCandle> = (0..=15).map(|i| candle(i * 3600, 1.30)).collect();
        // The last bar opens at exactly `expiry` → `candle.time >= expiry`.
        assert_eq!(candles.last().unwrap().time.timestamp(), expiry);
        let replay = run(
            &expiry_plan(expiry),
            &candles,
            Granularity::H1,
            all_live(),
            expires(),
        )
        .await;
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
    #[tokio::test]
    async fn trade_expiry_does_not_fire_a_bar_short() {
        let expiry = 15 * 3600;
        let candles: Vec<EngineCandle> = (0..15).map(|i| candle(i * 3600, 1.30)).collect();
        assert!(candles.last().unwrap().time.timestamp() < expiry);
        let replay = run(
            &expiry_plan(expiry),
            &candles,
            Granularity::H1,
            all_live(),
            expires(),
        )
        .await;
        assert!(!replay.done);
        assert!(replay.fires.is_empty());
    }

    fn expires() -> DateTime<Utc> {
        Utc.timestamp_opt(99 * 3600, 0).unwrap()
    }

    /// A plan with a single intrabar `too-high` veto at `level` (crosses up).
    fn too_high_plan(level: f64) -> TradePlan {
        serde_json::from_str(&format!(
            r#"{{
                "trade_id": "t-th",
                "instrument": "EUR_USD",
                "direction": "short",
                "granularity": "h1",
                "pip_size": 0.0001,
                "rules": [{{
                    "rule_id": "01-veto-too-high",
                    "trigger": {{ "type": "horizontal_cross", "level": {level}, "dir": "up", "bar": "intrabar" }},
                    "fire_mode": "once",
                    "intent": {{
                        "v": 1,
                        "id": "th-intent",
                        "not_after": "2099-01-01T00:00:00Z",
                        "action": "veto",
                        "instrument": "EUR_USD"
                    }}
                }}]
            }}"#
        ))
        .expect("parse plan")
    }

    /// A candle with explicit OHLC (so we can place a wick across a level).
    /// A bid==ask==mid bar with explicit OHLC (zero spread).
    fn ohlc(epoch: i64, o: f64, h: f64, l: f64, c: f64) -> EngineCandle {
        EngineCandle {
            time: Utc.timestamp_opt(epoch, 0).unwrap(),
            o,
            h,
            l,
            c,
            bid_o: o,
            bid_h: h,
            bid_l: l,
            bid_c: c,
            ask_o: o,
            ask_h: h,
            ask_l: l,
            ask_c: c,
        }
    }

    /// The core warm-up guarantee: a veto level breached only in the **warm-up
    /// prefix** (before `live_start`) must NOT fire — those bars seed silently.
    /// A breach in the **live** window fires. Proves the live boundary, not the
    /// geometry, gates the veto.
    #[tokio::test]
    async fn warmup_prefix_does_not_fire_a_veto_touched_before_live_start() {
        let level = 1.30;
        // 40 bars below the level, except two close-confirmed up-crosses:
        // bar 5 (warm-up) and bar 25 (live, well past the SEED_BARS floor).
        let mut candles: Vec<EngineCandle> = (0..40)
            .map(|i| ohlc(i * 3600, 1.20, 1.21, 1.19, 1.205))
            .collect();
        candles[5] = ohlc(5 * 3600, 1.29, 1.31, 1.28, 1.305); // warm-up breach
        candles[25] = ohlc(25 * 3600, 1.29, 1.31, 1.28, 1.305); // live breach

        // live_start at bar 20 → bar 5 is warm-up (silent), bar 25 is live.
        let live_at = Utc.timestamp_opt(20 * 3600, 0).unwrap();
        let r = run(
            &too_high_plan(level),
            &candles,
            Granularity::H1,
            live_at,
            expires(),
        )
        .await;
        assert_eq!(
            r.fires.len(),
            1,
            "only the live breach (bar 25) should fire"
        );
        assert_eq!(r.fires[0].fired.rule_id, "01-veto-too-high");
        // The fire is the live one, not the warm-up one: its candle is bar 25.
        assert_eq!(
            r.fires[0].forward.first().map(|c| c.time),
            Some(candles[25].time),
            "the fire must be the live breach at bar 25, not the warm-up breach at bar 5"
        );
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
