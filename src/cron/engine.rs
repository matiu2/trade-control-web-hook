//! Server-side trade-plan engine tick — the cron-driven evaluator that
//! replaces TradingView's paid alerts.
//!
//! Where [`sweep`](super::sweep) walks pending broker orders, this walks every
//! registered [`TradePlan`]. For each plan it loads the persisted
//! [`PlanState`], fetches the candles that have closed since the watermark,
//! runs the pure [`evaluate_plan`] FSM, persists the advanced state, and
//! dispatches each fired intent through the *same* handlers the webhook uses
//! (`run_enter` / `run_close` / the veto + prep handlers). The plan was already
//! HMAC-verified at register time, so the engine synthesises a [`Verified`]
//! directly from the triggering candle — no re-verification.
//!
//! # Seed-without-firing
//!
//! A plan's very first tick must **not** retroactively fire conditions that
//! were already true when it was registered (a fresh TV alert doesn't back-fire
//! on history either). So when there's no prior state, the engine fetches a
//! short back-window, seeds the watermark + `last_close` via
//! [`seed_plan_state`], persists it, and dispatches nothing. The *next* tick
//! evaluates only genuinely-new candles.
//!
//! # Shadow (observe-only) plans
//!
//! A plan registered with [`shadow`](trade_control_core::trade_plan::TradePlan::shadow)
//! `= true` is evaluated and its [`PlanState`] advanced **identically** to a
//! live plan — same candles, same FSM, same watermark — but its fired intents
//! are **never dispatched**: each is logged as a `SHADOW would-fire` line
//! instead (see [`log_shadow_fire`]). This is the safe way to run the engine
//! beside the live TradingView alerts on demo (the Stage F gate): both observe
//! the same bars, but only the TV alert places real orders, so the engine's
//! decisions can be diffed against the alert without any double-firing.
//!
//! # Fail-soft per plan
//!
//! Like the sweep, a single plan's failure (broker fetch, KV write, dispatch)
//! is logged and skipped — one stale plan must never jam the whole tick.
//!
//! # Where the tests live
//!
//! The decision logic is pure and tested where it lives: the FSM
//! ([`evaluate_plan`]) and the seed-without-firing rule ([`seed_plan_state`])
//! have native table tests in the `trade-control-engine` crate; [`Shell::from_candle`]
//! is tested in `core`. This module's own functions are thin wasm-bound glue
//! (`worker::Env`, the concrete [`BrokerHandle`], and `worker::Response`, which
//! panics off-wasm at construction — see `every_rejection_outcome_classifies_as_skip`
//! in `lib.rs` for the same constraint), so only the pure helpers
//! (`seed_since`, `plan_state_expires_at`) are unit-tested here.
//! End-to-end behaviour is exercised on the demo deploy, run in parallel with
//! the live TV alerts (Stage F gate).

use chrono::{DateTime, Utc};
use trade_control_core::broker::{Broker, Candle, CandleError, Granularity, filter_new_candles};
use trade_control_core::incoming::Verified;
use trade_control_core::intent::{Action, Shell, VetoLevel};
use trade_control_core::plan_state::PlanState;
use trade_control_core::state::{StateStore, StoredPlan};
use trade_control_core::tick_bundle::{
    DispatchOutcome, KvTickTransition, TICK_BUNDLE_SCHEMA_VERSION, TickBundle,
};
use trade_control_engine::{PlanEval, evaluate_plan, seed_plan_state};
use worker::{Env, ScheduleContext};

use crate::ActionResult;
use crate::cron::sweep::{BrokerHandle, acquire_broker_for_account, open_store};
use crate::state::KvStateStore;
use crate::tick_recording::record_tick_to_r2;

/// How many bars of history to fetch when seeding a fresh plan. Enough to give
/// each `OnClose` rule a prior close to compare against on the next tick, with
/// slack for a cron gap; small so the seed fetch stays cheap.
const SEED_BARS: i64 = 10;

