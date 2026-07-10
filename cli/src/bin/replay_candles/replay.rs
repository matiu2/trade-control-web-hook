//! Drive the pure FSM over a candle window, one closed bar per tick.
//!
//! This mirrors what the worker's `run_engine_tick` does each cron fire —
//! seed-without-firing on the first tick, then feed newly-closed candles through
//! `evaluate_plan` and thread the advanced `PlanState` forward — but natively,
//! with no KV, no broker, and no worker runtime. Each fired intent is captured
//! together with the candles that followed it, so the caller can simulate the
//! fill.

use chrono::{DateTime, Duration, Utc};
use trade_control_core::broker::Broker;
use trade_control_core::dispatch::{self, ActionResult};
use trade_control_core::dispatch_config::DispatchConfig;
use trade_control_core::incoming::Verified;
use trade_control_core::intent::{Action, Intent as TcIntent, Shell};
use trade_control_core::pause_gate;
use trade_control_core::state::MemStateStore;
use trade_control_engine::BidAskCandle as EngineCandle;
use trade_control_engine::{
    Candle, FiredIntent, Granularity, PlanState, TradePlan, Trigger, evaluate_controls_only,
    evaluate_plan, seed_plan_state,
};

use super::replay_broker::ReplayBroker;
use super::verbose::BarTrace;

/// The entry decision the **real** dispatch (`trade_control_core::dispatch::run_enter`)
/// reached for one fired `enter`, carried on the [`Fire`] so the report reads it
/// without re-deriving any gate. This is the whole point of routing the replay
/// through `run_enter`: the offline entry decision == the live worker's, so the
/// replay can't drift from the engine
/// (`[[strategy_changes_in_both_replayer_and_worker]]`).
#[derive(Debug, Clone)]
pub enum EnterGateOutcome {
    /// `run_enter` accepted + placed (`ActionResult::Ok`). `order_id` is what the
    /// broker handed back (the `{intent.id}-{attempt_no}` id for a multi-shot
    /// enter), used to correlate the gate's cancel-and-replace back to this fire.
    /// `None` for a placement that recorded no broker id (e.g. a dry-run enter).
    Placed { order_id: Option<String> },
    /// `run_enter` rejected before placing (`ActionResult::Rejected`) — a 0R
    /// skip. `reason` is the real dispatch outcome string (e.g.
    /// `rejected: cooled-down`, `rejected: veto-active (too-low)`,
    /// `rejected: paused [...]`, `rejected: missing-prep (...)`).
    Rejected { reason: String },
    /// This fire isn't an enter (a veto / prep / pause / resume), so no entry
    /// gate ran. The report renders these from the intent action alone.
    NotAnEnter,
}

/// One fired intent plus the forward candle path needed to simulate its fill.
pub struct Fire {
    pub fired: FiredIntent,
    /// Bid/ask candles at or after the firing bar (ascending) — the fill
    /// simulator's input (it fills each leg on the relevant book side).
    pub forward: Vec<EngineCandle>,
    /// The unified entry decision the real `run_enter` reached for this fire.
    /// For an enter it's `Placed` / `Rejected`; for anything else `NotAnEnter`.
    /// The report reads this directly instead of re-deriving any gate.
    pub gate_outcome: EnterGateOutcome,
    /// Set when a **later** enter superseded this fire's still-resting order
    /// (the gate's cancel-and-replace path). A superseded order was cancelled
    /// before it could fill, so the report must show it as cancelled — not its
    /// standalone simulated fill. The faithful model: a new entry cancels any
    /// resting sibling/prior order on the same setup.
    pub superseded: bool,
    /// The MEAN bid-ask spread over the trailing `spread_window` bars at the
    /// fire bar, fetched through the SAME `Broker::get_bidask_candles` provider
    /// (on the `ReplayBroker`) that `run_enter`'s gate used to place the stop.
    /// The report's displayed bracket + simulated exit + System-2 baseline all
    /// floor off THIS, so they match the gate's placed stop instead of the
    /// single fire-bar spread. `None` when unavailable (falls back to the fire
    /// bar's own close spread — the pre-window behaviour).
    pub entry_spread_price: Option<f64>,
}

impl Fire {
    /// The placed broker order id, when this fire was an accepted enter — the
    /// key `mark_superseded` correlates the gate's cancel-and-replace against.
    /// `None` for a rejected enter, a non-enter, or a placement with no id.
    pub fn order_id(&self) -> Option<&str> {
        match &self.gate_outcome {
            EnterGateOutcome::Placed { order_id } => order_id.as_deref(),
            _ => None,
        }
    }

    /// The 0R-skip reason when this enter was rejected by `run_enter`, else
    /// `None`. The report surfaces it in place of a simulated fill.
    pub fn rejected_reason(&self) -> Option<&str> {
        match &self.gate_outcome {
            EnterGateOutcome::Rejected { reason } => Some(reason.as_str()),
            _ => None,
        }
    }

    /// The active news-blackout ids when this enter was rejected by the pause
    /// gate (`rejected: paused [a,b]`), else an empty Vec. Lets the fixture
    /// snapshot freeze a news-blackout SKIP as a regression, the way the old
    /// `suppressed_by` field did, without storing a second field.
    pub fn suppressed_by(&self) -> Vec<String> {
        let Some(reason) = self.rejected_reason() else {
            return Vec::new();
        };
        // Format from `run_enter`: `rejected: paused [id1,id2(reason)]`.
        let Some(inner) = reason
            .strip_prefix("rejected: paused [")
            .and_then(|s| s.strip_suffix(']'))
        else {
            return Vec::new();
        };
        if inner.is_empty() {
            return Vec::new();
        }
        inner.split(',').map(|s| s.trim().to_string()).collect()
    }
}

