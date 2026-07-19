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
    Candle, FiredIntent, Granularity, PlanState, TradePlan, Trigger, enter_preconditions_reason,
    evaluate_controls_only, evaluate_plan, seed_plan_state,
};

use trade_control_core::signals::{
    DetectFlags, DetectorConfig, atr_length_for, detect_at, first_confirmed_signal_at, wilder_atr,
};

use super::replay_broker::{RealizedOutcome, ReplayBroker};
use super::verbose::{BarTrace, DetectedMark};
use trade_control_cli::replay_args::DetectorMarkConfig;

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
    /// The broker-ledger realized outcome for this enter (PR 4b-2): the fill /
    /// exit / floored bracket the `ReplayBroker`'s position ledger computed for
    /// the placed order, so the report reads it instead of re-simulating the fill
    /// itself. Populated by [`run`] after the loop (once the reversal-close set is
    /// known) for a **placed** enter that resolved to a drawable outcome; `None`
    /// for a non-enter, a rejected/superseded enter (the report renders those from
    /// gate state), or an unresolved bracket (nothing to draw). The report's
    /// not-taken kinds (`NeverFilled` / `Declined` / `SpreadBlackout`) come
    /// through here as `Some` — they're fill physics the broker owns.
    pub realized: Option<RealizedOutcome>,
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
    mark_cfg: DetectorMarkConfig,
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
    // Edge-trigger the "signal confirmed, not entering (spread-hour)" line: a
    // confirmed signal that lands inside a spread block persists confirmed for
    // many bars, but we only want ONE line — on the bar the confirmation first
    // becomes ready while still suppressed — not a line per suppressed bar.
    let mut prev_confirmed_ready = false;

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
    // PR 4b-3: the SAME shared spread-hour resting-order lifecycle the live cron
    // runs, driven per bar below. Its two offline seams — a fixed dispatch config
    // and an armed-Verified source reading the broker ledger — live in
    // `super::lifecycle`; the broker + store are the ones above.
    let lifecycle_cfg = super::lifecycle::ReplayConfigProvider::new(plan.pip_size);

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
        // Mark the candle detector's verdict on THIS bar — the same `detect_at`
        // + `wilder_atr` the engine's `latched_signal_at` runs — so a golden the
        // plan never entered on (wrong phase, watermark-skipped, opposite dir)
        // still surfaces. Filtered by the active `--candle-detector-*` config;
        // `None` when the feature is off or nothing passes the filter.
        let detected = detect_mark(&mid, i, granularity, &mark_cfg);
        // The enter declines the engine recorded this tick: a signal fired +
        // matched direction but the pre-flight rejected it (needs-golden /
        // needs-confirmed / resolve-failed like below-min-R). `evaluate_plan`
        // saw only this one new bar, so every decline belongs to `candles[i]` —
        // this is the "golden seen but no entry, here's why" line.
        //
        // Suppress the `not golden` decline when marking golden-only candles: in
        // that view it's tautological noise (the operator already said they only
        // care about golden signals). See `suppresses_not_golden_decline`.
        let hide_not_golden = mark_cfg.suppresses_not_golden_decline();
        let decline_reasons: Vec<String> = eval
            .entry_declines
            .iter()
            .filter(|d| {
                !(hide_not_golden && d.reason == trade_control_core::plan_eval::NOT_GOLDEN_DECLINE)
            })
            .map(|d| d.reason.clone())
            .collect();
        // "not-taken" reason: a marked signal that fired NO enter and produced
        // NO decline was blocked by an unmet *precondition* (the trigger never
        // fired) — wrong phase (break-and-close outstanding), an unstamped
        // retest, or a confirmation requirement. `EntryDecline` only covers the
        // fired-then-declined case; this covers the never-fired case. Compute it
        // only for a marked bar with no fire and no decline, so it doesn't add
        // noise where the golden actually entered or was already explained.
        let not_taken = not_taken_reason(
            plan,
            &state,
            candles[i].time,
            detected.is_some(),
            &fired_rules,
            &decline_reasons,
        );
        // Gated on bar size like the engine's entry suppression: only 15m/1h are
        // dominated by a 1h spread hour, so on H4+ the "signal confirmed, not
        // entering (spread-hour suppressed)" trace line must NOT appear — the H4
        // entry actually fires. Keeps the trace honest (replay == live).
        let spread_hour = trade_control_core::spread_blackout::suppress_on_spread_hour(
            &plan.instrument,
            candles[i].time,
            plan.granularity,
        );
        // On a spread-hour bar, resolve the block bounds and whether a confirmed
        // signal is ready — so the trace can say "signal confirmed, not entering,
        // spread-hour X→Y". The confirmation typically lands a couple of bars
        // AFTER the golden printed (and on a bar that prints no fresh mark of its
        // own), so this is deliberately NOT gated on `detected.is_some()`: it fires
        // on the first suppressed bar where the QM enter would have become
        // eligible. Edge-triggered via `prev_confirmed_ready` so a confirmation
        // that stays ready across a long block yields one line, not one per bar.
        let confirmed_ready = spread_hour && confirmed_signal_ready(plan, &state, &mid, i);
        let (spread_block, confirmed_while_suppressed) = if spread_hour {
            (
                trade_control_core::spread_blackout::spread_block_window(
                    &plan.instrument,
                    candles[i].time,
                ),
                confirmed_ready && !prev_confirmed_ready,
            )
        } else {
            (None, false)
        };
        prev_confirmed_ready = confirmed_ready;
        traces.push(BarTrace::diff(
            candles[i].time,
            &before,
            &state,
            fired_rules,
            detected,
            decline_reasons,
            not_taken,
            spread_hour,
            spread_block,
            confirmed_while_suppressed,
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
            // Attach the position-ledger geometry to the placed order so the
            // broker owns its fill/exit outcome (PR 4b-2). The dispatch's
            // `place_entry` already recorded a geometry-less attempt under this
            // order id; `record_order` upgrades it in place with the forward path
            // + entry-spread the report used to walk. The report then reads
            // `broker.realized_outcome(order_id, closes)` instead of re-simulating.
            // The shell must be the SAME one the dispatch armed — rebuild it the
            // identical way (signal-folded for an H&S Pine fire, plain otherwise).
            if let EnterGateOutcome::Placed {
                order_id: Some(order_id),
            } = &gate_outcome
            {
                let shell = match &fired.signal {
                    Some(sig) => Shell::from_candle_and_signal(&fired.candle, sig),
                    None => Shell::from_candle(&fired.candle),
                };
                replay_broker.record_order(
                    order_id.clone(),
                    fired.intent.clone(),
                    shell,
                    forward.clone(),
                    entry_spread_price,
                );
            }
            fires.push(Fire {
                fired,
                forward,
                gate_outcome,
                superseded: false,
                entry_spread_price,
                realized: None,
            });
        }

        // PR 4b-3: run the shared resting-order lifecycle for this bar AFTER the
        // enter dispatch, so any order that just went resting is visible. In a
        // spread hour it cancels + backs up the resting order through the
        // engine→broker path (the cancel sets the ledger's `cancelled` flag →
        // `realized_outcome` → None → the report shows no fill); on a later clean
        // bar it re-drives it. The `is_spread_hour` gate is the ON trigger, so a
        // clean bar is a no-op. Account is `None` — the ReplayBroker doesn't scope
        // orders by account, and `is_spread_hour` keys on instrument + time only.
        //
        // Point the broker's prior-attempt clock at THIS bar first. `set_as_of` is
        // otherwise only called per-enter inside `dispatch_enter`, so without this
        // the lifecycle's `list_pending_orders`/`resolve` would judge pending-ness
        // against the last fire bar, not the current one — a resting order would be
        // listed (or not) as of stale time. Use the same `now` the lifecycle runs
        // at so the cancel/restore timing is bar-accurate.
        replay_broker.set_as_of(now);
        let src = super::lifecycle::ReplayVerifiedSource::new(&replay_broker);
        // Replay is the SOLE owner of the record (no System 2 widened stops
        // offline), so it clears the record itself — `ClearRecord`, the default
        // behaviour, byte-identical to before the Option-A clear-policy split.
        trade_control_core::pending_lifecycle::pending_order_lifecycle(
            &replay_broker,
            &store,
            &lifecycle_cfg,
            &src,
            None,
            now,
            trade_control_core::pending_lifecycle::ClearPolicy::ClearRecord,
        )
        .await;

        if eval.done {
            done = true;
            break;
        }
    }

    // PR 4b-2: the report reads each placed enter's outcome from the broker
    // ledger. The reversal-close set is plan-wide (a `06-close-on-reversal` can
    // flatten a position that filled bars earlier), so it's only known now that
    // every fire is collected. Build it, then ask the broker to realize each
    // placed order against it and stash the outcome on the fire. A superseded
    // order (its resting order cancelled by a later entry) is skipped — the
    // report renders it as cancelled, not a fill, exactly as before.
    let closes = super::report::collect_close_fires_from(&fires);
    for fire in fires.iter_mut() {
        if fire.superseded {
            continue;
        }
        let order_id = fire.order_id().map(str::to_owned);
        if let Some(order_id) = order_id {
            fire.realized = replay_broker.realized_outcome(&order_id, &closes);
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

/// The candle-detector mark for `mid[i]`, filtered by `cfg`. Runs the SAME
/// `detect_at` + `wilder_atr` the engine's `latched_signal_at` uses per bar (all
/// five patterns on — [`DetectFlags::default`] — per the design decision), so a
/// marked golden is exactly what the engine would have detected on that bar. The
/// golden test compares `Detected::size` against the Wilder ATR over `mid[..=i]`
/// at the granularity's `atr_length_for`. Returns `None` when the feature is
/// off, no pattern printed, or the signal fails the direction/golden filter.
fn detect_mark(
    mid: &[Candle],
    i: usize,
    granularity: Granularity,
    cfg: &DetectorMarkConfig,
) -> Option<DetectedMark> {
    if cfg.is_off() {
        return None;
    }
    let d = detect_at(mid, i, &DetectFlags::default())?;
    let atr = wilder_atr(&mid[..=i], atr_length_for(granularity));
    let golden = atr.is_some_and(|a| d.is_golden(a));
    if !cfg.accepts(d.direction, golden) {
        return None;
    }
    Some(DetectedMark {
        direction: d.direction,
        kind: d.geometry.kind,
        size: d.size,
        atr,
        golden,
    })
}

/// Does a confirmed signal in the plan's direction exist as of bar `i`? Runs the
/// engine's own [`first_confirmed_signal_at`] over `mid[..=i]` with the same
/// `confirmed_floor` [`evaluate_one_entry`](trade_control_engine) uses (the later
/// of the break-and-close stamp and the replay-start cursor), so "confirmed" here
/// means exactly what a confirmation-gated (QM) enter would see. Used only to
/// enrich the spread-hour line: when this is true on a suppressed spread-hour bar,
/// a ready-to-enter setup was held back purely by the spread hour. `want_kind` is
/// `None` (any pattern) and the re-entry watermark is ignored — this is a report,
/// not the fire path.
fn confirmed_signal_ready(plan: &TradePlan, state: &PlanState, mid: &[Candle], i: usize) -> bool {
    let cfg = DetectorConfig::pine_defaults(plan.granularity);
    let explicit = [
        state.break_close_at,
        plan.replay_start
            .and_then(|s| chrono::DateTime::from_timestamp(s, 0)),
    ]
    .into_iter()
    .flatten()
    .max();
    // Same derived scan floor the fire path uses (`eval_pine_entry`), via the
    // shared `confirmed_scan_floor` seam — so this "would have entered" annotation
    // can't disagree with the real decision (bug ①).
    let window_times: Vec<_> = mid.iter().map(|c| c.time).collect();
    let confirmed_floor = trade_control_core::signals::confirmed_scan_floor(
        &window_times,
        i,
        &cfg,
        plan.granularity,
        explicit,
    );
    first_confirmed_signal_at(mid, i, &cfg, plan.direction, None, confirmed_floor, None).is_some()
}

/// Why a *marked* signal on this bar wasn't taken as an entry — the
/// precondition gate(s) it was blocked by (break-and-close / confirmation /
/// retest). Only meaningful for a bar that carries a mark, fired no enter, and
/// produced no `EntryDecline`: those two cases already explain themselves (the
/// enter fired, or the dispatchable pre-flight declined it with a reason).
///
/// Delegates the actual gate logic to the engine's `enter_preconditions_reason`
/// so the "why not taken" answer can't drift from the real per-bar decision.
/// Returns a joined reason string (gates within an enter leg join with `and`,
/// alternative enter legs join with `or`, e.g.
/// `requires retest (…) or requires confirmation (…)` for strategy-v2)
/// for the `not-taken:` line, or `None` when nothing was outstanding.
fn not_taken_reason(
    plan: &TradePlan,
    state: &PlanState,
    bar: DateTime<Utc>,
    marked: bool,
    fired_rules: &[String],
    decline_reasons: &[String],
) -> Option<String> {
    // Only annotate a marked-but-unexplained bar: a fire or an existing decline
    // already tells the story.
    if !marked || !fired_rules.is_empty() || !decline_reasons.is_empty() {
        return None;
    }
    // Per-leg reason: gates within an enter leg join with "and", legs join with
    // "or" (any leg firing enters). For strategy-v2 this reads e.g. "requires
    // retest … or requires confirmation …" — the two enters are alternatives,
    // not a single enter needing both. Single-enter plans collapse to one leg.
    enter_preconditions_reason(plan, state, bar)
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

    match dispatch::run_enter(
        broker,
        store,
        &verified,
        &cfg,
        now,
        None,
        Some(granularity),
        false,
    )
    .await
    {
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
                // A control tick (pause/resume/news) is not an enter — no order,
                // no ledger outcome.
                realized: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use trade_control_cli::replay_args::{DirectionFilter, GoldenFilter};
    use trade_control_engine::intent::Direction;

    /// The detector-mark config for tests that don't exercise marking: feature
    /// off (either axis `None`), so `run` records no marks and behaves exactly as
    /// before this feature landed.
    fn no_marks() -> DetectorMarkConfig {
        DetectorMarkConfig::new(DirectionFilter::None, GoldenFilter::None, Direction::Long)
    }

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
            cross_buffer_atr: 0.0,
            retest_atr_step: trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            replay_start: None,
            armed_at: None,
            armed_sentiment: None,
        };
        let candles: Vec<EngineCandle> = (0..20)
            .map(|i| candle(i * 3600, 1.30 + i as f64 * 0.001))
            .collect();
        let expires = Utc.timestamp_opt(99 * 3600, 0).unwrap();

        let replay = run(
            &plan,
            &candles,
            Granularity::H1,
            all_live(),
            expires,
            no_marks(),
        )
        .await;

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
            cross_buffer_atr: 0.0,
            retest_atr_step: trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            replay_start: None,
            armed_at: None,
            armed_sentiment: None,
        };
        let replay = run(
            &plan,
            &[],
            Granularity::H1,
            all_live(),
            all_live(),
            no_marks(),
        )
        .await;
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
            no_marks(),
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
            no_marks(),
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
            no_marks(),
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
            None,
            &no_marks(),
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
            no_marks(),
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
            no_marks(),
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
        let report = crate::report::render(
            &paused_enter_plan(3600, 2 * 3600),
            &r,
            true,
            false,
            &[],
            None,
            &no_marks(),
        );
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
            no_marks(),
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
            no_marks(),
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
        let report = crate::report::render(
            &two_enter_v2_plan(),
            &r,
            true,
            false,
            &[],
            None,
            &no_marks(),
        );
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
        let r = run(
            &plan,
            &candles,
            Granularity::H1,
            all_live(),
            expires(),
            no_marks(),
        )
        .await;

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
        // alive to re-enter. (A reversal-close is a per-position close, never a
        // setup invalidation.)
        assert!(
            !r.done,
            "a reversal-close flattens but never retires the plan (retries continue)"
        );

        // The replay report shows the short CLOSED ON REVERSAL, not held open.
        let report = crate::report::render(&plan, &r, true, false, &[], None, &no_marks());
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

    /// A LONG plan whose enter never crosses its trigger — so nothing fires —
    /// still surfaces a golden **bullish** pinbar in the live window as a mark
    /// (direction `with` the trade). This is the whole point of the feature: the
    /// "golden candle we never entered on" is visible even with zero fires.
    fn never_firing_long_plan() -> TradePlan {
        // Enter is an up-cross of 9.99 — far above the ~1.x price band, so it
        // never triggers over the window. The plan just advances the watermark.
        serde_json::from_str(
            r#"{
                "trade_id": "gold-mark",
                "instrument": "EUR_USD",
                "direction": "long",
                "granularity": "h1",
                "pip_size": 0.0001,
                "rules": [{
                    "rule_id": "05-enter",
                    "trigger": { "type": "horizontal_cross", "level": 9.99, "dir": "up", "bar": "on_close" },
                    "fire_mode": "once",
                    "intent": {
                        "v": 1, "id": "gold-mark-enter", "not_after": "2099-01-01T00:00:00Z",
                        "action": "enter", "instrument": "EUR_USD", "direction": "long",
                        "entry": { "type": "stop", "from": "close", "offset_pips": 0.0, "at": 9.99 },
                        "stop_loss": { "absolute": 9.90 },
                        "take_profit": { "absolute": 10.5 },
                        "broker": "oanda", "trade_id": "gold-mark", "max_retries": 0
                    }
                }]
            }"#,
        )
        .expect("parse never-firing long plan")
    }

    /// Build a window: 30 warm-up bars to warm the H1 ATR (length 24), then a
    /// prior bar and a golden bullish pinbar (size 0.20 ≫ the ~0.01 ATR of the
    /// flat warm-up) in the live window. Returns (candles, live_start).
    fn window_with_golden_pinbar() -> (Vec<EngineCandle>, DateTime<Utc>) {
        // 30 flat warm-up bars (tiny 0.01 range → small ATR).
        let mut candles: Vec<EngineCandle> = (0..30)
            .map(|i| ohlc(i * 3600, 1.10, 1.105, 1.095, 1.10))
            .collect();
        // bar 30: prior bar for the pinbar (its low 1.05 must be undercut).
        candles.push(ohlc(30 * 3600, 1.10, 1.12, 1.05, 1.06));
        // bar 31: GOLDEN bullish pinbar — range 1.00..1.20 (0.20), body top
        // quartile, long lower wick, low 1.00 < prior low 1.05. size 0.20 ≫ ATR.
        candles.push(ohlc(31 * 3600, 1.16, 1.20, 1.00, 1.18));
        // bar 32: a trailing flat bar so bar 31 is a fully-closed live bar.
        candles.push(ohlc(32 * 3600, 1.18, 1.185, 1.175, 1.18));
        // live_start at bar 25 → the pinbar (bar 31) is in the live window.
        let live = candles[25].time;
        (candles, live)
    }

    fn marks(r: &Replay) -> Vec<&super::super::verbose::DetectedMark> {
        r.traces
            .iter()
            .filter_map(|t| t.detected.as_ref())
            .collect()
    }

    #[tokio::test]
    async fn golden_pinbar_marked_even_though_no_enter_fires() {
        let (candles, live) = window_with_golden_pinbar();
        let cfg =
            DetectorMarkConfig::new(DirectionFilter::With, GoldenFilter::Golden, Direction::Long);
        let r = run(
            &never_firing_long_plan(),
            &candles,
            Granularity::H1,
            live,
            expires(),
            cfg,
        )
        .await;

        // Nothing fired (the enter never crossed 9.99), yet the golden bullish
        // pinbar is marked.
        assert!(r.fires.is_empty(), "the enter never crosses → no fires");
        let m = marks(&r);
        assert_eq!(m.len(), 1, "exactly the golden pinbar is marked: {m:?}");
        assert!(m[0].golden, "the pinbar is golden (size ≫ ATR)");
        assert_eq!(m[0].direction, Direction::Long, "bullish → Long");

        // And the always-on summary counts it.
        let report =
            crate::report::render(&never_firing_long_plan(), &r, true, false, &[], None, &cfg);
        assert!(
            report.contains("1 golden"),
            "summary reports the golden mark:\n{report}"
        );
    }

    #[tokio::test]
    async fn against_and_none_filters_hide_the_bullish_golden() {
        let (candles, live) = window_with_golden_pinbar();

        // `against` on a LONG plan wants SHORT signals — the bullish pinbar is
        // filtered out.
        let against = DetectorMarkConfig::new(
            DirectionFilter::Against,
            GoldenFilter::Golden,
            Direction::Long,
        );
        let r = run(
            &never_firing_long_plan(),
            &candles,
            Granularity::H1,
            live,
            expires(),
            against,
        )
        .await;
        assert!(marks(&r).is_empty(), "against-dir hides the bullish golden");

        // `none` on either axis disables marking entirely — and the summary is
        // omitted.
        let off =
            DetectorMarkConfig::new(DirectionFilter::None, GoldenFilter::Golden, Direction::Long);
        let r_off = run(
            &never_firing_long_plan(),
            &candles,
            Granularity::H1,
            live,
            expires(),
            off,
        )
        .await;
        assert!(marks(&r_off).is_empty(), "none disables marking");
        let report = crate::report::render(
            &never_firing_long_plan(),
            &r_off,
            true,
            false,
            &[],
            None,
            &off,
        );
        assert!(
            !report.contains("Candle detector"),
            "summary omitted when off:\n{report}"
        );
    }

    /// A LONG `pine_pattern` enter whose TP sits BELOW the entry (wrong side for a
    /// long → `EntryOutsideRange`), so the golden bullish pinbar fires the
    /// detector but the bracket can't resolve. `needs_golden` so only the golden
    /// bar is a candidate. The plan must record the decline reason.
    fn golden_enter_resolve_fails_plan() -> TradePlan {
        serde_json::from_str(
            r#"{
                "trade_id": "gold-decline",
                "instrument": "EUR_USD",
                "direction": "long",
                "granularity": "h1",
                "pip_size": 0.0001,
                "rules": [{
                    "rule_id": "05-enter",
                    "trigger": { "type": "pine_pattern", "dir": "long" },
                    "fire_mode": "once",
                    "intent": {
                        "v": 1, "id": "gold-decline-enter", "not_after": "2099-01-01T00:00:00Z",
                        "action": "enter", "instrument": "EUR_USD", "direction": "long",
                        "needs_golden": true,
                        "entry": { "type": "stop", "from": "signal_high", "offset_pips": 0.0 },
                        "stop_loss": { "from": "signal_low", "offset_pips": 0.0 },
                        "take_profit": { "absolute": 0.90 },
                        "broker": "oanda", "trade_id": "gold-decline", "max_retries": 0
                    }
                }]
            }"#,
        )
        .expect("parse golden-enter resolve-fail plan")
    }

    #[tokio::test]
    async fn golden_enter_decline_reason_surfaces_on_the_bar_and_in_summary() {
        let (candles, live) = window_with_golden_pinbar();
        let cfg =
            DetectorMarkConfig::new(DirectionFilter::With, GoldenFilter::Golden, Direction::Long);
        let plan = golden_enter_resolve_fails_plan();
        let r = run(&plan, &candles, Granularity::H1, live, expires(), cfg).await;

        // The golden fired the detector but the enter declined — nothing fired.
        assert!(
            r.fires.iter().all(|f| f.fired.rule_id != "05-enter"),
            "the resolve-failed enter must not fire"
        );

        // The decline is recorded on the golden bar's trace with a reason.
        let declines: Vec<&String> = r
            .traces
            .iter()
            .flat_map(|t| t.entry_declines.iter())
            .collect();
        assert_eq!(declines.len(), 1, "exactly one decline: {declines:?}");
        assert!(
            declines[0].contains("range") || declines[0].contains("R="),
            "decline carries the resolve reason: {}",
            declines[0]
        );

        // The always-on report surfaces it (no --verbose needed), and --verbose
        // shows the ✗ line right under the ◆ GOLDEN mark on the same bar.
        let plain = crate::report::render(&plan, &r, true, false, &[], None, &cfg);
        assert!(
            plain.contains("Entry declines:"),
            "always-on decline rollup present:\n{plain}"
        );
        let verbose = crate::report::render(&plan, &r, true, true, &[], None, &cfg);
        assert!(
            verbose.contains("◆ GOLDEN") && verbose.contains("✗ not entered:"),
            "verbose joins the golden mark and the decline on the bar:\n{verbose}"
        );
    }

    /// A window whose warm-up bars have a WIDE range (large ATR) so the trailing
    /// bullish pinbar is a valid detected signal but its size is BELOW ATR — i.e.
    /// **non-golden**. Mirrors `window_with_golden_pinbar`'s geometry, scaled so
    /// the pinbar is small relative to the warm-up volatility.
    fn window_with_non_golden_pinbar() -> (Vec<EngineCandle>, DateTime<Utc>) {
        // 30 WIDE warm-up bars (range 0.20 → large ATR ≈ 0.20).
        let mut candles: Vec<EngineCandle> = (0..30)
            .map(|i| ohlc(i * 3600, 1.10, 1.20, 1.00, 1.10))
            .collect();
        // bar 30: prior bar whose low 1.085 the pinbar must undercut.
        candles.push(ohlc(30 * 3600, 1.10, 1.11, 1.085, 1.09));
        // bar 31: a SMALL bullish pinbar — range 1.06..1.11 (0.05 ≪ ATR ≈ 0.20),
        // body in the top quartile, long lower wick, low 1.06 < prior low 1.085.
        // Detected as a bullish reversal, but size 0.05 < ATR → NOT golden.
        candles.push(ohlc(31 * 3600, 1.10, 1.11, 1.06, 1.105));
        // bar 32: trailing flat bar so bar 31 is a fully-closed live bar.
        candles.push(ohlc(32 * 3600, 1.105, 1.11, 1.10, 1.105));
        let live = candles[25].time;
        (candles, live)
    }

    /// A `needs_golden` long enter whose trigger sits far away so it never fires;
    /// a non-golden signal on any bar produces the `not golden` decline.
    fn needs_golden_never_fires_plan() -> TradePlan {
        serde_json::from_str(
            r#"{
                "trade_id": "needs-golden",
                "instrument": "EUR_USD",
                "direction": "long",
                "granularity": "h1",
                "pip_size": 0.0001,
                "rules": [{
                    "rule_id": "05-enter",
                    "trigger": { "type": "pine_pattern", "dir": "long" },
                    "fire_mode": "once",
                    "intent": {
                        "v": 1, "id": "needs-golden-enter", "not_after": "2099-01-01T00:00:00Z",
                        "action": "enter", "instrument": "EUR_USD", "direction": "long",
                        "needs_golden": true,
                        "entry": { "type": "stop", "from": "signal_high", "offset_pips": 0.0 },
                        "stop_loss": { "from": "signal_low", "offset_pips": 0.0 },
                        "take_profit": { "absolute": 9.99 },
                        "broker": "oanda", "trade_id": "needs-golden", "max_retries": 0
                    }
                }]
            }"#,
        )
        .expect("parse needs-golden plan")
    }

    /// The bug: with `--candle-detector-golden golden` (the default), a
    /// `needs golden but signal is not golden` decline is tautological noise —
    /// the operator asked to see golden-only candles. It must be suppressed from
    /// BOTH the per-bar trace and the always-on rollup. Under `both`, it's kept.
    #[tokio::test]
    async fn not_golden_decline_suppressed_under_golden_only_filter() {
        let (candles, live) = window_with_non_golden_pinbar();
        let plan = needs_golden_never_fires_plan();

        // Default view: golden-only. The non-golden pinbar declines with the
        // NOT_GOLDEN reason, which must be suppressed.
        let golden_only =
            DetectorMarkConfig::new(DirectionFilter::With, GoldenFilter::Golden, Direction::Long);
        let r = run(
            &plan,
            &candles,
            Granularity::H1,
            live,
            expires(),
            golden_only,
        )
        .await;

        // Sanity: the signal is genuinely non-golden (else this test proves
        // nothing). Under a `both` golden filter it's marked and NOT golden.
        let both =
            DetectorMarkConfig::new(DirectionFilter::With, GoldenFilter::Both, Direction::Long);
        let r_both = run(&plan, &candles, Granularity::H1, live, expires(), both).await;
        let marked: Vec<&super::super::verbose::DetectedMark> = r_both
            .traces
            .iter()
            .filter_map(|t| t.detected.as_ref())
            .collect();
        assert!(
            marked.iter().any(|m| !m.golden),
            "the pinbar is a detected NON-golden signal: {marked:?}"
        );

        // Under golden-only: the NOT_GOLDEN decline is gone from every trace…
        let hidden = r
            .traces
            .iter()
            .flat_map(|t| t.entry_declines.iter())
            .any(|d| d == trade_control_core::plan_eval::NOT_GOLDEN_DECLINE);
        assert!(
            !hidden,
            "not-golden decline suppressed from traces under golden-only"
        );
        // …and from the always-on rollup + verbose bar lines.
        let report = crate::report::render(&plan, &r, true, true, &[], None, &golden_only);
        assert!(
            !report.contains("needs golden but signal is not golden"),
            "not-golden decline absent from the report under golden-only:\n{report}"
        );

        // Under `both`: the decline IS kept (the operator wants non-golden info).
        let kept = r_both
            .traces
            .iter()
            .flat_map(|t| t.entry_declines.iter())
            .any(|d| d == trade_control_core::plan_eval::NOT_GOLDEN_DECLINE);
        assert!(kept, "not-golden decline retained under the `both` filter");
    }

    /// A single-enter plan WITH a break-and-close prep rule whose trigger never
    /// fires — so the spine stays `AwaitBreakAndClose` and the pine enter is
    /// never even considered. A golden pinbar in the window is marked, fires no
    /// enter, and produces no decline (the trigger never ran), so the replay
    /// annotates it `not taken: requires break-and-close`.
    fn break_and_close_pending_plan() -> TradePlan {
        serde_json::from_str(
            r#"{
                "trade_id": "bc-pending",
                "instrument": "EUR_USD",
                "direction": "long",
                "granularity": "h1",
                "pip_size": 0.0001,
                "rules": [
                    {
                        "rule_id": "03-prep-break-and-close",
                        "trigger": { "type": "horizontal_cross", "level": 9.99, "dir": "up", "bar": "on_close" },
                        "fire_mode": "once",
                        "intent": {
                            "v": 1, "id": "bc-pending-prep", "not_after": "2099-01-01T00:00:00Z",
                            "action": "prep", "instrument": "EUR_USD",
                            "broker": "oanda", "trade_id": "bc-pending"
                        }
                    },
                    {
                        "rule_id": "05-enter",
                        "trigger": { "type": "pine_pattern", "dir": "long" },
                        "fire_mode": "once",
                        "intent": {
                            "v": 1, "id": "bc-pending-enter", "not_after": "2099-01-01T00:00:00Z",
                            "action": "enter", "instrument": "EUR_USD", "direction": "long",
                            "needs_golden": true,
                            "requires_preps": ["break-and-close"],
                            "entry": { "type": "stop", "from": "signal_high", "offset_pips": 0.0 },
                            "stop_loss": { "from": "signal_low", "offset_pips": 0.0 },
                            "take_profit": { "absolute": 9.99 },
                            "broker": "oanda", "trade_id": "bc-pending", "max_retries": 0
                        }
                    }
                ]
            }"#,
        )
        .expect("parse break-and-close-pending plan")
    }

    /// The user's ask: a marked golden that never *fired* an enter (because a
    /// precondition — here break-and-close — is unmet) should say WHY it wasn't
    /// taken. Distinct from the fired-then-declined `EntryDecline` surface.
    #[tokio::test]
    async fn golden_not_taken_surfaces_unmet_precondition() {
        let (candles, live) = window_with_golden_pinbar();
        let plan = break_and_close_pending_plan();
        let cfg =
            DetectorMarkConfig::new(DirectionFilter::With, GoldenFilter::Golden, Direction::Long);
        let r = run(&plan, &candles, Granularity::H1, live, expires(), cfg).await;

        // The enter never fires: break-and-close never crossed 9.99, so the
        // spine stays AwaitBreakAndClose and the enter is never considered.
        assert!(
            r.fires.iter().all(|f| f.fired.rule_id != "05-enter"),
            "the enter must not fire while break-and-close is pending"
        );

        // The golden bar carries a `not_taken` reason naming break-and-close, and
        // NO entry-decline (the trigger never fired, so there's nothing to decline).
        let marked_bar = r
            .traces
            .iter()
            .find(|t| t.detected.is_some())
            .expect("the golden pinbar is marked");
        assert!(
            marked_bar.entry_declines.is_empty(),
            "no EntryDecline on a never-fired enter: {:?}",
            marked_bar.entry_declines
        );
        let reason = marked_bar
            .not_taken
            .as_ref()
            .expect("golden bar carries a not_taken reason");
        assert!(
            reason.contains("break-and-close"),
            "not_taken names the outstanding precondition: {reason}"
        );

        // And it renders under --verbose right beneath the ◆ mark.
        let verbose = crate::report::render(&plan, &r, true, true, &[], None, &cfg);
        assert!(
            verbose.contains("◆ GOLDEN") && verbose.contains("✗ not taken:"),
            "verbose joins the golden mark and the not-taken reason:\n{verbose}"
        );
    }

    /// A golden that DID fire + was declined keeps the `EntryDecline` path and
    /// gets NO `not_taken` (the two surfaces are mutually exclusive per bar).
    #[tokio::test]
    async fn fired_then_declined_golden_has_no_not_taken() {
        let (candles, live) = window_with_golden_pinbar();
        let plan = golden_enter_resolve_fails_plan();
        let cfg =
            DetectorMarkConfig::new(DirectionFilter::With, GoldenFilter::Golden, Direction::Long);
        let r = run(&plan, &candles, Granularity::H1, live, expires(), cfg).await;

        let marked_bar = r
            .traces
            .iter()
            .find(|t| t.detected.is_some())
            .expect("golden marked");
        assert!(
            !marked_bar.entry_declines.is_empty(),
            "this golden fired and was declined (has an EntryDecline)"
        );
        assert!(
            marked_bar.not_taken.is_none(),
            "a declined bar must NOT also carry a not_taken: {:?}",
            marked_bar.not_taken
        );
    }

    // --- PR 4b-3: spread-hour resting order cancelled via the shared lifecycle ---

    /// An OHLC bid==ask==mid bar at an RFC3339 instant (the spread-hour golden
    /// needs real calendar times so `is_spread_hour` reads AUD/CHF's baked hours).
    fn ohlc_at(rfc3339: &str, o: f64, h: f64, l: f64, c: f64) -> EngineCandle {
        let time: DateTime<Utc> = rfc3339.parse().expect("valid rfc3339");
        EngineCandle {
            time,
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

    /// A single-shot SHORT plan (the AUD/CHF 2026-07-08 origin case, on EUR/USD so
    /// the spread hour is a single, un-wrapped block): the enter fires on an
    /// OnClose down-cross of 1.1010 and rests a short stop-entry at 1.1000
    /// (SL 1.1020, TP 1.0950). The trigger is first reached on the 21:00Z
    /// spread-hour bar.
    ///
    /// EUR/USD's baked spread hours are the single hour 21:00Z (NY-close block),
    /// so 22:00Z/23:00Z are genuinely clean — the restore re-drive lands there and
    /// fills. (AUD/CHF's own block runs 21:00–05:00Z, so its first clean bar is
    /// 06:00Z; the mechanism is identical, EUR/USD just keeps the golden compact.)
    fn eurusd_spread_hour_plan() -> TradePlan {
        serde_json::from_str(
            r#"{
                "trade_id": "eurusd-sh",
                "instrument": "EUR/USD",
                "direction": "short",
                "granularity": "h1",
                "pip_size": 0.0001,
                "rules": [
                    {
                        "rule_id": "05-enter",
                        "trigger": { "type": "horizontal_cross", "level": 1.1010, "dir": "down", "bar": "on_close" },
                        "fire_mode": "once",
                        "intent": {
                            "v": 1, "id": "eurusd-sh-enter", "not_after": "2099-01-01T00:00:00Z",
                            "action": "enter", "instrument": "EUR/USD", "direction": "short",
                            "entry": { "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.1000 },
                            "stop_loss": { "absolute": 1.1020 },
                            "take_profit": { "absolute": 1.0950 },
                            "broker": "tradenation", "trade_id": "eurusd-sh", "max_retries": 0,
                            "pip_size": 0.0001
                        }
                    }
                ]
            }"#,
        )
        .expect("parse EUR/USD spread-hour plan")
    }

    /// GOLDEN (AUD/CHF 2026-07-08 origin): a **single-shot** resting short-stop
    /// (`max_retries: 0`) whose trigger is first reached on the 21:00Z spread-hour
    /// spike still ends up filling on the next **clean** bar (22:00Z), NOT inside
    /// the 12p-spread rubbish spike.
    ///
    /// IMPORTANT — what this test does and does NOT exercise: because the enter is
    /// single-shot, the engine retires the plan (`Phase::Done`) the moment it fires
    /// (19:00Z), so the replay loop **breaks before ever reaching the 20:00Z
    /// spread-hour bar** — the shared `pending_order_lifecycle` never lists, cancels
    /// or restores this order. The clean-bar fill here is produced solely by
    /// `find_fill`'s spread-hour spike-skip (the v88 mechanism), *not* by the
    /// cancel→restore lifecycle. The lifecycle cancel→restore path — which only runs
    /// for a **multi-shot** enter whose plan stays `AwaitEntry` into the spread hour
    /// — is exercised by
    /// [`multishot_spread_hour_order_is_cancelled_and_deduped_restore_shows_no_fill`]
    /// below. (The former docstring claimed this test drove the cancel→restore
    /// mechanism; it never did — see PR 4b-4.)
    #[tokio::test]
    async fn spread_hour_order_is_cancelled_then_restored_and_fills_on_the_clean_bar() {
        // The lifecycle keys on the bar's CLOSE (`now = open + 1h`); the find_fill
        // spread-hour skip keys on the bar's OPEN. EUR/USD's only spread hour is
        // 21:00Z, so the timeline is:
        //   19:00Z bar (close 20:00Z, clean): enter fires, short stop rests @1.1000
        //                                     (high stays above 1.1000 — no touch).
        //   20:00Z bar (close 21:00Z, SPREAD HOUR): lifecycle CANCELS the resting
        //                                     order (now=21:00Z); no touch here.
        //   21:00Z bar (open 21:00Z spike, close 22:00Z clean): lifecycle RESTORES
        //                                     it (now=22:00Z clean); the low taps
        //                                     1.1000 but find_fill skips the spike
        //                                     open — no fill.
        //   22:00Z bar (open+close clean): reaches 1.1000 → FILLS here.
        //   23:00Z bar: runs to TP 1.0950.
        // 10 warm-up bars above the 1.1010 enter level so the live window starts at
        // 19:00Z (past the SEED_BARS floor).
        let mut candles: Vec<EngineCandle> = (9..19)
            .map(|h| {
                ohlc_at(
                    &format!("2026-07-08T{h:02}:00:00Z"),
                    1.1020,
                    1.1022,
                    1.1018,
                    1.1020,
                )
            })
            .collect();
        // 19:00Z: closes below 1.1010 → 05-enter fires, short stop rests @1.1000
        // (low 1.1006 stays above the trigger — no touch).
        candles.push(ohlc_at(
            "2026-07-08T19:00:00Z",
            1.1016,
            1.1017,
            1.1006,
            1.1008,
        ));
        // 20:00Z (close 21:00Z = SPREAD HOUR): lifecycle cancels; low 1.1005 keeps
        // the order untouched so nothing fills before the cancel.
        candles.push(ohlc_at(
            "2026-07-08T20:00:00Z",
            1.1008,
            1.1009,
            1.1005,
            1.1007,
        ));
        // 21:00Z (open = spread hour spike): straddles 1.1000 (low 1.0995) but the
        // find_fill spike-open skip blocks the fill; lifecycle restores (now clean).
        candles.push(ohlc_at(
            "2026-07-08T21:00:00Z",
            1.1002,
            1.1004,
            1.0995,
            1.0999,
        ));
        // 22:00Z CLEAN (open + close clean): re-reaches 1.1000 → the restored order
        // fills here.
        candles.push(ohlc_at(
            "2026-07-08T22:00:00Z",
            1.1001,
            1.1003,
            1.0994,
            1.0997,
        ));
        // 23:00Z: runs to TP 1.0950.
        candles.push(ohlc_at(
            "2026-07-08T23:00:00Z",
            1.0995,
            1.0997,
            1.0948,
            1.0951,
        ));
        let live_at: DateTime<Utc> = "2026-07-08T19:00:00Z".parse().unwrap();
        let expires_at: DateTime<Utc> = "2026-07-10T00:00:00Z".parse().unwrap();

        let r = run(
            &eurusd_spread_hour_plan(),
            &candles,
            Granularity::H1,
            live_at,
            expires_at,
            no_marks(),
        )
        .await;

        // The enter fired (the down-cross), placing the resting short-stop.
        let enter = r
            .fires
            .iter()
            .find(|f| f.fired.rule_id == "05-enter")
            .expect("the short enter must fire on the 19:00Z down-cross");

        // The lifecycle cancelled the resting order in the 21:00Z spread hour,
        // then RESTORED (re-drove) it on the next clean bar — so it fills on the
        // 22:00Z clean bar and runs to TP. The observable: a TOOK PROFIT whose
        // fill_at is 22:00Z, NOT the 21:00Z rubbish spike.
        let realized = enter
            .realized
            .as_ref()
            .expect("the restored order must have a realized outcome (cancel→restore→fill)");
        assert_eq!(
            realized.kind,
            crate::report::FillKind::TookProfit,
            "the restored order fills on the clean bar and runs to TP, got {:?}",
            realized.kind
        );
        let clean_bar: DateTime<Utc> = "2026-07-08T22:00:00Z".parse().unwrap();
        let spike_bar: DateTime<Utc> = "2026-07-08T21:00:00Z".parse().unwrap();
        assert_eq!(
            realized.fill_at, clean_bar,
            "the fill must land on the 22:00Z clean bar, not the 21:00Z spread-hour spike"
        );
        assert_ne!(
            realized.fill_at, spike_bar,
            "the order must NOT fill on the rubbish spread-hour spike"
        );

        // The report reflects the taken TP (exactly one win, no loss).
        let report = crate::report::render(
            &eurusd_spread_hour_plan(),
            &r,
            true,
            false,
            &[],
            None,
            &no_marks(),
        );
        assert!(
            report.contains("TOOK PROFIT"),
            "the restored order must show its TP fill:\n{report}"
        );
        assert!(
            report.contains("TP: 1  SL: 0"),
            "exactly one taken position (the restored order's TP), no loss:\n{report}"
        );
    }

    /// The predicate-false twin: the SAME setup on a CLEAN hour is NOT cancelled
    /// by the lifecycle — the resting order fills as normal. Proves the lifecycle
    /// only acts in a spread hour (the ON trigger), so it can't be silently
    /// cancelling every resting order.
    #[tokio::test]
    async fn clean_hour_resting_order_is_not_cancelled_and_fills() {
        // Identical geometry in the 00:00Z..12:00Z clean stretch (no EUR/USD spread
        // hour — its only spread hour is 21:00Z): 10 warm-up bars, the enter fires
        // at 12:00Z and the stop fills on the 13:00Z bar, uncancelled.
        let mut candles: Vec<EngineCandle> = (2..12)
            .map(|h| {
                ohlc_at(
                    &format!("2026-07-08T{h:02}:00:00Z"),
                    1.1020,
                    1.1022,
                    1.1018,
                    1.1020,
                )
            })
            .collect();
        // 12:00Z: closes below 1.1010 → enter fires, short stop rests @1.1000.
        candles.push(ohlc_at(
            "2026-07-08T12:00:00Z",
            1.1016,
            1.1017,
            1.1006,
            1.1008,
        ));
        // 13:00Z CLEAN: straddles 1.1000 → fills here (no spread-hour cancel).
        candles.push(ohlc_at(
            "2026-07-08T13:00:00Z",
            1.1002,
            1.1004,
            1.0995,
            1.0999,
        ));
        // 14:00Z: runs to TP 1.0950.
        candles.push(ohlc_at(
            "2026-07-08T14:00:00Z",
            1.0995,
            1.0997,
            1.0948,
            1.0951,
        ));
        let live_at: DateTime<Utc> = "2026-07-08T12:00:00Z".parse().unwrap();
        let expires_at: DateTime<Utc> = "2026-07-10T00:00:00Z".parse().unwrap();

        let r = run(
            &eurusd_spread_hour_plan(),
            &candles,
            Granularity::H1,
            live_at,
            expires_at,
            no_marks(),
        )
        .await;

        let enter = r
            .fires
            .iter()
            .find(|f| f.fired.rule_id == "05-enter")
            .expect("the short enter must fire on the 12:00Z down-cross");
        let realized = enter
            .realized
            .as_ref()
            .expect("a clean-hour resting order must fill (not cancelled)");
        assert!(
            realized.kind.is_taken(),
            "the clean-hour order fills — a taken outcome, got {:?}",
            realized.kind
        );
    }

    /// The **multi-shot** twin of [`eurusd_spread_hour_plan`] (`max_retries: 1`).
    /// A multi-shot enter keeps the plan in `AwaitEntry` after firing (the engine
    /// does NOT retire it), so the replay loop runs on into the spread hour and the
    /// shared `pending_order_lifecycle` genuinely lists + cancels the resting order.
    fn eurusd_spread_hour_plan_multishot() -> TradePlan {
        serde_json::from_str(
            r#"{
                "trade_id": "eurusd-sh-ms",
                "instrument": "EUR/USD",
                "direction": "short",
                "granularity": "h1",
                "pip_size": 0.0001,
                "rules": [
                    {
                        "rule_id": "05-enter",
                        "trigger": { "type": "horizontal_cross", "level": 1.1010, "dir": "down", "bar": "on_close" },
                        "fire_mode": "once",
                        "intent": {
                            "v": 1, "id": "eurusd-sh-ms-enter", "not_after": "2099-01-01T00:00:00Z",
                            "action": "enter", "instrument": "EUR/USD", "direction": "short",
                            "entry": { "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.1000 },
                            "stop_loss": { "absolute": 1.1020 },
                            "take_profit": { "absolute": 1.0950 },
                            "broker": "tradenation", "trade_id": "eurusd-sh-ms", "max_retries": 1,
                            "pip_size": 0.0001
                        }
                    }
                ]
            }"#,
        )
        .expect("parse EUR/USD multi-shot spread-hour plan")
    }

    /// GOLDEN (the real AUD/CHF QM cancel→restore mechanism, on EUR/USD's compact
    /// 1h block): a **multi-shot** resting order still pending when the 21:00Z
    /// spread hour arrives is **cancelled by the shared lifecycle** on the spike
    /// bar (engine → `broker.cancel_order`), held cancelled through the block, then
    /// **RESTORED (re-driven → reactivated, `cancelled=false`)** once the block
    /// lifts, so it **fills on the clean 22:00Z bar** and runs to TP. The full
    /// cancel→restore→fill sequence the live path runs, end to end.
    ///
    /// This is the mechanism PR 4b-4 repaired. Two bugs had to be fixed for the
    /// restore to actually happen (both in the SHARED core path, so LIVE gets the
    /// same fix):
    ///   1. **Retry-gate bypass on restore** (`run_enter(.., restore = true)`).
    ///      The RAIL-7 re-drive carries the ORIGINAL fire's `shell.time`, which the
    ///      retry gate had already marked seen — so a multi-shot order's restore was
    ///      `retry-fire-replay`-REJECTED and never re-placed. A restore is a
    ///      re-placement of a known-cancelled order, not a fresh fire, so it now
    ///      skips the retry gate (exactly as single-shot already did).
    ///   2. **Reactivate match on the STABLE key.** The restore re-drives through
    ///      `run_enter`, which re-floors the SL at the restore bar, so the
    ///      re-floored SL differs from the original. `reactivate_matching_cancelled`
    ///      matches on instrument + direction + entry trigger (stable), not the
    ///      re-floored SL/TP.
    ///
    /// Asserts on BOTH `enter.realized` (the ledger) AND the rendered report text /
    /// `Net R` / `TP:/SL:`, so the split-brain (report re-simulating a phantom fill
    /// blind to the cancel) can never regress silently. EUR/USD's block is 1h
    /// (21:00–22:00Z) so the block lifts well inside the record's backstop — it does
    /// NOT hit the 8h-block-vs-3h-backstop record-expiry race that AUD/CHF's
    /// overnight block does (a separate, flagged follow-up).
    #[tokio::test]
    async fn multishot_spread_hour_order_is_cancelled_then_restored_and_fills() {
        // 10 warm-up bars above 1.1010 so the live window starts at 19:00Z.
        let mut candles: Vec<EngineCandle> = (9..19)
            .map(|h| {
                ohlc_at(
                    &format!("2026-07-08T{h:02}:00:00Z"),
                    1.1020,
                    1.1022,
                    1.1018,
                    1.1020,
                )
            })
            .collect();
        // 19:00Z (close 20:00Z, clean): closes below 1.1010 → 05-enter fires, short
        // stop rests @1.1000. Low 1.1006 stays above the trigger — no touch. The
        // multi-shot enter keeps the plan in AwaitEntry, so the loop runs on.
        candles.push(ohlc_at(
            "2026-07-08T19:00:00Z",
            1.1016,
            1.1017,
            1.1006,
            1.1008,
        ));
        // 20:00Z (close 21:00Z = SPREAD HOUR): low 1.1005 keeps the order untouched;
        // the lifecycle CANCELS the still-resting order here (now=21:00Z).
        candles.push(ohlc_at(
            "2026-07-08T20:00:00Z",
            1.1008,
            1.1009,
            1.1005,
            1.1007,
        ));
        // 21:00Z (open = spread-hour spike): straddles 1.1000 (low 1.0995), but the
        // order is cancelled (held through the block) so no fill lands here.
        candles.push(ohlc_at(
            "2026-07-08T21:00:00Z",
            1.1002,
            1.1004,
            1.0995,
            1.0999,
        ));
        // 22:00Z CLEAN (block lifted): the lifecycle RESTORES (reactivates) the
        // order, it rests again, and price re-reaches 1.1000 → FILLS here.
        candles.push(ohlc_at(
            "2026-07-08T22:00:00Z",
            1.1001,
            1.1003,
            1.0994,
            1.0997,
        ));
        // 23:00Z: runs to TP 1.0950.
        candles.push(ohlc_at(
            "2026-07-08T23:00:00Z",
            1.0995,
            1.0997,
            1.0948,
            1.0951,
        ));
        let live_at: DateTime<Utc> = "2026-07-08T19:00:00Z".parse().unwrap();
        let expires_at: DateTime<Utc> = "2026-07-10T00:00:00Z".parse().unwrap();

        let r = run(
            &eurusd_spread_hour_plan_multishot(),
            &candles,
            Granularity::H1,
            live_at,
            expires_at,
            no_marks(),
        )
        .await;

        // The enter fired (the down-cross) and placed the resting short-stop.
        let enter = r
            .fires
            .iter()
            .find(|f| f.fired.rule_id == "05-enter")
            .expect("the short enter must fire on the 19:00Z down-cross");

        // MECHANISM: cancelled in the block, RESTORED (reactivated) when it lifts,
        // fills on the clean 22:00Z bar and runs to TP — a real ledger outcome, NOT
        // `None`. The fill must land on 22:00Z (the clean bar), never the 21:00Z
        // spike (the `find_fill` spread-hour skip still blocks the rubbish bar).
        let realized = enter
            .realized
            .as_ref()
            .expect("the restored order must have a realized outcome (cancel→restore→fill)");
        assert_eq!(
            realized.kind,
            crate::report::FillKind::TookProfit,
            "the restored order fills on the clean bar and runs to TP, got {:?}",
            realized.kind
        );
        assert_eq!(
            realized.fill_at,
            "2026-07-08T22:00:00Z".parse::<DateTime<Utc>>().unwrap(),
            "the restored order must fill on the 22:00Z clean bar, not the 21:00Z spike"
        );

        // REPORT TEXT agrees with the ledger — the split-brain guard. Exactly one
        // taken position (the restored order's TP), no loss, positive Net R.
        let report = crate::report::render(
            &eurusd_spread_hour_plan_multishot(),
            &r,
            true,
            false,
            &[],
            None,
            &no_marks(),
        );
        assert!(
            report.contains("TOOK PROFIT"),
            "the restored order must show its TP fill in the journal:\n{report}"
        );
        assert!(
            !report.contains("NO FILL"),
            "the restored order fills — the journal must NOT show a NO FILL line:\n{report}"
        );
        assert!(
            report.contains("TP: 1  SL: 0"),
            "exactly one taken position (the restored order's TP), no loss:\n{report}"
        );
    }

    /// A **multi-shot AUD/CHF** plan — the real 2026-07-08 QM instrument, whose
    /// baked spread block is the 8h overnight run (21:00–05:00Z), NOT a single
    /// hour. Exercises the backstop SPLIT: the cancel-record's TTL must be sized to
    /// the 8h block (concern 1) so it OUTLIVES the block; the order is then restored
    /// at the block lift by the normal `off_now` path, NOT the safety ceiling. This
    /// is the regression guard for the TTL-vs-block relationship — with the old flat
    /// 3h TTL the record expired at 00:00Z (mid-block) and the order was never
    /// restored (0R).
    fn audchf_spread_block_plan_multishot() -> TradePlan {
        serde_json::from_str(
            r#"{
                "trade_id": "audchf-sh-ms",
                "instrument": "AUD/CHF",
                "direction": "short",
                "granularity": "h1",
                "pip_size": 0.0001,
                "rules": [
                    {
                        "rule_id": "05-enter",
                        "trigger": { "type": "horizontal_cross", "level": 0.5610, "dir": "down", "bar": "on_close" },
                        "fire_mode": "once",
                        "intent": {
                            "v": 1, "id": "audchf-sh-ms-enter", "not_after": "2099-01-01T00:00:00Z",
                            "action": "enter", "instrument": "AUD/CHF", "direction": "short",
                            "entry": { "type": "stop", "from": "close", "offset_pips": 0.0, "at": 0.5600 },
                            "stop_loss": { "absolute": 0.5620 },
                            "take_profit": { "absolute": 0.5560 },
                            "broker": "tradenation", "trade_id": "audchf-sh-ms", "max_retries": 1,
                            "pip_size": 0.0001
                        }
                    }
                ]
            }"#,
        )
        .expect("parse AUD/CHF multi-shot spread-block plan")
    }

    /// GOLDEN (multi-hour block): a multi-shot AUD/CHF resting order cancelled at
    /// the 21:00Z block start is held cancelled ACROSS the full 8h overnight block
    /// (its record TTL sized to the block, so it never expires mid-block), then
    /// restored at the block lift and filled on a clean post-block bar → TP. The
    /// TTL-vs-block regression guard: assert the fill lands AFTER the block start
    /// (a genuinely deferred entry) and the journal shows a taken TP, not a 0R
    /// no-fill.
    #[tokio::test]
    async fn multishot_multi_hour_block_order_is_held_then_restored_at_block_end() {
        // Warm-up bars above 0.5610 through the daytime (clean) hours, so the live
        // window starts at 19:00Z (2 clean bars before the 21:00Z block start).
        let mut candles: Vec<EngineCandle> = (9..19)
            .map(|h| {
                ohlc_at(
                    &format!("2026-07-08T{h:02}:00:00Z"),
                    0.5620,
                    0.5622,
                    0.5618,
                    0.5620,
                )
            })
            .collect();
        // 19:00Z (clean): closes below 0.5610 → 05-enter fires, short stop rests
        // @0.5600. Low 0.5606 stays above the trigger — no touch. Multi-shot keeps
        // the plan AwaitEntry so the loop runs on into the block.
        candles.push(ohlc_at(
            "2026-07-08T19:00:00Z",
            0.5616,
            0.5617,
            0.5606,
            0.5608,
        ));
        // 20:00Z (close 21:00Z = BLOCK START): low 0.5605 keeps the order untouched;
        // the lifecycle CANCELS the resting order here (now=21:00Z, in-block).
        candles.push(ohlc_at(
            "2026-07-08T20:00:00Z",
            0.5608,
            0.5609,
            0.5605,
            0.5607,
        ));
        // 21:00Z .. 04:00Z — the 8h overnight block. The order is held cancelled the
        // whole way; even bars whose low dips to the trigger must NOT fill (order is
        // cancelled). Keep lows just above 0.5600 so geometry is unambiguous.
        for h in [21u32, 22, 23] {
            candles.push(ohlc_at(
                &format!("2026-07-08T{h:02}:00:00Z"),
                0.5606,
                0.5608,
                0.5602,
                0.5605,
            ));
        }
        for h in [0u32, 1, 2, 3, 4] {
            candles.push(ohlc_at(
                &format!("2026-07-09T{h:02}:00:00Z"),
                0.5606,
                0.5608,
                0.5602,
                0.5605,
            ));
        }
        // 05:00Z / 06:00Z — the block lifts here. The order is restored, rests, and
        // the 06:00Z bar's low 0.5599 reaches the 0.5600 trigger → FILLS.
        candles.push(ohlc_at(
            "2026-07-09T05:00:00Z",
            0.5605,
            0.5607,
            0.5601,
            0.5603,
        ));
        candles.push(ohlc_at(
            "2026-07-09T06:00:00Z",
            0.5602,
            0.5603,
            0.5599,
            0.5601,
        ));
        // 07:00Z: runs to TP 0.5560.
        candles.push(ohlc_at(
            "2026-07-09T07:00:00Z",
            0.5600,
            0.5601,
            0.5558,
            0.5560,
        ));
        let live_at: DateTime<Utc> = "2026-07-08T19:00:00Z".parse().unwrap();
        let expires_at: DateTime<Utc> = "2026-07-11T00:00:00Z".parse().unwrap();

        let r = run(
            &audchf_spread_block_plan_multishot(),
            &candles,
            Granularity::H1,
            live_at,
            expires_at,
            no_marks(),
        )
        .await;

        let enter = r
            .fires
            .iter()
            .find(|f| f.fired.rule_id == "05-enter")
            .expect("the short enter must fire on the 19:00Z down-cross");

        // MECHANISM: cancelled at 21:00Z, held across the full 8h block (the record
        // TTL outlives it), restored at the block lift, filled on a clean post-block
        // bar. The realized fill must exist (not None) and land AFTER the 21:00Z
        // block start — a genuinely deferred entry, the operator's intent.
        let realized = enter
            .realized
            .as_ref()
            .expect("the held-then-restored order must fill after the block (cancel→hold→restore)");
        assert!(
            realized.kind.is_taken(),
            "the restored order fills after the block — a taken outcome, got {:?}",
            realized.kind
        );
        let block_start: DateTime<Utc> = "2026-07-08T21:00:00Z".parse().unwrap();
        assert!(
            realized.fill_at > block_start,
            "the entry must be DEFERRED past the 21:00Z block start (held cancelled across the \
             block), filled at {}",
            realized.fill_at
        );

        // REPORT TEXT: a taken position with a positive Net R, NOT a 0R no-fill (the
        // old flat-3h-TTL bug expired the record mid-block → NO FILL / 0R).
        let report = crate::report::render(
            &audchf_spread_block_plan_multishot(),
            &r,
            true,
            false,
            &[],
            None,
            &no_marks(),
        );
        assert!(
            !report.contains("NO FILL"),
            "the deferred order fills after the block — NOT a NO FILL / 0R:\n{report}"
        );
        assert!(
            report.contains("TP: 1  SL: 0"),
            "exactly one taken position (the deferred order's TP):\n{report}"
        );
    }
}