/// Walk every registered trade plan, evaluate it against fresh candles, and
/// dispatch fired intents. `now` is threaded in (not `Utc::now()`) so the tick
/// stays a pure function of `(env, now)`.
pub async fn run_engine_tick(env: &Env, ctx: &ScheduleContext, now: DateTime<Utc>) {
    let store = match open_store(env) {
        Some(s) => s,
        None => return,
    };

    let plans = match store.list_all_trade_plans().await {
        Ok(v) => v,
        Err(err) => {
            rlog_err!("cron engine: list_all_trade_plans: {err}");
            return;
        }
    };

    rlog!("cron engine: {} registered plans", plans.len());

    for stored in plans {
        if let Err(err) = tick_one(env, ctx, &store, &stored, now).await {
            rlog_err!(
                "cron engine[{}/{}]: {err}",
                stored.account.as_deref().unwrap_or("<global>"),
                stored.plan.trade_id,
            );
        }
    }
}

/// Evaluate and dispatch one plan. Returns an error string so the caller can
/// log it with plan context. Never panics; transient broker/KV failures bubble
/// up as a skip for this tick.
async fn tick_one(
    env: &Env,
    ctx: &ScheduleContext,
    store: &KvStateStore,
    stored: &StoredPlan,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let plan = &stored.plan;
    let account = stored.account.as_deref();
    let expires_at = plan_state_expires_at(now);

    // Load prior state, or seed-without-firing on the first tick.
    //
    // A `None` here is meant to be "this plan has never ticked" → seed. But a
    // re-seed is dangerous: it jumps the watermark to the newest candle and
    // fires nothing, so any price-cross veto in the skipped gap is lost forever
    // (bug #15). The plan-state row is now no-TTL, so it can't age out
    // mid-trade — the only remaining way a *live* plan reads `None` is a
    // transient KV eventual-consistency read-miss. Guard against that by
    // re-reading once before committing to a seed: a real first tick stays
    // `None`, a read-miss resolves to the persisted state and we proceed
    // normally instead of silently re-seeding past a cross.
    let prior = match read_plan_state_settled(store, account, &plan.trade_id).await? {
        Some(state) => state,
        None => {
            let broker = acquire_broker_for_account(env, account)
                .await
                .ok_or("broker acquisition failed")?;
            return seed_first_tick(store, &broker, plan, account, expires_at, now).await;
        }
    };

    let broker = acquire_broker_for_account(env, account)
        .await
        .ok_or("broker acquisition failed")?;

    // Fetch candles closed since the watermark. A plan with no watermark (an
    // earlier seed that found an empty window) re-seeds from the back-window.
    let since = match prior.watermark {
        Some(w) => w,
        None => return seed_first_tick(store, &broker, plan, account, expires_at, now).await,
    };
    let candles = fetch_candles(&broker, &plan.instrument, plan.granularity, since, now).await?;
    let fresh = filter_new_candles(candles, since);
    if fresh.is_empty() {
        return Ok(());
    }

    // The H&S `PinePattern` entry is stateful and needs lookback the
    // watermark-bounded `fresh` slice doesn't carry. When the plan has one,
    // fetch a wider back-window ending at `now` for the detector; otherwise the
    // window is unused and `fresh` stands in.
    let detector_window = detector_window_for(&broker, plan, &fresh, now).await?;

    let eval = evaluate_plan(plan, &prior, &fresh, &detector_window, now, expires_at);

    // Surface any trendline out-of-window anchor diagnostics the pure evaluator
    // can't log itself. An out-of-window anchor falls back to the `bar_seconds`
    // divisor (wall-clock spacing across any gap in the un-fetched span) or, on
    // a pre-`bar_seconds` plan, silently can't fire — both rare, neither should
    // be invisible. Logged here (not in the engine crate, which has no rlog!).
    for w in &eval.warnings {
        rlog!("cron engine: plan {} {}", plan.trade_id, w);
    }

    // Persist the advanced state (or clear it + the plan when the spine is
    // done) before dispatching, so a dispatch failure can't replay a fired bar.
    // Capture the transition (before/after/success/error) for the tick-bundle.
    let kv = persist_plan_state(store, plan, account, &eval, now, &prior).await;
    // A hard put failure is still a skip for this tick — but the bundle records
    // the failed transition first, so a replay can see "wanted to advance,
    // couldn't" rather than a silent gap.
    let put_failed = !kv.success && !eval.done;

    // Shadow plans observe only: the state above advanced exactly as a live
    // plan would, but we never touch the broker or the seen-id index — each
    // would-be fire is logged so it can be diffed against the live TV alert.
    // Shadow ticks touch no broker, so they're the safest to record first.
    if plan.shadow {
        for fired in &eval.fired {
            log_shadow_fire(&plan.trade_id, fired);
        }
        // A shadow no-op is as information-free as a live one (it still never
        // touches the broker), so it's trimmed identically — the diff against
        // the live TV alert only cares about ticks where something fired or the
        // FSM moved, and those are exactly the noteworthy ones.
        if !eval.is_noteworthy(&prior) {
            log_noop_tick(plan, &eval, now);
            return Ok(());
        }
        let bundle = build_tick_bundle(
            stored,
            &prior,
            &fresh,
            &detector_window,
            eval,
            now,
            expires_at,
            Vec::new(),
            kv,
        );
        record_tick_to_r2(env, ctx, bundle);
        return Ok(());
    }

    if put_failed {
        // Record the failed transition before bailing, so a replay sees the
        // "wanted to advance, couldn't" rather than a silent gap. No dispatch
        // happened, so `dispatch_outcomes` is empty.
        let bundle = build_tick_bundle(
            stored,
            &prior,
            &fresh,
            &detector_window,
            eval,
            now,
            expires_at,
            Vec::new(),
            kv.clone(),
        );
        record_tick_to_r2(env, ctx, bundle);
        return Err(format!("put_plan_state: {}", kv.error.unwrap_or_default()));
    }

    let mut dispatch_outcomes = Vec::with_capacity(eval.fired.len());
    for (seq, fired) in eval.fired.iter().enumerate() {
        let outcome = dispatch_fired(env, store, &broker, fired, plan.granularity, now).await;
        dispatch_outcomes.push(DispatchOutcome {
            rule_id: fired.rule_id.clone(),
            intent_id: fired.intent.id.clone(),
            outcome,
            seq: seq as u32,
        });
    }

    // Trim no-op ticks: a new bar arrived but nothing fired, no phase/state
    // advance, plan not done, KV write succeeded. The fat bundle (whole plan +
    // both states + wide detector window) carries no information for such a
    // tick, so emit a lightweight heartbeat instead and skip the R2 write. A
    // noteworthy tick (something fired / finished / advanced) still records in
    // full. (A no-op has an empty `fired`, so `dispatch_outcomes` is empty here
    // too — nothing was dispatched.)
    if !eval.is_noteworthy(&prior) {
        log_noop_tick(plan, &eval, now);
        return Ok(());
    }

    let bundle = build_tick_bundle(
        stored,
        &prior,
        &fresh,
        &detector_window,
        eval,
        now,
        expires_at,
        dispatch_outcomes,
        kv,
    );
    record_tick_to_r2(env, ctx, bundle);
    Ok(())
}