/// The outcome of replaying a plan over a candle series.
pub struct Replay {
    pub fires: Vec<Fire>,
    pub final_state: PlanState,
    /// True if the plan reached its terminal `Done` phase during the window.
    pub done: bool,
    /// Distinct warnings surfaced by the evaluator (deduped, in first-seen order).
    pub warnings: Vec<String>,
    /// Per-live-bar state-delta trace for `--verbose`: what the engine changed
    /// each tick (phase, the break-and-close / retest stamps, fired rules) — the
    /// silent state moves the fire report can't show. One entry per evaluated
    /// live bar, in order; quiet bars are kept here and skipped at render time.
    pub traces: Vec<BarTrace>,
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
            traces: Vec::new(),
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
    let mut traces: Vec<BarTrace> = Vec::new();
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

        // Sub-bar control ticks: before this bar's close, replay any pause/news
        // window edge whose wall-clock epoch fell *inside* the just-elapsed bar
        // (a 14:30 event on an H1 chart, between the 14:00 and 15:00 closes). The
        // live worker's 5s cron opens/closes it there; the replay's virtual clock
        // reproduces that exactly by running the SAME `evaluate_controls_only` at
        // each such epoch. `i >= seed_end >= 1`, so `i - 1` is always valid — the
        // prior close and the last-closed bar (`mid[i - 1]`) are the shell.
        let prev_close = candles[i - 1].time + bar;
        inject_control_ticks(
            plan,
            &mut state,
            &store,
            &mut fires,
            &mid[i - 1],
            &candles[i..],
            prev_close,
            now,
            expires_at,
        )
        .await;

        // Pin the store's clock to this tick so historically-dated pause TTLs
        // aren't judged "expired" against real wall-clock — the same
        // wall-clock-vs-cursor trap that drops blackout state in replay.
        store.set_clock(now);

        tracing::debug!(
            bar = %candles[i].time,
            phase = ?state.phase,
            o = mid[i].o,
            h = mid[i].h,
            l = mid[i].l,
            c = mid[i].c,
            "tick: evaluating live bar"
        );

        // Snapshot the pre-tick state so `--verbose` can report exactly what the
        // engine changed this bar — phase moves and the break-and-close / retest
        // stamps are silent in the fire report (retest never even fires an
        // intent). A cheap clone; the diff is a pure `PlanState` comparison.
        let before = state.clone();

        let eval = evaluate_plan(plan, &state, new, detector_window, now, expires_at);
        state = eval.new_state;

        let fired_rules: Vec<String> = eval.fired.iter().map(|f| f.rule_id.clone()).collect();
        traces.push(BarTrace::diff(
            candles[i].time,
            &before,
            &state,
            fired_rules,
        ));