/// Persist the tick's advanced state and report the KV transition for the
/// tick-bundle. On `done` the plan-state row *and* the plan row are cleared
/// (the spine is finished); otherwise the new state is written with a fresh TTL.
/// `success`/`error` capture whether the authoritative write landed — a failed
/// clear is logged but doesn't fail the tick (the TTL still ages the row out),
/// while a failed `put` is surfaced so the caller can skip dispatch.
async fn persist_plan_state(
    store: &KvStateStore,
    plan: &trade_control_core::trade_plan::TradePlan,
    account: Option<&str>,
    eval: &PlanEval,
    now: DateTime<Utc>,
    prior: &PlanState,
) -> KvTickTransition {
    let key = plan_state_key(account, &plan.trade_id);
    if eval.done {
        // Snapshot the finished plan + its terminal state to the archive
        // keyspace BEFORE dropping the live rows, so `plan list --include-all`
        // can still surface a vetoed/completed setup for analysis. A failed
        // archive is logged but must not fail the tick — the clears below still
        // proceed (the engine has finished with this plan regardless).
        if let Err(err) = store
            .archive_plan(account, plan, &eval.new_state, now)
            .await
        {
            rlog_err!("cron engine: archive_plan({}): {err}", plan.trade_id);
        }
        if let Err(err) = store.clear_plan_state(account, &plan.trade_id).await {
            rlog_err!("cron engine: clear_plan_state({}): {err}", plan.trade_id);
        }
        if let Err(err) = store.clear_trade_plan(account, &plan.trade_id).await {
            rlog_err!("cron engine: clear_trade_plan({}): {err}", plan.trade_id);
        }
        return KvTickTransition {
            key,
            before: Some(prior.clone()),
            after: None,
            cleared_plan: true,
            success: true,
            error: None,
        };
    }
    match store
        .put_plan_state(account, &plan.trade_id, &eval.new_state)
        .await
    {
        Ok(()) => KvTickTransition {
            key,
            before: Some(prior.clone()),
            after: Some(eval.new_state.clone()),
            cleared_plan: false,
            success: true,
            error: None,
        },
        Err(err) => KvTickTransition {
            key,
            before: Some(prior.clone()),
            after: Some(eval.new_state.clone()),
            cleared_plan: false,
            success: false,
            error: Some(err.to_string()),
        },
    }
}

/// Assemble the [`TickBundle`] for this tick. `dispatch_outcomes` is empty for a
/// shadow tick (which dispatches nothing); live ticks populate it.
#[allow(clippy::too_many_arguments)]
fn build_tick_bundle(
    stored: &StoredPlan,
    prior: &PlanState,
    fresh: &[Candle],
    detector_window: &[Candle],
    eval: PlanEval,
    now: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    dispatch_outcomes: Vec<DispatchOutcome>,
    kv: KvTickTransition,
) -> TickBundle {
    let plan = &stored.plan;
    TickBundle {
        schema_version: TICK_BUNDLE_SCHEMA_VERSION,
        tick_ts: now,
        correlation_id: plan.trade_id.clone(),
        account: stored.account.clone(),
        request_id: format!("{}@{}", plan.trade_id, now.to_rfc3339()),
        plan: plan.clone(),
        prior_state: prior.clone(),
        new_candles: fresh.to_vec(),
        detector_window: detector_window.to_vec(),
        now,
        expires_at,
        shadow: plan.shadow,
        eval,
        dispatch_outcomes,
        kv,
    }
}

/// Reconstruct the plan-state KV key for the bundle's `KvTickTransition`. Mirrors
/// `KvStateStore::plan_state_key` (which is private) using the public
/// [`account_scope`](trade_control_core::state::account_scope); a recording label
/// only, so the small duplication is fine.
fn plan_state_key(account: Option<&str>, trade_id: &str) -> String {
    let scope = trade_control_core::state::account_scope(account);
    format!("plan-state:{scope}:{trade_id}")
}

/// Log a fired intent that a shadow plan suppressed. Mirrors the structured
/// fields of [`dispatch_fired`]'s `cron engine fired` line (minus the broker
/// outcome, which never happened) so a log scrape can diff `SHADOW would-fire`
/// against the live TV alert's actual placement on the same candle.
fn log_shadow_fire(trade_id: &str, fired: &trade_control_engine::FiredIntent) {
    rlog!(
        "cron engine SHADOW would-fire: trade_id={} rule={} action={:?} id={} candle_time={} (observe-only, no broker)",
        trade_id,
        fired.rule_id,
        fired.intent.action,
        fired.intent.id,
        fired.candle.time,
    );
}

/// Emit the lightweight heartbeat for a trimmed no-op tick.
///
/// A no-op tick saw a new closed bar but nothing fired and the FSM didn't
/// advance, so its fat [`TickBundle`] is skipped (see [`PlanEval::is_noteworthy`]).
/// This single line keeps the tick traceable in Cloudflare logs, so a silent
/// gap in the `ticks/` R2 stream is never mistaken for "the cron stopped
/// running". `new_state.watermark` is the new bar's open-time the tick processed.
fn log_noop_tick(
    plan: &trade_control_core::trade_plan::TradePlan,
    eval: &PlanEval,
    now: DateTime<Utc>,
) {
    rlog!(
        "cron engine: plan {} tick {} no-op (new bar {:?}, nothing fired/advanced, phase {:?}) — not recorded",
        plan.trade_id,
        now,
        eval.new_state.watermark,
        eval.new_state.phase,
    );
}