        for warning in eval.warnings {
            if !warnings.contains(&warning) {
                // Trendline-anchor diagnostics are recomputed every tick against a
                // one-bar-wider window, so the same underlying condition produces a
                // fresh string each bar (…153-bar…, …154-bar…). They're low-signal
                // for a normal replay, so keep them out of the console report and
                // emit them at debug level (RUST_LOG=debug) instead. Still recorded
                // on the structured fixture for anyone who wants them.
                tracing::debug!(target: "replay::trendline_anchor", "{warning}");
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
            // News-blackout control fires set/clear pause state in the SAME
            // store `run_enter`'s pause gate reads, via the SAME core helpers the
            // worker uses (so the replay can't drift from live). A pause/resume
            // isn't an enter — these seed the store state the enter gate then
            // consults; record them through as `NotAnEnter` fires afterwards.
            match fired.intent.action {
                Action::Pause => {
                    if let Err(e) = pause_gate::apply_pause(&store, &fired.intent, now).await {
                        tracing::error!(rule = %fired.rule_id, error = %e, "apply_pause failed");
                    }
                }
                Action::Resume => {
                    if let Err(e) = pause_gate::apply_resume(&store, &fired.intent).await {
                        tracing::error!(rule = %fired.rule_id, error = %e, "apply_resume failed");
                    }
                }
                // A prep fire seeds the store the enter's prep gate then reads —
                // exactly as the worker's `dispatch_action` routes `Action::Prep`
                // through `handle_prep` (→ `store.set_prep`). Before this, the
                // replay dropped prep fires (`_ => {}`), so `run_enter`'s prep
                // gate always saw `None` and rejected every preps-gated enter
                // with `missing-prep` — a silent divergence introduced when the
                // enter decision moved to the real `run_enter` (commit 1c0a043).
                // The break-and-close prep is emitted by the engine; the retest
                // prep is emitted by `stamp_retest` (engine) the bar it stamps.
                Action::Prep => {
                    let verified = Verified {
                        shell: Shell::from_candle(&fired.candle),
                        intent: fired.intent.clone(),
                    };
                    let result = dispatch::handle_prep(&store, &verified, now).await;
                    if !result.is_success() {
                        tracing::error!(
                            rule = %fired.rule_id,
                            status = result.status,
                            "handle_prep rejected in replay"
                        );
                    }
                }
                _ => {}
            }

            // Decide the entry ONCE, here, via the REAL dispatch. For an enter
            // fire we call `trade_control_core::dispatch::run_enter` — the exact
            // function the live worker runs — so the offline entry decision is
            // the live engine's: every gate (pause, retry/multi-shot, cooldown,
            // prep, veto, entry-level veto, allow_entry, market/spread blackout,
            // SL-floor) is applied identically, and the report reads the verdict
            // off the fire instead of re-deriving any of them. Non-enters carry
            // `NotAnEnter`.
            let gate_outcome = if fired.intent.action == Action::Enter {
                dispatch_enter(
                    &replay_broker,
                    &store,
                    &fired,
                    &plan_pip(plan),
                    now,
                    plan.granularity,
                )
                .await
            } else {
                EnterGateOutcome::NotAnEnter
            };

            // A `run_enter` placement may have cancelled a prior resting order to
            // replace it (cancel-and-replace, via the retry gate). Stamp any
            // already-recorded enter fire whose order the broker cancelled so the
            // report shows it superseded instead of its standalone simulated fill.
            if matches!(gate_outcome, EnterGateOutcome::Placed { .. }) {
                mark_superseded(&mut fires, &replay_broker);
            }

            // The fill simulator walks candles at/after the firing bar.
            let forward = candles[i..].to_vec();
            // Capture the entry-spread window through the SAME provider the gate
            // used (`ReplayBroker::get_bidask_candles`, bounded at the fire bar),
            // so the report's bracket/exit floor off the identical statistic the
            // gate placed the stop with. Only meaningful for an enter.
            let entry_spread_price = if fired.intent.action == Action::Enter {
                entry_spread_for(&replay_broker, &fired.intent, granularity, now).await
            } else {
                None
            };
            fires.push(Fire {
                fired,
                forward,
                gate_outcome,
                superseded: false,
                entry_spread_price,
            });
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
        traces,
    }
}

/// The mean bid-ask spread over the trailing `spread_window` bars at the fire
/// bar, read through the SAME `Broker::get_bidask_candles` provider the gate
/// used — so the report's displayed/simulated floor matches the gate's placed
/// stop. Mirrors the gate's count-back window (`window + 2` bars of slack) and
/// reduces with the shared `trailing_spread_mean`. `None` when unavailable (the
/// report then falls back to the fire bar's own close spread).
async fn entry_spread_for(
    broker: &ReplayBroker,
    intent: &TcIntent,
    granularity: Granularity,
    now: DateTime<Utc>,
) -> Option<f64> {
    let window = intent
        .spread_window
        .unwrap_or(trade_control_core::intent::DEFAULT_SPREAD_WINDOW)
        .max(1);
    let lookback_bars = (window as i64) + 2;
    let since = now - Duration::seconds(granularity.seconds() * lookback_bars);
    let candles = broker
        .get_bidask_candles(&intent.instrument, granularity, since, now)
        .await
        .ok()?;
    trade_control_core::broker::trailing_spread_mean(&candles, window).map(|(mean, _)| mean)
}

/// The pip size every resolution in the replay uses — the plan's baked value.
/// `run_enter` prefers the intent's own `pip_size` and falls back to this, so a
/// `DispatchConfig` built with `plan.pip_size` matches what the worker resolves.
fn plan_pip(plan: &TradePlan) -> f64 {
    plan.pip_size
}

/// Dispatch one fired `enter` through the REAL `run_enter`, returning the
/// unified [`EnterGateOutcome`] the report reads. This is the single decision
/// point: `run_enter` applies every entry gate (pause/retry/cooldown/prep/veto/
/// entry-level-veto/allow_entry/blackouts/SL-floor) exactly as the live worker
/// does, so the offline verdict can't drift from the engine.
///
/// The offline `ReplayBroker` needs the intent + shell to resolve a prior
/// attempt's later state, but `run_enter` calls `broker.place_entry` with only an
/// `EntryRequest`. So we arm the broker with the firing geometry + a unique order
/// id first; `place_entry` consumes it and hands the id back, which `run_enter`
/// records on the `EntryAttempt`. `set_as_of` points the broker's prior-attempt
/// resolution at this bar (time-accurate, for the multi-shot retry gate).
async fn dispatch_enter(
    broker: &ReplayBroker,
    store: &MemStateStore,
    fired: &FiredIntent,
    pip_size: &f64,
    now: DateTime<Utc>,
    granularity: Granularity,
) -> EnterGateOutcome {
    let intent = &fired.intent;
    // Reconstruct the firing shell exactly as the worker's dispatch would (an
    // H&S Pine fire folds its latched signal; a stop/limit enter has none), so
    // every gate + resolution sees the same anchors.
    let shell = match &fired.signal {
        Some(sig) => Shell::from_candle_and_signal(&fired.candle, sig),
        None => Shell::from_candle(&fired.candle),
    };

    // Point the broker's prior-attempt resolution at this bar (time-accurate),
    // and arm the placement the dispatch will reach for. The order id is unique
    // per fire (`{intent.id}-{shell.time}`) so the retry gate correlates this
    // attempt against later re-entries.
    broker.set_as_of(fired.candle.time);
    let order_id = format!("{}-{}", intent.id, shell.time.timestamp());
    broker.arm_placement(order_id.clone(), intent.clone(), shell.clone());

    let verified = Verified {
        shell,
        intent: intent.clone(),
    };
    // The replay isn't risk-limiting — these are the worker defaults
    // (`MAX_RISK_PCT_PER_TRADE` / `MAX_OPEN_POSITIONS`). `pip_size` is the plan's
    // baked value (the intent's own `pip_size` still takes precedence inside
    // `run_enter`); `caps` default to no per-account narrowing.
    let cfg = DispatchConfig {
        worker_max_risk_pct: 1.0,
        worker_max_open_positions: 3,
        pip_size: *pip_size,
        // No edge-resolved tick in replay; the baked `Intent::tick_size` takes
        // precedence inside `run_enter`, falling back to `pip_size` when absent
        // — same chain as the worker.
        tick_size: None,
        caps: Default::default(),
    };

    match dispatch::run_enter(broker, store, &verified, &cfg, now, None, Some(granularity)).await {
        ActionResult::Ok(outcome) => {
            tracing::debug!(rule = %fired.rule_id, %outcome, "tick: enter placed");
            EnterGateOutcome::Placed {
                order_id: Some(order_id),
            }
        }
        ActionResult::Rejected { outcome, .. } => {
            tracing::debug!(rule = %fired.rule_id, %outcome, "tick: enter rejected (0R skip)");
            EnterGateOutcome::Rejected { reason: outcome }
        }
        ActionResult::Failed(reason) => {
            // The ReplayBroker never returns a broker error on a properly-armed
            // placement, so this is a wiring fault, not a market outcome. Treat
            // it as a no-fill (no placed order), and log it loudly.
            tracing::error!(
                rule = %fired.rule_id,
                %reason,
                "tick: enter dispatch FAILED unexpectedly under ReplayBroker"
            );
            EnterGateOutcome::Rejected {
                reason: format!("failed: {reason}"),
            }
        }
    }
}

/// Stamp `superseded = true` on any already-recorded enter `Fire` whose order
/// the gate has now cancelled (cancel-and-replace). Called right after each
/// accepted placement: the gate cancels at most a prior resting order, and the
/// matching fire is the one carrying that `order_id`. Idempotent — a fire
/// already marked stays marked.
fn mark_superseded(fires: &mut [Fire], broker: &ReplayBroker) {
    let cancelled = broker.cancelled_order_ids();
    for fire in fires.iter_mut() {
        if let Some(id) = fire.order_id()
            && cancelled.contains(&id.to_string())
        {
            fire.superseded = true;
        }
    }
}

/// The distinct, ascending, unfired control-rule epochs (pause/resume/
/// news-start/news-end `TimeReached`) that fall **strictly between** `lo` and
/// `hi` — i.e. inside a bar, not on either bar close. The bar ticks already fire
/// any epoch that lands on a close, so injecting those again would double-fire;
/// the latch (`state.fired`) makes a duplicate harmless but we keep the interval
/// half-open on both ends to be explicit.
fn control_epochs_between(
    plan: &TradePlan,
    state: &PlanState,
    lo: DateTime<Utc>,
    hi: DateTime<Utc>,
) -> Vec<DateTime<Utc>> {
    let (lo, hi) = (lo.timestamp(), hi.timestamp());
    let mut epochs: Vec<i64> = plan
        .rules
        .iter()
        .filter(|r| {
            matches!(
                r.intent.action,
                Action::Pause | Action::Resume | Action::NewsStart | Action::NewsEnd
            ) && !state.fired.contains(&r.rule_id)
        })
        .filter_map(|r| match r.trigger {
            Trigger::TimeReached { at_epoch } if at_epoch > lo && at_epoch < hi => Some(at_epoch),
            _ => None,
        })
        .collect();
    epochs.sort_unstable();
    epochs.dedup();
    epochs
        .into_iter()
        .filter_map(|e| DateTime::from_timestamp(e, 0))
        .collect()
}

/// Inject the worker's sub-bar control ticks into the replay. The live worker's
/// 5s cron opens/closes a pause or news window the instant its wall-clock epoch
/// passes — which, for an event baked at a sub-bar minute (14:30 on H1), is
/// **between** two bar closes. The replay owns a virtual clock, so it reproduces
/// that exactly: for each unfired control epoch in `(prev_close, this_close)` it
/// pins the clock to the epoch and runs [`evaluate_controls_only`] — the SAME
/// engine entry point the worker's candle-less tick calls — so both drivers
/// open/close every window at the identical instant
/// (`[[strategy_changes_in_both_replayer_and_worker]]`).
///
/// Control fires never fill and are always `NotAnEnter`; pause/resume seed the
/// same store state the enter gate reads (via the same `pause_gate` helpers the
/// bar loop uses), and news-start/news-end are reflected in the returned `state`
/// (`open_news_windows`). `last_candle` is the last closed bar (the fire shell);
/// `forward` is unused for a non-filling control fire but recorded for shape.
#[allow(clippy::too_many_arguments)]
async fn inject_control_ticks(
    plan: &TradePlan,
    state: &mut PlanState,
    store: &MemStateStore,
    fires: &mut Vec<Fire>,
    last_candle: &Candle,
    forward: &[EngineCandle],
    prev_close: DateTime<Utc>,
    this_close: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) {
    for epoch in control_epochs_between(plan, state, prev_close, this_close) {
        store.set_clock(epoch);
        let eval = evaluate_controls_only(plan, state, last_candle, epoch, expires_at);
        *state = eval.new_state;
        for fired in eval.fired {
            tracing::debug!(
                virtual_tick = %epoch,
                rule = %fired.rule_id,
                action = ?fired.intent.action,
                "sub-bar control tick fired"
            );
            match fired.intent.action {
                Action::Pause => {
                    if let Err(e) = pause_gate::apply_pause(store, &fired.intent, epoch).await {
                        tracing::error!(rule = %fired.rule_id, error = %e, "apply_pause failed");
                    }
                }
                Action::Resume => {
                    if let Err(e) = pause_gate::apply_resume(store, &fired.intent).await {
                        tracing::error!(rule = %fired.rule_id, error = %e, "apply_resume failed");
                    }
                }
                // NewsStart / NewsEnd are reflected in `state.open_news_windows`
                // already (the engine mutated it); nothing to seed in the store.
                _ => {}
            }
            fires.push(Fire {
                fired,
                forward: forward.to_vec(),
                gate_outcome: EnterGateOutcome::NotAnEnter,
                superseded: false,
                entry_spread_price: None,
            });
        }
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
            cross_buffer_pct: 0.0,
            retest_atr_step: trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            replay_start: None,
            armed_at: None,
            armed_sentiment: None,
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
            cross_buffer_pct: 0.0,
            retest_atr_step: trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            replay_start: None,
            armed_at: None,
            armed_sentiment: None,
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

    /// A single-shot LONG stop-enter (crosses 1.1050 up, OnClose) wrapped in a
    /// news-blackout `pause`/`resume` pair (TimeReached). `pause_epoch` /
    /// `resume_epoch` bound the blackout window. The enter shares the trade_id
    /// the pause/resume scope to, so the blackout gate can find it.
    fn paused_enter_plan(pause_epoch: i64, resume_epoch: i64) -> TradePlan {
        serde_json::from_str(&format!(
            r#"{{
                "trade_id": "t041",
                "instrument": "CAD_CHF",
                "direction": "long",
                "granularity": "h1",
                "pip_size": 0.0001,
                "rules": [
                    {{
                        "rule_id": "01-pause-cad-gdp",
                        "trigger": {{ "type": "time_reached", "at_epoch": {pause_epoch} }},
                        "fire_mode": "once",
                        "intent": {{
                            "v": 1, "id": "pause-cad-gdp", "not_after": "2099-01-01T00:00:00Z",
                            "action": "pause", "instrument": "CAD_CHF",
                            "trade_id": "t041", "blackout_id": "cad-gdp", "reason": "CAD GDP"
                        }}
                    }},
                    {{
                        "rule_id": "02-resume-cad-gdp",
                        "trigger": {{ "type": "time_reached", "at_epoch": {resume_epoch} }},
                        "fire_mode": "once",
                        "intent": {{
                            "v": 1, "id": "resume-cad-gdp", "not_after": "2099-01-01T00:00:00Z",
                            "action": "resume", "instrument": "CAD_CHF",
                            "trade_id": "t041", "blackout_id": "cad-gdp"
                        }}
                    }},
                    {{
                        "rule_id": "05-enter",
                        "trigger": {{ "type": "horizontal_cross", "level": 1.1050, "dir": "up", "bar": "on_close" }},
                        "fire_mode": "once",
                        "intent": {{
                            "v": 1, "id": "t041-enter", "not_after": "2099-01-01T00:00:00Z",
                            "action": "enter", "instrument": "CAD_CHF", "direction": "long",
                            "entry": {{ "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.1100 }},
                            "stop_loss": {{ "absolute": 1.1000 }},
                            "take_profit": {{ "absolute": 1.1300 }},
                            "broker": "tradenation", "trade_id": "t041", "max_retries": 0
                        }}
                    }}
                ]
            }}"#
        ))
        .expect("parse paused-enter plan")
    }

    /// Bug `BUG-replay-candles-pause-not-enforced`: an enter that fires inside an
    /// active news-blackout pause must be SUPPRESSED (no fill, 0R) — matching the
    /// live worker, which 423s a paused entry. Without the gate this enter would
    /// fill at 1.1100 and take profit at 1.1300.
    #[tokio::test]
    async fn enter_inside_pause_window_is_suppressed_no_fill() {
        // 10 seed bars below 1.1050, then bar 10 = pause epoch, bar 12 crosses
        // 1.1050 up (enter fires), bars 13+ run through 1.1100 to TP 1.1300.
        // Resume is far past the window so the pause is active at bar 12.
        let mut candles: Vec<EngineCandle> = (0..10).map(|i| candle(i * 3600, 1.1040)).collect();
        candles.push(candle(10 * 3600, 1.1040)); // bar 10: pause fires (epoch 10*3600)
        candles.push(candle(11 * 3600, 1.1045)); // bar 11
        candles.push(ohlc(12 * 3600, 1.1045, 1.1058, 1.1042, 1.1055)); // bar 12: OnClose cross
        candles.push(ohlc(13 * 3600, 1.1060, 1.1120, 1.1055, 1.1110)); // would fill @1.1100
        candles.push(ohlc(14 * 3600, 1.1110, 1.1310, 1.1185, 1.1300)); // would TP 1.1300

        let pause_epoch = 10 * 3600;
        let resume_epoch = 100 * 3600; // far past the window — pause stays active
        let r = run(
            &paused_enter_plan(pause_epoch, resume_epoch),
            &candles,
            Granularity::H1,
            all_live(),
            expires(),
        )
        .await;

        let enter = r
            .fires
            .iter()
            .find(|f| f.fired.rule_id == "05-enter")
            .expect("enter fired");
        assert_eq!(
            enter.suppressed_by(),
            vec!["cad-gdp(CAD GDP)".to_string()],
            "the enter must be suppressed by the active cad-gdp blackout"
        );

        // And the report must show the skip, not a fabricated fill.
        let report = crate::report::render(
            &paused_enter_plan(pause_epoch, resume_epoch),
            &r,
            true,
            false,
            &[],
        );
        assert!(
            report.contains("SUPPRESSED") && report.contains("NO FILL"),
            "report must show the paused enter as a 0R skip:\n{report}"
        );
        assert!(
            report.contains("TP: 0  SL: 0"),
            "a suppressed enter is not tallied as a win:\n{report}"
        );
    }

    #[tokio::test]
    async fn sub_bar_pause_epoch_opens_via_virtual_tick_and_suppresses_enter() {
        // PR2 parity: the pause epoch is at 10.5h — BETWEEN the bar-10 close
        // (11h) and... no: it lands strictly inside bar 10's life (open 10h,
        // close 11h), i.e. in the injection interval for bar 10
        // (prev_close 10h, this_close 11h). The live worker's 5s cron would open
        // the blackout at 10.5h; the replay injects a virtual tick there. The
        // enter crosses at bar 12, well after 10.5h, so the (still-open) blackout
        // must suppress it — exactly as the bar-aligned pause test does, but now
        // proving the SUB-BAR open path.
        let mut candles: Vec<EngineCandle> = (0..10).map(|i| candle(i * 3600, 1.1040)).collect();
        candles.push(candle(10 * 3600, 1.1040)); // bar 10
        candles.push(candle(11 * 3600, 1.1045)); // bar 11
        candles.push(ohlc(12 * 3600, 1.1045, 1.1058, 1.1042, 1.1055)); // bar 12: enter cross
        candles.push(ohlc(13 * 3600, 1.1060, 1.1120, 1.1055, 1.1110));
        candles.push(ohlc(14 * 3600, 1.1110, 1.1310, 1.1185, 1.1300));

        let pause_epoch = 10 * 3600 + 1800; // 10.5h — sub-bar, inside bar 10
        let resume_epoch = 100 * 3600; // far past — blackout stays active
        let r = run(
            &paused_enter_plan(pause_epoch, resume_epoch),
            &candles,
            Granularity::H1,
            all_live(),
            expires(),
        )
        .await;

        // The pause fired at its exact sub-bar epoch via a virtual tick.
        let pause = r
            .fires
            .iter()
            .find(|f| f.fired.rule_id == "01-pause-cad-gdp")
            .expect("sub-bar pause fired");
        assert_eq!(
            pause.fired.intent.action,
            Action::Pause,
            "the sub-bar control fire is the pause"
        );

        // And it suppressed the later enter, same as a bar-aligned blackout.
        let enter = r
            .fires
            .iter()
            .find(|f| f.fired.rule_id == "05-enter")
            .expect("enter fired");
        assert_eq!(
            enter.suppressed_by(),
            vec!["cad-gdp(CAD GDP)".to_string()],
            "the enter must be suppressed by the sub-bar-opened cad-gdp blackout"
        );
    }

    #[tokio::test]
    async fn virtual_tick_opens_window_at_same_instant_as_worker_controls_only() {
        // Parity invariant: the replay's virtual tick and the worker's
        // candle-less tick both call `evaluate_controls_only`, so a sub-bar epoch
        // opens the window at the SAME instant in each. Here we drive both paths
        // directly and assert they agree.
        let pause_epoch = 10 * 3600 + 1800; // 10.5h, sub-bar
        let plan = paused_enter_plan(pause_epoch, 100 * 3600);
        let prior = seed_plan_state(&plan, &[], expires());
        let last = Candle {
            time: Utc.timestamp_opt(10 * 3600, 0).unwrap(),
            o: 1.1,
            h: 1.1,
            l: 1.1,
            c: 1.1,
        };
        let epoch_dt = Utc.timestamp_opt(pause_epoch, 0).unwrap();

        // Worker path: control-only eval at the epoch.
        let worker = evaluate_controls_only(&plan, &prior, &last, epoch_dt, expires());
        let worker_ids: Vec<&str> = worker.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(worker_ids, vec!["01-pause-cad-gdp"]);

        // Replay path: the epoch is inside the (10h, 11h) bar interval, so the
        // injector picks it up, and firing it lands the SAME rule.
        let picked = control_epochs_between(
            &plan,
            &prior,
            Utc.timestamp_opt(10 * 3600, 0).unwrap(),
            Utc.timestamp_opt(11 * 3600, 0).unwrap(),
        );
        assert_eq!(picked, vec![epoch_dt], "injector picks the sub-bar epoch");

        let store = MemStateStore::default();
        let mut fires = Vec::new();
        let mut state = prior.clone();
        inject_control_ticks(
            &plan,
            &mut state,
            &store,
            &mut fires,
            &last,
            &[],
            Utc.timestamp_opt(10 * 3600, 0).unwrap(),
            Utc.timestamp_opt(11 * 3600, 0).unwrap(),
            expires(),
        )
        .await;
        let replay_ids: Vec<&str> = fires.iter().map(|f| f.fired.rule_id.as_str()).collect();
        assert_eq!(
            replay_ids, worker_ids,
            "replay virtual tick fires the same control rule as the worker"
        );
    }

    /// The converse: the SAME enter with NO pause window fills and takes profit.
    /// This is the A/B the journal needs — with-blackout (0R) vs without (+R).
    #[tokio::test]
    async fn enter_without_pause_fills_and_takes_profit() {
        let mut candles: Vec<EngineCandle> = (0..10).map(|i| candle(i * 3600, 1.1040)).collect();
        candles.push(candle(10 * 3600, 1.1040));
        candles.push(candle(11 * 3600, 1.1045));
        candles.push(ohlc(12 * 3600, 1.1045, 1.1058, 1.1042, 1.1055));
        candles.push(ohlc(13 * 3600, 1.1060, 1.1120, 1.1055, 1.1110));
        candles.push(ohlc(14 * 3600, 1.1110, 1.1310, 1.1185, 1.1300));

        // Pause + resume both BEFORE the enter bar → no active blackout at bar 12.
        let r = run(
            &paused_enter_plan(3600, 2 * 3600),
            &candles,
            Granularity::H1,
            all_live(),
            expires(),
        )
        .await;

        let enter = r
            .fires
            .iter()
            .find(|f| f.fired.rule_id == "05-enter")
            .expect("enter fired");
        assert!(
            enter.suppressed_by().is_empty(),
            "no active pause at the enter bar → not suppressed"
        );
        let report =
            crate::report::render(&paused_enter_plan(3600, 2 * 3600), &r, true, false, &[]);
        assert!(
            report.contains("TP: 1"),
            "without an active blackout the enter fills and takes profit:\n{report}"
        );
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

    /// A two-enter, multi-shot (strategy-v2-shaped) plan: a `stop` enter and a
    /// `limit` enter sharing one `trade_id`, both `max_retries: 5`, both fired
    /// by a level cross so the test needs no PinePattern geometry. Both are LONG
    /// with the same absolute SL/TP; only the entry order type differs. This is
    /// the cross-fire lifecycle the two bugs live in — a new entry must cancel a
    /// resting sibling order, and must not stack on an open position.
    fn two_enter_v2_plan() -> TradePlan {
        serde_json::from_str(
            r#"{
                "trade_id": "v2",
                "instrument": "EUR_USD",
                "direction": "long",
                "granularity": "h1",
                "pip_size": 0.0001,
                "rules": [
                    {
                        "rule_id": "05-enter",
                        "trigger": { "type": "horizontal_cross", "level": 1.1050, "dir": "up", "bar": "on_close" },
                        "fire_mode": "once",
                        "intent": {
                            "v": 1,
                            "id": "v2-stop",
                            "not_after": "2099-01-01T00:00:00Z",
                            "action": "enter",
                            "instrument": "EUR_USD",
                            "direction": "long",
                            "entry": { "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.1100 },
                            "stop_loss": { "absolute": 1.1000 },
                            "take_profit": { "absolute": 1.1300 },
                            "broker": "tradenation",
                            "trade_id": "v2",
                            "max_retries": 5
                        }
                    },
                    {
                        "rule_id": "09-enter-qm",
                        "trigger": { "type": "horizontal_cross", "level": 1.1060, "dir": "up", "bar": "on_close" },
                        "fire_mode": "once",
                        "intent": {
                            "v": 1,
                            "id": "v2-limit",
                            "not_after": "2099-01-01T00:00:00Z",
                            "action": "enter",
                            "instrument": "EUR_USD",
                            "direction": "long",
                            "entry": { "type": "limit", "from": "close", "offset_pips": 0.0, "at": 1.1100 },
                            "stop_loss": { "absolute": 1.1000 },
                            "take_profit": { "absolute": 1.1300 },
                            "broker": "tradenation",
                            "trade_id": "v2",
                            "max_retries": 5
                        }
                    }
                ]
            }"#,
        )
        .expect("parse v2 plan")
    }

    /// Bug 1 (cancel resting order) + Bug 2 (don't stack on an open position):
    /// when a second enter fires while the first's order is still **resting**,
    /// the gate must cancel-and-replace it (the first fire is `superseded`), and
    /// the still-resting order must not go on to fill as a second position.
    ///
    /// Geometry (all LONG stop/limit @ 1.1100, SL 1.1000, TP 1.1300):
    /// - bar 0..9: warm-up/seed below every level.
    /// - bar 10: crosses 1.1050 up → `05-enter` (stop) fires, order rests @1.1100.
    /// - bar 11: crosses 1.1060 up → `09-enter-qm` (limit) fires while the stop
    ///   still rests (neither has reached 1.1100 yet) → gate cancels the stop →
    ///   the stop fire is `superseded`.
    /// - bars 12+: price rises through 1.1100 (fills the limit) then to TP.
    /// Exactly one taken position (the limit), no overlap.
    #[tokio::test]
    async fn a_new_enter_cancels_a_resting_sibling_order_no_overlap() {
        // 10 seed bars at 1.1040 (below 1.1050), then the two crosses, then a run
        // up to TP. Keep highs below 1.1100 until bar 12 so neither order fills
        // before the cancel-and-replace happens.
        let mut candles: Vec<EngineCandle> = (0..10).map(|i| candle(i * 3600, 1.1040)).collect();
        // bar 10: OnClose cross of 1.1050 (prev close 1.1040 < 1.1050 ≤ close
        // 1.1055, and 1.1055 < 1.1060 so the limit doesn't yet cross) →
        // `05-enter` (stop) fires once; its order rests @1.1100.
        candles.push(ohlc(10 * 3600, 1.1045, 1.1058, 1.1042, 1.1055));
        // bar 11: OnClose cross of 1.1060 (prev close 1.1055 < 1.1060 ≤ close
        // 1.1065) → `09-enter-qm` (limit) fires once, while the stop's order
        // @1.1100 is still resting (high < 1.1100 throughout).
        candles.push(ohlc(11 * 3600, 1.1056, 1.1068, 1.1050, 1.1065));
        // bars 12..15: rise through 1.1100 (fills) up to TP 1.1300.
        candles.push(ohlc(12 * 3600, 1.1060, 1.1120, 1.1055, 1.1110)); // fills @1.1100
        candles.push(ohlc(13 * 3600, 1.1110, 1.1200, 1.1100, 1.1190));
        candles.push(ohlc(14 * 3600, 1.1190, 1.1310, 1.1185, 1.1300)); // hits TP 1.1300
        candles.push(ohlc(15 * 3600, 1.1300, 1.1320, 1.1290, 1.1305));

        let r = run(
            &two_enter_v2_plan(),
            &candles,
            Granularity::H1,
            all_live(),
            expires(),
        )
        .await;

        let enters: Vec<&Fire> = r
            .fires
            .iter()
            .filter(|f| f.fired.intent.action == Action::Enter)
            .collect();
        assert_eq!(enters.len(), 2, "both enters fire (stop then limit)");

        let stop = enters
            .iter()
            .find(|f| f.fired.rule_id == "05-enter")
            .expect("stop enter recorded");
        let limit = enters
            .iter()
            .find(|f| f.fired.rule_id == "09-enter-qm")
            .expect("limit enter recorded");

        // Bug 1: the stop order was still resting when the limit fired, so the
        // gate cancelled-and-replaced it.
        assert!(
            stop.superseded,
            "the resting stop order must be superseded by the later limit enter"
        );
        // Bug 2: the limit is the live entry; it isn't itself superseded, and it
        // fills + takes profit as the *only* open position.
        assert!(
            !limit.superseded,
            "the replacing limit enter is the live one, not superseded"
        );

        // The report must reflect this: the superseded stop shows SUPERSEDED (no
        // fabricated fill), and exactly one trade is tallied (the limit's TP) —
        // not two overlapping positions.
        let report = crate::report::render(&two_enter_v2_plan(), &r, true, false, &[]);
        assert!(
            report.contains("SUPERSEDED"),
            "report must show the cancelled stop as SUPERSEDED:\n{report}"
        );
        assert!(
            report.contains("TP: 1  SL: 0"),
            "exactly one taken position (the limit's TP), no overlap:\n{report}"
        );
    }

    /// A SHORT plan with a `06-close-on-reversal` PinePattern{Long} guard: the
    /// short fills, then a bullish reversal candle prints inside the SR band
    /// **before** SL/TP — the position must close on that reversal bar, not run
    /// to window-end. This is the trade-075 Wheat bug: the engine now fires the
    /// close (a PinePattern guard), and the replay flattens the open short there.
    fn short_with_close_on_reversal_plan() -> TradePlan {
        // Short stop entry at 1.180 (absolute), SL 1.300 (above), TP 0.950
        // (below) — a wide bracket so neither is touched before the reversal.
        // The enter fires on an OnClose down-cross of 1.190; the close-on-
        // reversal is a Long PinePattern gated on price ∈ [1.15, 1.20].
        serde_json::from_str(
            r#"{
                "trade_id": "wheat-rev",
                "instrument": "WHEAT_USD",
                "direction": "short",
                "granularity": "h1",
                "pip_size": 0.001,
                "rules": [
                    {
                        "rule_id": "05-enter",
                        "trigger": { "type": "horizontal_cross", "level": 1.190, "dir": "down", "bar": "on_close" },
                        "fire_mode": "once",
                        "intent": {
                            "v": 1, "id": "wheat-rev-enter", "not_after": "2099-01-01T00:00:00Z",
                            "action": "enter", "instrument": "WHEAT_USD", "direction": "short",
                            "entry": { "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.180 },
                            "stop_loss": { "absolute": 1.300 },
                            "take_profit": { "absolute": 0.950 },
                            "broker": "oanda", "trade_id": "wheat-rev", "max_retries": 5
                        }
                    },
                    {
                        "rule_id": "06-close-on-reversal",
                        "trigger": { "type": "pine_pattern", "dir": "long" },
                        "fire_mode": "once",
                        "intent": {
                            "v": 1, "id": "wheat-rev-close", "not_after": "2099-01-01T00:00:00Z",
                            "action": "close", "instrument": "WHEAT_USD",
                            "inside_window": ["price"], "sr_bands": [[1.150, 1.200]],
                            "broker": "oanda", "trade_id": "wheat-rev"
                        }
                    }
                ]
            }"#,
        )
        .expect("parse short-with-close plan")
    }

    #[tokio::test]
    async fn open_short_closes_on_a_reversal_candle_in_the_band() {
        // 10 clean warm-up bars above the entry (the `run` seed floor is 10 bars
        // when live_start precedes the window), then the live action: the OnClose
        // down-cross fires the short, the next bar fills it at 1.180, then a
        // bullish pinbar reversal (close 1.18, inside [1.15, 1.20]) prints — the
        // short must CLOSE there. Warm-up bars are flat (no stray signal).
        let mut candles: Vec<EngineCandle> = (0..11)
            .map(|i| ohlc(i * 3600, 1.20, 1.205, 1.195, 1.20))
            .collect();
        // bar 11: OnClose down-cross of 1.190 (prev 1.20 > 1.190 ≥ close 1.185)
        // → `05-enter` (short stop @1.180) fires; order rests.
        candles.push(ohlc(11 * 3600, 1.195, 1.196, 1.184, 1.185));
        // bar 12: dips to 1.180 → fills the short stop @1.180. Stays above TP.
        candles.push(ohlc(12 * 3600, 1.185, 1.186, 1.179, 1.182));
        // bar 13: a clean prior bar for the pinbar breakout (no signal shape):
        // a small down bar that does NOT undercut to set up a long pinbar itself.
        candles.push(ohlc(13 * 3600, 1.182, 1.185, 1.175, 1.178));
        // bar 14: BULLISH pinbar. range 1.00..1.20 = 0.20; body 1.16..1.18 (top
        // quartile: top_25 = 1.20 - 0.05 = 1.15 → body_bottom 1.16 ≥ 1.15);
        // lower wick = 1.16 - 1.00 = 0.16 ≥ 0.10; low 1.00 < prior low 1.175;
        // close 1.18 > open 1.16 → bullish. close 1.18 ∈ [1.15, 1.20]. low 1.00
        // stays above TP 0.95, high 1.20 below SL 1.30 → neither bracket hit.
        candles.push(ohlc(14 * 3600, 1.16, 1.20, 1.00, 1.18));
        // bar 15+: drift, so without the close the short would just stay open.
        candles.push(ohlc(15 * 3600, 1.18, 1.19, 1.17, 1.18));

        let plan = short_with_close_on_reversal_plan();
        let r = run(&plan, &candles, Granularity::H1, all_live(), expires()).await;

        // Both fired: the short enter, then the close guard on the reversal.
        assert!(
            r.fires.iter().any(|f| f.fired.rule_id == "05-enter"),
            "the short enter must fire and fill"
        );
        assert!(
            r.fires
                .iter()
                .any(|f| f.fired.rule_id == "06-close-on-reversal"),
            "the engine must fire the close-on-reversal guard"
        );
        // The reversal-close flattens the open short but must NOT retire the
        // plan — this is a multi-shot plan (max_retries: 5), so the spine stays
        // alive to re-enter. (A reversal-close is a per-trade exit, never a setup
        // invalidation.)
        assert!(
            !r.done,
            "a reversal-close flattens but never retires the plan (retries continue)"
        );

        // The replay report shows the short CLOSED ON REVERSAL, not held open.
        let report = crate::report::render(&plan, &r, true, false, &[]);
        assert!(
            report.contains("CLOSED ON REVERSAL"),
            "the open short must close on the reversal candle:\n{report}"
        );
        assert!(
            report.contains("REV: 1"),
            "the reversal close is tallied:\n{report}"
        );
        assert!(
            !report.contains("still open at window end"),
            "the short must not be reported as still open:\n{report}"
        );
    }
}