/// Read a plan's persisted state, tolerating a transient KV read-miss before
/// concluding the plan has never ticked.
///
/// The state row is no-TTL, so a live plan's row never ages out — but
/// Cloudflare KV is eventually consistent, and a read immediately after a write
/// on another edge can momentarily return `None`. Treating that `None` as
/// "first tick" would re-seed and skip the cross (bug #15). So on a `None`, we
/// re-read once: a genuine first tick stays `None`; a read-miss resolves to the
/// real state on the retry. The cost is one extra KV GET on the rare miss.
async fn read_plan_state_settled(
    store: &KvStateStore,
    account: Option<&str>,
    trade_id: &str,
) -> Result<Option<PlanState>, String> {
    let first = store
        .get_plan_state(account, trade_id)
        .await
        .map_err(|err| format!("get_plan_state: {err}"))?;
    if first.is_some() {
        return Ok(first);
    }
    // `None` — re-read once to rule out an eventual-consistency miss before
    // committing to a (watermark-jumping) seed.
    let settled = store
        .get_plan_state(account, trade_id)
        .await
        .map_err(|err| format!("get_plan_state (re-read): {err}"))?;
    if settled.is_some() {
        rlog!(
            "cron engine: plan-state for {trade_id} missed on first read, resolved on re-read \
             (KV eventual consistency) — proceeding without a re-seed"
        );
    }
    Ok(settled)
}

/// First-tick seed: fetch a back-window, seed the state without firing, persist.
async fn seed_first_tick(
    store: &KvStateStore,
    broker: &BrokerHandle,
    plan: &trade_control_core::trade_plan::TradePlan,
    account: Option<&str>,
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let since = seed_since(plan.granularity, now);
    let candles = fetch_candles(broker, &plan.instrument, plan.granularity, since, now).await?;
    let state = seed_plan_state(plan, &candles, expires_at);
    store
        .put_plan_state(account, &plan.trade_id, &state)
        .await
        .map_err(|err| format!("put_plan_state (seed): {err}"))?;
    rlog!(
        "cron engine seed: trade_id={} instrument={} watermark={:?} ({} back-window candles)",
        plan.trade_id,
        plan.instrument,
        state.watermark,
        candles.len(),
    );
    Ok(())
}

/// Fetch candles through whichever broker the plan's account uses.
async fn fetch_candles(
    broker: &BrokerHandle,
    instrument: &str,
    granularity: Granularity,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Vec<Candle>, String> {
    let result = match broker {
        BrokerHandle::Oanda(b) => b.get_candles(instrument, granularity, since, now).await,
        BrokerHandle::TradeNation(b) => b.get_candles(instrument, granularity, since, now).await,
    };
    match result {
        Ok(c) => Ok(c),
        // A degenerate window is a no-op, not a failure — the next tick widens.
        Err(CandleError::BadRange) => Ok(Vec::new()),
        Err(CandleError::Transient) => Err("candle fetch failed (transient)".into()),
    }
}

/// Build the detector back-window for a plan.
///
/// The window must reach back far enough to cover **both** consumers that need
/// history beyond the watermark-bounded `fresh` slice:
///
/// 1. A `PinePattern` entry (H&S) — the detector needs `min_lookback_bars` of
///    history behind the earliest fresh candle to recompute the latch (pattern
///    depth + confirm window + SL lookback).
/// 2. A `TrendlineCross` (break-and-close / retest necklines) — the engine
///    resolves the line's level in **bar-index** space by counting bars between
///    its two anchors. If an anchor predates the window, that count falls back
///    to the `bar_seconds` wall-clock divisor (which mis-counts across gaps) or,
///    on a pre-`bar_seconds` plan, can't resolve at all. Fetching back to the
///    **earliest anchor** keeps every anchor in-window, so the fallback never
///    fires for a normally-armed plan.
///
/// We take the earliest `since` either consumer asks for and fetch once. A plan
/// with neither (a pure M/W heartbeat) keeps the `fresh`-only fast path — no
/// extra fetch, every new candle already present (the only contract the
/// evaluator needs).
async fn detector_window_for(
    broker: &BrokerHandle,
    plan: &trade_control_core::trade_plan::TradePlan,
    fresh: &[Candle],
    now: DateTime<Utc>,
) -> Result<Vec<Candle>, String> {
    let earliest_fresh = fresh.iter().map(|c| c.time).min().unwrap_or(now);
    let pine_since = pine_lookback_since(plan, earliest_fresh);
    let anchor_since = trendline_anchor_since(plan);

    // The window start is the earliest any consumer needs. `None` from both ⇒
    // no history fetch required; `fresh` already satisfies the evaluator.
    let since = [pine_since, anchor_since].into_iter().flatten().min();
    let Some(since) = since else {
        return Ok(fresh.to_vec());
    };

    let window = fetch_candles(broker, &plan.instrument, plan.granularity, since, now).await?;

    // Merge fetched history with `fresh` and dedup by open-time so every new
    // candle is present even if the history fetch's closed-bar cutoff dropped the
    // most recent one. Ascending by time.
    let mut merged = window;
    for c in fresh {
        if !merged.iter().any(|w| w.time == c.time) {
            merged.push(*c);
        }
    }
    merged.sort_by_key(|c| c.time);
    Ok(merged)
}

/// The fetch start a `PinePattern` entry needs: `min_lookback_bars` of history
/// behind the earliest fresh candle. `None` when the plan has no Pine entry.
fn pine_lookback_since(
    plan: &trade_control_core::trade_plan::TradePlan,
    earliest_fresh: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    use trade_control_core::signals::{DetectorConfig, min_lookback_bars};
    use trade_control_core::trade_plan::Trigger;

    let has_pine = plan.rules.iter().any(|r| {
        r.intent.action == Action::Enter && matches!(r.trigger, Trigger::PinePattern { .. })
    });
    if !has_pine {
        return None;
    }
    let cfg = DetectorConfig::pine_defaults(plan.granularity);
    let lookback = min_lookback_bars(&cfg) as i64;
    Some(earliest_fresh - chrono::Duration::seconds(plan.granularity.seconds() * lookback))
}

/// The fetch start the plan's trendlines need: the earliest anchor epoch across
/// every `TrendlineCross` rule, minus one bar of slack so the anchor's own bar
/// is comfortably inside the fetched window (the exact-match path in
/// `bar_index_at` wants the anchor bar present). `None` when the plan has no
/// trendline rule.
fn trendline_anchor_since(
    plan: &trade_control_core::trade_plan::TradePlan,
) -> Option<DateTime<Utc>> {
    let earliest_anchor = earliest_trendline_anchor_epoch(plan.rules.iter().map(|r| &r.trigger))?;
    // One bar of slack before the anchor.
    let slack = plan.granularity.seconds();
    DateTime::from_timestamp(earliest_anchor - slack, 0)
}

/// The earliest anchor epoch across every `TrendlineCross` trigger in the
/// sequence, or `None` if there are no trendline triggers. Pulled out as a free
/// function over triggers so it can be unit-tested without building `Intent`s.
fn earliest_trendline_anchor_epoch<'a>(
    triggers: impl Iterator<Item = &'a trade_control_core::trade_plan::Trigger>,
) -> Option<i64> {
    use trade_control_core::trade_plan::Trigger;
    triggers
        .filter_map(|t| match t {
            Trigger::TrendlineCross { a, b, .. } => Some(a.at_epoch.min(b.at_epoch)),
            _ => None,
        })
        .min()
}

/// Dispatch one fired intent through the same handlers the webhook uses, then
/// record the outcome on the seen-by-id index exactly as the HTTP path does.
/// Returns the dispatch outcome string (`ActionResult::describe()`) so the tick
/// can fold it into the recorded [`TickBundle`].
///
/// The plan was verified at register, so the [`Verified`] is synthesised
/// directly: the [`Shell`] from the triggering candle (OHLC + time; `open`
/// populated so M/W body logic works), the intent cloned verbatim.
async fn dispatch_fired(
    env: &Env,
    store: &KvStateStore,
    broker: &BrokerHandle,
    fired: &trade_control_engine::FiredIntent,
    granularity: Granularity,
    now: DateTime<Utc>,
) -> String {
    // An H&S `PinePattern` fire carries the latched signal geometry; fold it onto
    // the shell so the enter resolves entry/SL/TP against the *pattern* extremes
    // (signal_high/low, recent_*, golden, signal_confirmed) exactly as the TV
    // alert's `{{plot(...)}}` substitutions did. Every other fire (M/W, vetos,
    // preps) carries no signal and gets the plain candle shell.
    let shell = match &fired.signal {
        Some(sig) => Shell::from_candle_and_signal(&fired.candle, sig),
        None => Shell::from_candle(&fired.candle),
    };
    let verified = Verified {
        shell,
        intent: fired.intent.clone(),
    };
    let result = match broker {
        BrokerHandle::Oanda(b) => dispatch_action(b, store, &verified, env, granularity, now).await,
        BrokerHandle::TradeNation(b) => {
            dispatch_action(b, store, &verified, env, granularity, now).await
        }
    };
    let outcome = result.describe();
    rlog!(
        "cron engine fired: rule={} action={:?} id={} outcome={outcome}",
        fired.rule_id,
        verified.intent.action,
        verified.intent.id,
    );
    crate::record_dispatcher_outcome(store, &verified, now, &result).await;
    outcome
}

/// Engine-side action dispatch. Mirrors the webhook's `run_action` but for the
/// actions an engine-fired intent can carry, and with `raw_body: None` (there
/// is no signed wire body — the plan rode one signed envelope at register, and
/// each fired intent is reconstructed, not re-received). Control actions (prep,
/// stop-next-entry veto) are wrapped into an [`ActionResult`] so every fired
/// intent records uniformly.
async fn dispatch_action<B: Broker>(
    broker: &B,
    store: &KvStateStore,
    verified: &Verified,
    env: &Env,
    granularity: Granularity,
    now: DateTime<Utc>,
) -> ActionResult {
    match verified.intent.action {
        Action::Enter => {
            // Resolve the dispatch config at this edge (mirrors the webhook
            // fetch path) so `run_enter` is `Env`-free.
            let cfg = crate::build_dispatch_config(env, verified).await;
            crate::run_enter(broker, store, verified, &cfg, now, None, Some(granularity)).await
        }
        Action::Close => crate::run_close(broker, store, verified, now).await,
        Action::Invalidate => crate::run_invalidate(broker, store, verified, now).await,
        Action::Veto => {
            // A stop-next-entry veto sets KV only (no broker); higher levels
            // cancel/close via the broker. Matches the webhook's split.
            if matches!(
                verified.intent.level.unwrap_or_default(),
                VetoLevel::StopNextEntry
            ) {
                control_result(crate::handle_veto(store, verified, now).await, "vetoed")
            } else {
                crate::run_veto_with_broker(broker, store, verified, now).await
            }
        }
        Action::Prep => control_result(crate::handle_prep(store, verified, now).await, "prepped"),
        // Calendar/news control rules: a plan folds in pause/resume (blackout
        // window) and news-start/news-end (news window) as TimeReached rules.
        // They set KV state only (no broker), exactly as the matching TV alert
        // message would have. Route to the same handlers the webhook uses.
        Action::Pause => {
            control_result(crate::handle_pause(store, verified, now).await, "pause-set")
        }
        Action::Resume => control_result(
            crate::handle_resume(store, verified, now).await,
            "pause-cleared",
        ),
        Action::NewsStart => control_result(
            crate::handle_news_start(store, verified, now).await,
            "news-window-open",
        ),
        Action::NewsEnd => control_result(
            crate::handle_news_end(store, verified, now).await,
            "news-window-closed",
        ),
        other => {
            // Status / Unlock / Clear* / Register / PrepExpire / MarketInfo /
            // Plan* are operator/control actions a plan never embeds as a fired
            // rule. Treat as a no-op rejection so it's visible but inert.
            ActionResult::Rejected {
                status: 400,
                body: "engine: unsupported fired action".to_string(),
                outcome: format!("rejected: unsupported-action {other:?}"),
            }
        }
    }
}

/// Fold a control-action handler's [`ControlResult`] into an [`ActionResult`]
/// so engine dispatch records uniformly. A `2xx` is `Ok`; anything else is
/// `Rejected` — control handlers never reach the broker, so `Failed` (a broker
/// error) is not a possible outcome here.
fn control_result(
    res: trade_control_core::dispatch::ControlResult,
    ok_outcome: &str,
) -> ActionResult {
    if res.is_success() {
        ActionResult::Ok(ok_outcome.to_string())
    } else {
        let code = res.status;
        ActionResult::Rejected {
            status: code,
            body: format!("control dispatch returned status {code}"),
            outcome: format!("rejected: control-status-{code}"),
        }
    }
}

/// The `expires_at` stamp on a plan's state row. Plans don't carry their own
/// expiry — the carrier enter intent's `not_after` set the *plan's* register
/// TTL, which is the real GC. The state row just needs to outlive a few ticks,
/// so a generous day past `now` is enough; whichever of the plan row or its
/// state row ages out first, the engine stops ticking it.
fn plan_state_expires_at(now: DateTime<Utc>) -> DateTime<Utc> {
    now + chrono::Duration::days(1)
}

/// The seed back-window start: [`SEED_BARS`] bars of the plan's granularity
/// before `now`. Pure so the window math is unit-testable without a broker.
fn seed_since(granularity: Granularity, now: DateTime<Utc>) -> DateTime<Utc> {
    now - chrono::Duration::seconds(granularity.seconds() * SEED_BARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn seed_since_walks_back_n_bars_of_granularity() {
        let now = ts("2026-06-17T20:00:00Z");
        // 10 H1 bars back = 10 hours.
        assert_eq!(seed_since(Granularity::H1, now), ts("2026-06-17T10:00:00Z"));
        // 10 M15 bars back = 150 minutes.
        assert_eq!(
            seed_since(Granularity::M15, now),
            ts("2026-06-17T17:30:00Z")
        );
    }

    #[test]
    fn plan_state_expires_at_is_a_day_out() {
        let now = ts("2026-06-17T20:00:00Z");
        assert_eq!(plan_state_expires_at(now), ts("2026-06-18T20:00:00Z"));
    }

    // ===== detector_window widening for trendline anchors =====

    use trade_control_core::trade_plan::{BarEvent, CrossDir, LinePoint, Trigger};

    fn trendline(a_epoch: i64, b_epoch: i64) -> Trigger {
        Trigger::TrendlineCross {
            a: LinePoint {
                at_epoch: a_epoch,
                price: 1.0,
            },
            b: LinePoint {
                at_epoch: b_epoch,
                price: 1.0,
            },
            extend_forward: true,
            bar_seconds: 3600,
            dir: CrossDir::Down,
            bar: BarEvent::OnClose,
        }
    }

    #[test]
    fn earliest_anchor_is_min_across_all_trendline_rules() {
        // Two trendlines; the break-and-close anchor `a` (epoch 100) is the
        // earliest of all four anchors → that's what the window must reach.
        let triggers = [
            trendline(100, 500),
            trendline(300, 900),
            Trigger::MwEveryBar, // ignored
        ];
        assert_eq!(earliest_trendline_anchor_epoch(triggers.iter()), Some(100));
    }

    #[test]
    fn earliest_anchor_is_none_without_a_trendline() {
        // Pure M/W (+ a time veto) has no trendline → no anchor-driven fetch.
        let triggers = [Trigger::MwEveryBar, Trigger::TimeReached { at_epoch: 999 }];
        assert_eq!(earliest_trendline_anchor_epoch(triggers.iter()), None);
    }

    #[test]
    fn earliest_anchor_handles_reversed_endpoints() {
        // `b` earlier than `a` (anchors aren't required to be time-ordered) — the
        // min of the pair is still picked.
        let triggers = [trendline(800, 200)];
        assert_eq!(earliest_trendline_anchor_epoch(triggers.iter()), Some(200));
    }
}
