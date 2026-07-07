//! The `Enter` dispatch path: gates → sizing → broker placement → recovery.

use super::action_result::ActionResult;
use super::shared::record_control_event_for;
use crate::allow_entry_gate;
use crate::broker::{Broker, EntryError, EntryRequest};
use crate::dispatch_config::DispatchConfig;
use crate::incoming;
use crate::intent::{
    MW_CANCEL_VETO_NAME, MwAnchors, MwUpdate, ResolveError, Resolved, effective_mw_params,
    is_inside_any, plan_mw_update,
};
use crate::recover_entry;
use crate::spread_blackout;
use crate::state::{StateStore, veto_ttl_seconds};
use crate::sweep_gate::now_utc_minute_of_day;

/// Render a raw price for an operator-facing message: fixed generous precision
/// (enough for 5dp FX and finer) with trailing zeros trimmed, so an index level
/// like `209.99` doesn't print as `209.9930432131929` (float dust from the
/// spread-mean arithmetic) nor as `209.99000`. Deliberately **pip-independent**
/// — the SL-spread floor is a pure price-distance ratio and its messages must
/// not depend on an instrument's catalog pip (a wrong pip would make a correct
/// decision *read* wrong; see the SL-floor spec).
fn fmt_price_trim(v: f64) -> String {
    if !v.is_finite() {
        return format!("{v}");
    }
    // 6 dp rounds off the float dust while keeping sub-tick precision for every
    // instrument class; trim trailing zeros (and a bare trailing dot).
    let s = format!("{v:.6}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    trimmed.to_string()
}

/// Run an `enter` intent end-to-end (gates → sizing → broker placement →
/// `recover_entry` fallback).
///
/// `raw_body` is the **exact signed YAML bytes** this intent arrived as, when
/// known. On a successful real placement we persist it under an
/// `order:{broker_order_id}` KV row so the spread-blackout apply cron can
/// recover it (it finds a broker *pending order*, not a signed intent) and
/// re-drive this same entry on recovery. `None` is passed only where no signed
/// body is available (there is none today — both the HTTP path and the
/// blackout re-drive supply it); a `None` simply skips the order-body write, so
/// such an order can't be blackout-cancelled-and-restored.
pub async fn run_enter<B: Broker, S: StateStore>(
    broker: &B,
    store: &S,
    verified: &incoming::Verified,
    cfg: &DispatchConfig,
    now: chrono::DateTime<chrono::Utc>,
    raw_body: Option<&str>,
    // The trade's timeframe, when this enter was dispatched from a registered
    // plan (the engine path passes `Some(plan.granularity)`). The break-even
    // position cron needs it to fetch the right closed candles. The webhook and
    // blackout-restore re-drive paths have no plan timeframe in hand and pass
    // `None` — those enters simply don't get cron-managed break-even (the
    // signed enter still carries its `breakeven` rule; only the cron snapshot
    // is skipped without a granularity to fetch on).
    enter_granularity: Option<crate::broker::Granularity>,
) -> ActionResult {
    // Blackout gate — if any pause for this trade_id is active, reject
    // before doing any other work. Pauses are intentionally cheap to
    // check (one prefix list on the trade's own keys) so they can sit
    // ahead of the retry/cooldown/prep/veto chain. Trades minted
    // without a `trade_id` (legacy single-shot entries) bypass this
    // gate entirely — there's no key to look pauses up by.
    if let Some(tid) = verified.intent.trade_id.as_deref() {
        match store.list_pauses_for_trade(tid).await {
            Ok(pauses) if !pauses.is_empty() => {
                let blackouts: Vec<String> = pauses
                    .iter()
                    .map(|p| match &p.reason {
                        Some(r) => format!("{}({r})", p.blackout_id),
                        None => p.blackout_id.clone(),
                    })
                    .collect();
                tracing::info!(
                    "entry rejected: trade {tid} paused (active blackouts: {})",
                    blackouts.join(", ")
                );
                return ActionResult::Rejected {
                    status: 423,
                    body: "trade paused".to_string(),
                    outcome: format!("rejected: paused [{}]", blackouts.join(",")),
                };
            }
            Ok(_) => {}
            Err(err) => {
                tracing::error!("KV list_pauses_for_trade: {err}");
                return ActionResult::Rejected {
                    status: 500,
                    body: "state error".to_string(),
                    outcome: "rejected: state-error".into(),
                };
            }
        }
    }

    // Retry gate — when the intent opts into multi-shot mode via
    // `max_retries`, the gate inspects prior attempts (cancel-and-
    // replace a still-pending one, reject a fresh placement when an
    // earlier attempt is still open, allow another placement when
    // earlier attempts have closed) and enforces the placement cap.
    // "Retry" here means re-entry into a setup after a prior fill
    // closed (typically at SL), *not* a re-attempt of a failed
    // placement — broker failures are terminal and 502 out. See
    // `core::retry_gate` for the full semantics. The single-shot
    // path (`max_retries: Static(0)`, the default) skips this branch
    // entirely so no new KV/broker calls land on the byte-identical
    // baseline.
    let retry_attempt_no = if !matches!(
        verified.intent.max_retries,
        crate::tunable::Tunable::Static(0)
    ) {
        match crate::retry_gate::evaluate(broker, store, &verified.intent, &verified.shell).await {
            crate::retry_gate::RetryGateOutcome::Proceed { next_attempt_no } => {
                Some(next_attempt_no)
            }
            crate::retry_gate::RetryGateOutcome::Rejected {
                status,
                message,
                outcome,
            } => {
                return ActionResult::Rejected {
                    status,
                    body: message.to_string(),
                    outcome,
                };
            }
        }
    } else {
        None
    };

    // Cooldown gate — scoped to this intent's account so a cooldown on
    // a different account doesn't pause this one. A global cooldown
    // (set without `account:`) still pauses every account.
    match store
        .is_cooled_down(
            verified.intent.account.as_deref(),
            &verified.intent.instrument,
        )
        .await
    {
        Ok(true) => {
            tracing::info!(
                "entry rejected: {} cooled down (id={})",
                verified.intent.instrument,
                verified.intent.id
            );
            return ActionResult::Rejected {
                status: 423,
                body: "instrument cooled down".to_string(),
                outcome: "rejected: cooled-down".into(),
            };
        }
        Ok(false) => {}
        Err(err) => {
            tracing::error!("KV is_cooled_down: {err}");
            return ActionResult::Rejected {
                status: 500,
                body: "state error".to_string(),
                outcome: "rejected: state-error".into(),
            };
        }
    }

    // Prep gate — every name in `requires_preps` must be currently set,
    // and the stored `set_at` timestamps must be strictly increasing in
    // list order.
    let mut prev_ts: Option<chrono::DateTime<chrono::Utc>> = None;
    for step in &verified.intent.requires_preps {
        match store
            .get_prep(
                verified.intent.account.as_deref(),
                &verified.intent.instrument,
                step,
            )
            .await
        {
            Ok(Some(set_at)) => {
                if let Some(prev) = prev_ts
                    && set_at <= prev
                {
                    tracing::info!(
                        "entry rejected: prep {} not after previous (id={})",
                        step,
                        verified.intent.id
                    );
                    return ActionResult::Rejected {
                        status: 412,
                        body: "prep order violated".to_string(),
                        outcome: format!("rejected: prep-order-violated ({step})"),
                    };
                }
                prev_ts = Some(set_at);
            }
            Ok(None) => {
                tracing::info!(
                    "entry rejected: missing prep {} (id={})",
                    step,
                    verified.intent.id
                );
                return ActionResult::Rejected {
                    status: 412,
                    body: "missing prep".to_string(),
                    outcome: format!("rejected: missing-prep ({step})"),
                };
            }
            Err(err) => {
                tracing::error!("KV get_prep: {err}");
                return ActionResult::Rejected {
                    status: 500,
                    body: "state error".to_string(),
                    outcome: "rejected: state-error".into(),
                };
            }
        }
    }

    // Veto gate — entry is rejected if any opted-in veto is active.
    // Scope the check to the entry's `account` so a veto on a different
    // account doesn't block this trade; a global veto (set with no
    // `account:`) still blocks every account by design. The veto lookup
    // is also scoped to this entry's `trade_id` so a veto from a
    // different setup on the same instrument can't block it
    // (2026-06-11 fix). `Intent::validate` guarantees `trade_id` is
    // present on `enter`; the guard here is defence-in-depth.
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        tracing::error!(
            "enter missing trade_id at veto gate (id={})",
            verified.intent.id
        );
        return ActionResult::Rejected {
            status: 400,
            body: "enter requires trade_id".to_string(),
            outcome: "rejected: missing-trade-id".into(),
        };
    };
    for veto in &verified.intent.vetos {
        match store
            .is_vetoed(
                verified.intent.account.as_deref(),
                trade_id,
                &verified.intent.instrument,
                veto,
            )
            .await
        {
            Ok(true) => {
                tracing::info!(
                    "entry rejected: veto {} active (id={})",
                    veto,
                    verified.intent.id
                );
                return ActionResult::Rejected {
                    status: 412,
                    body: "veto active".to_string(),
                    outcome: format!("rejected: veto-active ({veto})"),
                };
            }
            Ok(false) => {}
            Err(err) => {
                tracing::error!("KV is_vetoed: {err}");
                return ActionResult::Rejected {
                    status: 500,
                    body: "state error".to_string(),
                    outcome: "rejected: state-error".into(),
                };
            }
        }
    }

    let worker_max_risk_pct = cfg.worker_max_risk_pct;
    let worker_max_open_positions = cfg.worker_max_open_positions;
    // Pip size precedence: the value baked into the signed intent at arm
    // time (the authority — `tv-arm` reads it from `instrument-lookup`) wins;
    // a missing field falls back to `cfg.pip_size`, which the edge resolved
    // from the per-instrument `PIP_SIZE_<instrument>` secret then the forex
    // default. The fallback keeps any pre-baked in-flight intent resolving
    // during rollout. See `DispatchConfig` / `pip_size_for`.
    let pip_size = verified.intent.pip_size.unwrap_or(cfg.pip_size);

    // M/W real-time geometry. For M/W enters carrying a `trade_id`, evolve
    // the live neckline / right-shoulder per bar (Phase B): a deeper body
    // still inside the 60% validity floor revises the neckline; a higher
    // body records the right shoulder (→ SL anchor); a body past the floor
    // cancels the setup (cancel pending + `mw-cancel` veto, never closes an
    // open position). All comparisons are body-based, so a rogue wick can't
    // move geometry or cancel. A bar with no `open` (pre-v2.5 chart) leaves
    // the state untouched and resolves against baked params. Returns the
    // effective `MwParams` to resolve this bar against, or short-circuits.
    let mw_effective = match maybe_update_mw_state(broker, store, verified, now).await {
        MwStateOutcome::Proceed(mw) => Some(mw),
        MwStateOutcome::NotMw => None,
        MwStateOutcome::Cancelled(result) => return result,
    };

    let resolve_result = match &mw_effective {
        // M/W with live geometry: resolve against the effective params.
        Some(mw) => Resolved::from_mw_intent(&verified.intent, &verified.shell, mw),
        // Everything else (and M/W with no trade_id / no `open`): the
        // standard dispatch, which itself routes baked M/W to from_mw_intent.
        None => Resolved::from_intent(&verified.intent, &verified.shell, pip_size),
    };
    // `mut` so the SL-spread-floor salvage below can widen `stop_loss` in
    // place before the `EntryRequest` is built.
    let mut resolved = match resolve_result {
        Ok(r) => r,
        // An M/W bar that hasn't completed its real-time arming sequence is
        // a *benign, expected* decline ("stay armed for the next bar"), not a
        // bad request. Report it as a 200 with a distinct `declined:` outcome
        // so the timeline/verdict downstream can tell routine M/W declines
        // apart from a genuinely malformed enter. It is still a seen-id
        // `Skip` (Rejected), so the setup stays armed. See bug #7.
        Err(ResolveError::NotArmedYet) => {
            tracing::info!(
                "resolve: M/W not armed yet — declining this bar (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                status: 200,
                body: "declined: mw-not-armed".to_string(),
                outcome: "declined: mw-not-armed".into(),
            };
        }
        // Genuinely malformed enter (wrong-side SL/limit/stop, entry outside
        // SL..TP, sub-1R, missing field, bad script): a real 400 bad request.
        Err(err) => {
            tracing::error!("resolve: {err}");
            return ActionResult::Rejected {
                status: 400,
                body: "rejected".to_string(),
                outcome: "rejected: resolve-failed".into(),
            };
        }
    };

    // Entry-level veto gate — Bug #12. The pcl-exhausted / invalidation level
    // is a *continuous* predicate: reject when the resolved entry price is
    // already past it, regardless of whether the engine's cross-event guard
    // fired or wrote a KV veto. The legacy persistent KV veto gave this
    // continuous semantics for free; the engine's one-shot Intrabar guard can
    // miss a gap / pre-armed breach and let the entry through (the NZD/CAD
    // −110.53 GBP incident). Sits after `resolved` (needs the entry price) and
    // before `allow_entry` (a regression-critical veto must not be defeatable
    // by an operator script). The `rejected: veto-active (<name>)` outcome is
    // byte-identical to the legacy KV veto path and is a seen-id `Skip`.
    let entry_ref_price = resolved.entry.reference_price();
    if let Some(elv) = verified
        .intent
        .entry_level_vetos
        .iter()
        .find(|elv| elv.is_past(entry_ref_price))
    {
        tracing::info!(
            "entry rejected: entry-level veto {} active (entry={entry_ref_price} past level={}) (id={})",
            elv.name,
            elv.level,
            verified.intent.id
        );
        return ActionResult::Rejected {
            status: 412,
            body: "veto active".to_string(),
            outcome: format!("rejected: veto-active ({})", elv.name),
        };
    }

    // allow_entry gate — operator's Tunable<bool> script sees the full
    // shell + resolved geometry. Sits after Resolved::from_intent
    // (Phase 2 bindings need it) and ahead of the broker call (cheap
    // 412 on false). Doesn't consume a retry slot — only a successful
    // broker placement does.
    match allow_entry_gate::evaluate(&verified.intent, &verified.shell, &resolved, pip_size) {
        allow_entry_gate::AllowEntryOutcome::Proceed => {}
        allow_entry_gate::AllowEntryOutcome::Blocked => {
            tracing::info!(
                "entry rejected: allow_entry returned false (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                status: 412,
                body: "entry blocked".to_string(),
                outcome: "rejected: allow-entry-false".into(),
            };
        }
        allow_entry_gate::AllowEntryOutcome::NeedsGoldenUnmet => {
            tracing::info!(
                "entry rejected: needs_golden set but shell.golden != Some(true) (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                status: 412,
                body: "entry blocked: needs-golden".to_string(),
                outcome: "rejected: needs-golden".into(),
            };
        }
        allow_entry_gate::AllowEntryOutcome::NeedsConfirmedUnmet => {
            tracing::info!(
                "entry rejected: needs_confirmed set but shell.signal_confirmed != Some(true) (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                status: 412,
                body: "entry blocked: needs-confirmed".to_string(),
                outcome: "rejected: needs-confirmed".into(),
            };
        }
        allow_entry_gate::AllowEntryOutcome::ScriptError { kind, message } => {
            tracing::error!(
                "allow_entry script error (id={}): {message}",
                verified.intent.id
            );
            return ActionResult::Rejected {
                status: 412,
                body: "entry blocked: script error".to_string(),
                outcome: format!("rejected: allow-entry-{kind}"),
            };
        }
    }

    // Per-account caps were resolved at the edge into `cfg.caps` (the wasm
    // worker from the KV account index, the native runtime from Postgres).
    // Apply the per-account narrowing: an account record can tighten the
    // worker-wide ceiling but never relax it.
    let caps = cfg.caps;
    let max_risk_pct = caps.resolve_max_risk_pct(worker_max_risk_pct);
    let max_open_positions = caps.resolve_max_open_positions(worker_max_open_positions);

    // Resolve the optional bar-based order expiry into a concrete
    // `cancel_at` *before* any broker work, so a bad `expiry_bars`
    // rejects (without poisoning the seen-id) rather than placing an
    // order we can't honour. `None` = no bar-expiry requested.
    let cancel_at = match verified.intent.expiry_bars.as_ref() {
        None => None,
        Some(tunable) => {
            let n = match super::shared::resolve_phase1_u32(
                "expiry-bars",
                Some(tunable),
                &verified.shell,
                0,
            ) {
                Ok(n) => n,
                Err(outcome) => {
                    tracing::info!("entry rejected: {outcome} (id={})", verified.intent.id);
                    return ActionResult::Rejected {
                        status: 412,
                        body: "entry blocked: expiry-bars script".to_string(),
                        outcome,
                    };
                }
            };
            match crate::intent::resolve_cancel_at(n, &verified.shell, verified.intent.not_after) {
                Ok(ts) => Some(ts),
                Err(err) => {
                    tracing::info!(
                        "entry rejected: expiry-bars out of range (id={}): {err}",
                        verified.intent.id
                    );
                    return ActionResult::Rejected {
                        status: 400,
                        body: "entry blocked: expiry-bars out of range".to_string(),
                        outcome: "rejected: expiry-bars-out-of-range".into(),
                    };
                }
            }
        }
    };

    // Market-hours entry blackout (System 1, the reject gate): reject a
    // brand-new entry that fires inside this instrument's daily close→open
    // gap, so a resting stop order is never left to trigger on the reopen
    // liquidity gap (the incident this feature fixes). The per-instrument
    // UTC no-entry windows are derived once a day by the 06:00 UTC cron
    // (`src/cron/blackout_hours.rs`) from the broker's session hours and
    // stored in KV. This is a pure KV read + a minute-of-day comparison —
    // no broker round-trip — so it sits ahead of the (broker-touching)
    // spread-blackout gate below.
    //
    // REJECT, NOT a delay (same discipline as spread-blackout): no KV
    // write, no re-fire scheduled. The next signal bar re-triggers and
    // re-runs this check — once the market has reopened the same entry
    // passes. Returning `ActionResult::Rejected` is a `Skip` in
    // `seen_decision` (no `mark_seen`), so this reject never poisons the
    // intent id; the in-hours refire is allowed through. See CLAUDE.md
    // "Replay protection scope". Do NOT add any KV write on this path.
    //
    // FAIL OPEN: a KV read hiccup, or an instrument with no derived
    // windows (24h / unparseable / not-yet-refreshed), must never block a
    // legitimate entry — `get_blackout_windows` returns an empty Vec in
    // those cases and `is_inside_any` is then always `false`.
    match store.get_blackout_windows(&resolved.instrument).await {
        Err(err) => {
            tracing::error!(
                "market-blackout: windows read failed for {} (id={}): {err} — failing open (allowing entry)",
                resolved.instrument,
                verified.intent.id
            );
        }
        Ok(windows) => {
            let now_min = now_utc_minute_of_day(now);
            if is_inside_any(now_min, &windows) {
                tracing::info!(
                    "entry rejected: market-blackout instrument={} now_utc_min={now_min} windows={windows:?} (id={})",
                    resolved.instrument,
                    verified.intent.id
                );
                return ActionResult::Rejected {
                    status: 423,
                    body: "entry blocked: market-hours blackout".to_string(),
                    outcome: "rejected: market-blackout".into(),
                };
            }
        }
    }

    // System 1 of the spread blackout: reject a brand-new entry that
    // fires during the post-NY-close liquidity trough when the live
    // spread on THIS instrument is elevated. Runs here — after every
    // gate (retry/cooldown/prep/veto/allow_entry) and `Resolved::from_intent`,
    // immediately before the broker order. The pure decision lives in
    // `spread_blackout::spread_blackout_decision`; this is the thin
    // KV-read + quote-sample wrapper around it.
    //
    // REJECT, NOT a delay: we do not persist anything, do not schedule a
    // re-fire, and do not touch KV here. The next legitimate signal bar
    // re-triggers the alert and re-runs this check — by then the spread
    // may have recovered and the same entry passes. Stateless + idempotent.
    //
    // SEEN-ID: returning `ActionResult::Rejected` is a `Skip` in
    // `seen_decision` (no `mark_seen`), so this reject does NOT poison the
    // intent id — the next fire is allowed through. See CLAUDE.md
    // "Replay protection scope". Do NOT add any KV write on this path.
    match store.get_spread_blackout_window().await {
        // Fail open on a transient KV read error — a blackout-window read
        // hiccup must never block a legitimate entry.
        Err(err) => {
            tracing::error!(
                "spread-blackout: window read failed (id={}): {err} — failing open (allowing entry)",
                verified.intent.id
            );
        }
        // Window closed — the overwhelmingly common path. Fall through
        // WITHOUT a broker round-trip (no `get_quote` call).
        Ok(None) => {}
        // Window open — sample the live spread for this instrument and
        // decide. A fine-spread instrument/day is not blacked out.
        Ok(Some(_window)) => match broker.get_quote(&resolved.instrument).await {
            // Fail open on a quote error at decision time: a transient
            // broker quote hiccup must not strand a real entry. (A
            // fail-closed variant is recorded in the sub-plan open
            // questions; flip this branch to reject if demo shows the
            // trough also degrades the quote endpoint.)
            Err(err) => {
                tracing::error!(
                    "spread-blackout: get_quote failed for {} (id={}): {err:?} — failing open (allowing entry)",
                    resolved.instrument,
                    verified.intent.id
                );
            }
            Ok(quote) => {
                let spread_pips = quote.spread() / pip_size;
                let threshold = spread_blackout::elevated_threshold_pips(&resolved.instrument);
                if spread_blackout::spread_blackout_decision(true, spread_pips, threshold) {
                    // Name the instrument's baked normal/spike so the
                    // operator can judge whether the block is right. Baked
                    // figures come from the spread-sampler baseline; absent
                    // for an uncatalogued instrument (then we only have the
                    // flat threshold to show).
                    let normal = match spread_blackout::baked_baseline(&resolved.instrument) {
                        Some((low, high, median)) => format!(
                            "{} normal spread ~{median:.1}p (seen {low:.1}–{high:.1}p)",
                            resolved.instrument
                        ),
                        None => format!("{} (no baseline)", resolved.instrument),
                    };
                    let message = format!(
                        "entry blocked: spread blackout — {normal}, current spread {spread_pips:.1}p > {threshold:.1}p; preventing entry for safety"
                    );
                    tracing::info!(
                        "entry rejected: spread-blackout instrument={} spread={spread_pips:.1}p > {threshold:.1}p (id={})",
                        resolved.instrument,
                        verified.intent.id
                    );
                    return ActionResult::Rejected {
                        status: 423,
                        body: message,
                        outcome: "rejected: spread-blackout".into(),
                    };
                }
            }
        },
    }

    // SL-vs-spread floor (hard limit, every entry): the stop-loss distance must
    // be at least `SL_MIN_SPREAD_MULTIPLE`× the live bid-ask spread, so a stop
    // is a real market level and not dominated by the cost of crossing the book.
    // Pure decision in `crate::intent::sl_spread_floor_violation`;
    // this is the live-quote wrapper. Mirrored at arm/build time (tv-arm,
    // trade-control) so a bad setup is caught before signing — this is the
    // real-time backstop.
    //
    // Unlike spread-blackout this samples the quote on EVERY entry (no window
    // guard), since the floor always applies. It is the only other broker
    // round-trip on the entry path; keep it right beside spread-blackout.
    //
    // FAIL OPEN on a quote error: a transient broker quote hiccup must not
    // strand a legitimate entry (same discipline as spread-blackout). REJECT is
    // a `Skip` in `seen_decision` (no `mark_seen`), so it never poisons the
    // intent id — the next signal bar refires and re-checks. Do NOT add a KV
    // write on this path.
    // The spread the floor sizes off. Prefer the MEAN of `ask_c − bid_c` over
    // the last `spread_window` closed bid/ask candles (default 5), so a single
    // spiky entry bar can't blow the 10× floor out; fall back to a single live
    // `get_quote` when the windowed read is unavailable (no plan granularity —
    // the webhook / blackout-restore paths pass `enter_granularity: None` — or a
    // candle-fetch error / all-degenerate window). See
    // `crate::intent::mean_spread`.
    let spread_source = windowed_entry_spread(
        broker,
        &resolved.instrument,
        &verified.intent,
        now,
        enter_granularity,
    )
    .await;
    let effective_spread = match spread_source {
        Some((mean, n)) => {
            tracing::info!(
                "sl-spread-floor: using windowed mean spread {mean} over last {n} candles for {} (id={})",
                resolved.instrument,
                verified.intent.id,
            );
            Some(mean)
        }
        None => match broker.get_quote(&resolved.instrument).await {
            Err(err) => {
                tracing::error!(
                    "sl-spread-floor: windowed spread unavailable and get_quote failed for {} (id={}): {err:?} — failing open (allowing entry)",
                    resolved.instrument,
                    verified.intent.id
                );
                None
            }
            Ok(quote) => {
                tracing::info!(
                    "sl-spread-floor: windowed spread unavailable, falling back to live quote spread {} for {} (id={})",
                    quote.spread(),
                    resolved.instrument,
                    verified.intent.id,
                );
                Some(quote.spread())
            }
        },
    };
    if let Some(spread_price) = effective_spread {
        let entry_price = entry_reference_price(&resolved.entry);
        let sl_distance = (entry_price - resolved.stop_loss).abs();
        // Operator-facing messages render distances in **raw price**, the
        // same unit the broker quotes in. The floor is a pure ratio of two
        // price distances (`sl_distance` vs `spread`), so the rule — and
        // its log — must not depend on `pip_size`: a wrong catalog pip
        // would make a correct decision *read* wrong. (See the SL-floor
        // spec; pip rendering was removed here for exactly this reason.)

        // SALVAGE-BY-WIDENING: rather than reject a too-tight stop outright,
        // try widening the SL to `SL_WIDEN_SPREAD_MULTIPLE`× the spread and
        // re-check the trade still clears its R-floor. A wider stop is
        // strictly *safer* against spread noise; we only reject if even the
        // widened stop can't hold an `>= min_r` trade against the fixed TP.
        // Pure decision in `crate::intent::widen_sl_to_spread_floor`; this
        // is the live-quote wrapper. The widened SL may sit past the
        // pattern's invalidation level — that's fine, the continuous
        // entry-level vetos abort the trade independently if price reaches
        // invalidation. Mutating `resolved.stop_loss` here flows into the
        // `EntryRequest` built just below.
        match crate::intent::widen_sl_to_spread_floor(
            entry_price,
            resolved.stop_loss,
            resolved.take_profit,
            spread_price,
            resolved.min_r,
        ) {
            crate::intent::SlWiden::Unchanged => {}
            crate::intent::SlWiden::Widened {
                new_stop_loss,
                new_sl_distance,
                new_r,
            } => {
                tracing::info!(
                    "sl-spread-floor: widened SL {old_sl} -> {new_stop_loss} for {} (sl_distance {sl_distance} -> {new_sl_distance}, spread {spread_price}, {mult:.0}x floor; R now {new_r:.2} >= min_r {min_r:.2}) (id={})",
                    resolved.instrument,
                    verified.intent.id,
                    old_sl = resolved.stop_loss,
                    mult = crate::intent::SL_WIDEN_SPREAD_MULTIPLE,
                    min_r = resolved.min_r,
                );
                resolved.stop_loss = new_stop_loss;
            }
            crate::intent::SlWiden::Reject {
                widened_stop_loss,
                widened_sl_distance,
                r_at_widen,
                min_r,
            } => {
                let spread_str = fmt_price_trim(spread_price);
                let widened_lvl_str = fmt_price_trim(widened_stop_loss);
                let widened_dist_str = fmt_price_trim(widened_sl_distance);
                let message = format!(
                    "entry blocked: SL too close to spread and widening to {mult:.0}x spread (SL would move to {widened_lvl_str}, sl_distance {widened_dist_str}, spread {spread_str}) would drop R to {r_at_widen:.2} < min_r {min_r:.2}",
                    mult = crate::intent::SL_WIDEN_SPREAD_MULTIPLE,
                );
                tracing::info!(
                    "entry rejected: sl-widen-below-min-r instrument={} spread={spread_str} widened_stop_loss={widened_lvl_str} widened_sl_distance={widened_dist_str} r_at_widen={r_at_widen:.3} < min_r={min_r} (id={})",
                    resolved.instrument,
                    verified.intent.id,
                );
                // Fold the deciding numbers into `outcome` (not just `body`):
                // the offline replay surfaces `outcome` verbatim on its
                // "BLOCKED — rejected: …" line, so without them the operator
                // sees the reject name but not *why*. Show the `spread` (the
                // ask−bid distance in price, what the floor sizes off), the
                // widened SL **price level** (`widened_sl_lvl`, same price units
                // as the entry/SL/TP levels — what the stop would move to), and
                // the R it would leave vs the floor. The widened *distance* is
                // omitted: it is always `10 × spread` (redundant with the level
                // + spread), and `body` keeps the fuller "widening to 10x
                // spread" sentence.
                let outcome = format!(
                    "rejected: sl-widen-below-min-r (spread={spread_str} widened_sl_lvl={widened_lvl_str} r_at_widen={r_at_widen:.2} < min_r={min_r:.2})",
                );
                return ActionResult::Rejected {
                    status: 422,
                    body: message,
                    outcome,
                };
            }
        }
    }

    let entry_request = EntryRequest {
        instrument: &resolved.instrument,
        direction: resolved.direction,
        entry: resolved.entry.clone(),
        stop_loss: resolved.stop_loss,
        take_profit: resolved.take_profit,
        risk: resolved.risk,
        dry_run: resolved.dry_run,
    };

    // Log inputs + R-multiple up front so the operator sees the
    // planned trade geometry before the broker work begins. The
    // broker's own `sizing:` log then adds the computed units once
    // equity / FX have been fetched.
    let r_distance = (entry_reference_price(&resolved.entry) - resolved.stop_loss).abs();
    let tp_distance = (resolved.take_profit - entry_reference_price(&resolved.entry)).abs();
    let r_multiple = if r_distance > 0.0 {
        tp_distance / r_distance
    } else {
        f64::NAN
    };
    let prefix = if resolved.dry_run { "DRY-RUN " } else { "" };
    tracing::info!(
        "{prefix}entry id={} instrument={} direction={:?} entry={:?} sl={} tp={} risk={:?} r={:.3}",
        verified.intent.id,
        resolved.instrument,
        resolved.direction,
        resolved.entry,
        resolved.stop_loss,
        resolved.take_profit,
        resolved.risk,
        r_multiple,
    );

    // First placement. On `EntryTooCloseToMarket` (TN `#19-10`), the
    // stop trigger was overtaken by price; the optional `recover_entry`
    // policy may recover with a *single* synchronous market re-place
    // (never a loop — a too-close means price is moving). The re-place
    // is the SAME intended entry, so it shares `retry_attempt_no` and
    // does not consume an extra multi-shot slot.
    let placement = match broker
        .place_entry(max_risk_pct, max_open_positions, &entry_request)
        .await
    {
        Ok(order_id) => Ok(order_id),
        Err(EntryError::EntryTooCloseToMarket) => {
            place_entry_too_close_fallback(
                broker,
                &resolved,
                &verified.intent.id,
                max_risk_pct,
                max_open_positions,
            )
            .await
        }
        Err(err) => Err(err),
    };

    match placement {
        Ok(order_id) => {
            if resolved.dry_run {
                tracing::info!("DRY-RUN entry id={} (not placed)", verified.intent.id);
                ActionResult::Ok(format!("dry-run: id={}", verified.intent.id))
            } else {
                tracing::info!("entry placed id={} order={}", verified.intent.id, order_id);
                if let Some(attempt_no) = retry_attempt_no {
                    // Break-even snapshot: only when the enter carried a
                    // `breakeven` rule AND we know the trade's timeframe (engine
                    // path). The cron joins the open position back to this row,
                    // fetches closed candles at `granularity`, and moves the SL
                    // to entry once a candle closes past 50%-to-TP.
                    let breakeven_snapshot = match (resolved.breakeven, enter_granularity) {
                        (Some(rule), Some(granularity)) => Some(crate::state::BreakevenSnapshot {
                            rule,
                            entry_price: resolved.entry.reference_price(),
                            take_profit: resolved.take_profit,
                            granularity,
                        }),
                        _ => None,
                    };
                    crate::retry_gate::record_placement(
                        store,
                        &verified.intent,
                        verified.shell.time,
                        verified.intent.not_after,
                        now,
                        attempt_no,
                        &order_id,
                        resolved.direction,
                        resolved.stop_loss,
                        cancel_at,
                        breakeven_snapshot,
                    )
                    .await;
                }
                // Spread-blackout System 3 (Sub-plan 5): persist the raw signed
                // body keyed by the broker order id so the apply cron can
                // recover THIS order's intent (it finds a broker pending order,
                // never a signed intent) and re-drive it on recovery. Only when
                // we have the signed bytes in hand. No TTL — the body is
                // per-trade lifecycle state and is removed by `plan purge`
                // (no longer aged out with its EntryAttempt). Best-effort: a
                // write failure only costs the blackout-restore ability for this
                // one order, never the placement.
                if let Some(body) = raw_body
                    && let Err(err) = store.put_order_body(&order_id, body).await
                {
                    tracing::error!(
                        "order-body store for blackout-restore failed (order={order_id}): {err} \
                         — this order can't be blackout-cancelled+restored"
                    );
                }
                ActionResult::Ok(format!("entered: order={order_id}"))
            }
        }
        Err(err) => {
            // Stays `ActionResult::Failed` (a Skip in `seen_decision`):
            // a too-close / broker failure must never poison the seen-id
            // so the next signal bar can retry. The too-close case gets
            // a distinct outcome string for log-grep observability.
            let outcome = recover_entry::outcome_for_entry_error(&err);
            tracing::error!("entry failed: {err} ({outcome})");
            ActionResult::Failed(outcome)
        }
    }
}

/// Result of the per-bar M/W geometry update ([`maybe_update_mw_state`]).
enum MwStateOutcome {
    /// Not an M/W enter with a `trade_id`, or the bar carried no `open`:
    /// resolve against the baked params (the standard dispatch).
    NotMw,
    /// M/W setup still valid; resolve this bar against these effective
    /// (live-corrected) params.
    Proceed(crate::intent::MwParams),
    /// The setup was cancelled this bar (60% validity floor breached).
    /// Carries the terminal [`ActionResult`] the caller should return.
    Cancelled(ActionResult),
}

/// Evolve the live M/W geometry for this bar and decide how to resolve.
///
/// Only acts on M/W enters that carry a `trade_id` (the KV state is
/// trade-scoped). Reads the prior `MwState`, runs the pure
/// [`plan_mw_update`], and:
///
/// - **Proceed** → persists the updated state (when it changed) and returns
///   the effective [`MwParams`][crate::intent::MwParams] to resolve against.
/// - **Cancel** → cancels any pending order for the instrument, writes a
///   trade-scoped `mw-cancel` veto (so later fires of this `05-enter` are
///   blocked — it lists `mw-cancel` in its `vetos`), clears the state row,
///   and returns a rejection. It **never closes an open position** — the
///   veto is StopNextEntry-class; cancelling pending is the only broker
///   side effect (see `veto_close_only_when_thesis_invalidated`).
/// - **NoChange / NotMw** → `NotMw`, falling back to baked resolution.
///
/// Fail-soft: a KV read/write error logs and falls back to baked geometry
/// rather than blocking a legitimate entry.
async fn maybe_update_mw_state<B: Broker>(
    broker: &B,
    store: &impl StateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> MwStateOutcome {
    let intent = &verified.intent;
    let Some(mw) = intent.mw else {
        return MwStateOutcome::NotMw;
    };
    let Some(trade_id) = intent.trade_id.as_deref() else {
        // No trade_id → no trade-scoped state to evolve; baked resolution.
        return MwStateOutcome::NotMw;
    };
    let Some(direction) = intent.direction else {
        return MwStateOutcome::NotMw;
    };
    let account = intent.account.as_deref();

    let prior = match store.get_mw_state(account, trade_id).await {
        Ok(p) => p,
        Err(err) => {
            // Fail-soft: don't block a valid entry on a KV blip.
            tracing::error!(
                "mw-state get failed (trade_id={trade_id}): {err} — using baked geometry"
            );
            return MwStateOutcome::NotMw;
        }
    };

    let ttl_seconds = veto_ttl_seconds(0, intent.not_after, now);
    let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
    let anchors = MwAnchors {
        direction,
        runup_start: mw.runup_start,
        left_shoulder: mw.first_point,
        baked_neckline: mw.neckline,
        drawn_right_shoulder: mw.right_shoulder,
    };

    match plan_mw_update(anchors, prior, &verified.shell, now, expires_at) {
        MwUpdate::NoChange => MwStateOutcome::NotMw,
        MwUpdate::Proceed { state, changed } => {
            if changed {
                if let Err(err) = store
                    .upsert_mw_state(account, trade_id, &state, ttl_seconds)
                    .await
                {
                    // Persist failure is non-fatal: we still resolve this bar
                    // against the freshly-computed geometry; next bar re-derives
                    // from the prior row (or baked if the write never lands).
                    tracing::error!("mw-state upsert failed (trade_id={trade_id}): {err}");
                }
                tracing::info!(
                    "mw-state updated trade_id={trade_id} neckline={} right_shoulder={:?}",
                    state.neckline,
                    state.right_shoulder
                );
            }
            MwStateOutcome::Proceed(effective_mw_params(&mw, &state, direction))
        }
        MwUpdate::Cancel => {
            let cancelled = broker
                .cancel_pending_for_instrument(&intent.instrument)
                .await;
            if let Err(err) = store
                .set_veto(
                    account,
                    trade_id,
                    &intent.instrument,
                    MW_CANCEL_VETO_NAME,
                    ttl_seconds,
                )
                .await
            {
                tracing::error!("mw-state cancel: set_veto failed (trade_id={trade_id}): {err}");
            }
            record_control_event_for(
                store,
                account,
                Some(trade_id),
                crate::control_event::ControlKind::Veto,
                MW_CANCEL_VETO_NAME,
                &intent.instrument,
                ttl_seconds,
                now,
                None,
            )
            .await;
            // Clear the state row so a re-armed setup reusing the trade_id
            // starts clean. Best-effort.
            if let Err(err) = store.clear_mw_state(account, trade_id).await {
                tracing::error!("mw-state cancel: clear failed (trade_id={trade_id}): {err}");
            }
            tracing::info!(
                "mw-state CANCEL trade_id={trade_id} instrument={} account={} cancelled={cancelled} pending; mw-cancel veto set",
                intent.instrument,
                account.unwrap_or("<global>")
            );
            MwStateOutcome::Cancelled(ActionResult::Rejected {
                status: 412,
                body: "mw pattern cancelled (validity floor breached)".to_string(),
                outcome: "rejected: mw-cancel (validity-floor)".into(),
            })
        }
    }
}

/// Single synchronous market re-place for a stop-entry rejected with
/// `#19-10` ("entry too close to / wrong side of market"). Reads the
/// current market price, applies the `recover_entry` slippage guard
/// (pure [`recover_entry::recover_entry_plan`]), and on a within-threshold
/// `market` action re-places as a **market order** sized against the
/// actual fill reference — a worse market fill changes the stop distance
/// and therefore the 1%-equity position size, so the broker re-runs
/// sizing from the market reference rather than the stop-trigger math.
///
/// For `action: limit` it instead re-places a **limit** order resting at
/// the original trigger (after a geometry guard — a limit on the wrong
/// side would be a `#19-9`), preserving the planned R and waiting for a
/// pullback. No fresh sizing: the entry reference is unchanged. The
/// resting limit is recorded as a normal `EntryAttempt` by the caller, so
/// the cron sweep cancels it when the alert window / `expiry_bars` lapses
/// — no broker-native GTD required.
///
/// Returns the original [`EntryError::EntryTooCloseToMarket`] (so the
/// caller surfaces the distinct outcome) when the fallback is absent,
/// out of threshold, `skip`, a wrong-side `limit`, or the re-place
/// itself fails / the price read fails. One attempt only.
async fn place_entry_too_close_fallback<B: Broker>(
    broker: &B,
    resolved: &crate::intent::Resolved,
    intent_id: &str,
    max_risk_pct: f64,
    max_open_positions: u32,
) -> Result<String, EntryError> {
    use crate::intent::ResolvedEntry;

    // Only stop entries carry the fallback; a too-close on anything else
    // (shouldn't happen) is terminal.
    let trigger_price = match &resolved.entry {
        ResolvedEntry::Stop { trigger_price } => *trigger_price,
        _ => return Err(EntryError::EntryTooCloseToMarket),
    };

    // The current price drives both the slippage guard and the new
    // market reference. A failed read is "price unavailable" → skip.
    let current_price = match broker.get_current_price(&resolved.instrument).await {
        Ok(p) => p,
        Err(err) => {
            tracing::error!(
                "too-close fallback: get_current_price({}) failed: {err} (id={intent_id})",
                resolved.instrument
            );
            return Err(EntryError::EntryTooCloseToMarket);
        }
    };

    match recover_entry::recover_entry_plan(
        resolved.recover_entry.as_ref(),
        resolved.direction,
        trigger_price,
        current_price,
    ) {
        recover_entry::RecoverEntryPlan::Skip { reason } => {
            tracing::info!(
                "too-close fallback: not recovering (id={intent_id} reason={reason} trigger={trigger_price} price={current_price})"
            );
            Err(EntryError::EntryTooCloseToMarket)
        }
        recover_entry::RecoverEntryPlan::Market { reference_price } => {
            tracing::info!(
                "too-close fallback: re-placing as MARKET (id={intent_id} trigger={trigger_price} price={reference_price})"
            );
            // Re-size against the actual fill reference: build a fresh
            // request whose entry is a market order at the current
            // price. The broker computes stop_distance from this
            // reference (TN re-fetches live bid/ask; OANDA uses it
            // directly), so the position size reflects the worse fill.
            let market_request = EntryRequest {
                instrument: &resolved.instrument,
                direction: resolved.direction,
                entry: ResolvedEntry::Market { reference_price },
                stop_loss: resolved.stop_loss,
                take_profit: resolved.take_profit,
                risk: resolved.risk,
                dry_run: resolved.dry_run,
            };
            match broker
                .place_entry(max_risk_pct, max_open_positions, &market_request)
                .await
            {
                Ok(order_id) => {
                    tracing::info!(
                        "too-close fallback: market re-place succeeded (id={intent_id} order={order_id})"
                    );
                    Ok(order_id)
                }
                Err(err) => {
                    // One attempt only — do not loop. Surface the
                    // original too-close identity so telemetry shows the
                    // recovery was attempted and failed, and the seen-id
                    // stays un-poisoned for the next bar.
                    tracing::error!(
                        "too-close fallback: market re-place failed: {err} (id={intent_id})"
                    );
                    Err(EntryError::EntryTooCloseToMarket)
                }
            }
        }
        recover_entry::RecoverEntryPlan::Limit { trigger_price } => {
            tracing::info!(
                "too-close fallback: re-placing as LIMIT at original trigger (id={intent_id} trigger={trigger_price} price={current_price})"
            );
            // The entry reference is unchanged (the limit rests at the
            // original trigger), so the stop distance — and therefore the
            // 1%-equity sizing — is identical to the original plan. Reuse
            // the resolved stop/take-profit/risk verbatim; the broker
            // sizes from the limit trigger just as it would have from the
            // stop trigger.
            let limit_request = EntryRequest {
                instrument: &resolved.instrument,
                direction: resolved.direction,
                entry: ResolvedEntry::Limit { trigger_price },
                stop_loss: resolved.stop_loss,
                take_profit: resolved.take_profit,
                risk: resolved.risk,
                dry_run: resolved.dry_run,
            };
            match broker
                .place_entry(max_risk_pct, max_open_positions, &limit_request)
                .await
            {
                Ok(order_id) => {
                    tracing::info!(
                        "too-close fallback: limit re-place succeeded (id={intent_id} order={order_id})"
                    );
                    Ok(order_id)
                }
                Err(err) => {
                    // One attempt only. Surface the original too-close
                    // identity so the seen-id stays un-poisoned and the
                    // next bar can retry.
                    tracing::error!(
                        "too-close fallback: limit re-place failed: {err} (id={intent_id})"
                    );
                    Err(EntryError::EntryTooCloseToMarket)
                }
            }
        }
    }
}

/// Reference price for risk math — for market orders it's the close,
/// for stop/limit it's the trigger. Same pick the broker layer uses.
fn entry_reference_price(entry: &crate::intent::ResolvedEntry) -> f64 {
    use crate::intent::ResolvedEntry;
    match entry {
        ResolvedEntry::Market { reference_price } => *reference_price,
        ResolvedEntry::Stop { trigger_price } => *trigger_price,
        ResolvedEntry::Limit { trigger_price } => *trigger_price,
    }
}

/// The mean bid-ask spread (raw price) over the last `intent.spread_window`
/// closed bid/ask candles, and how many bars fed the mean — or `None` when a
/// windowed read isn't available.
///
/// The entry SL-spread floor uses this in preference to a single live
/// `get_quote` so a spiky entry candle can't dominate the `10× spread` floor
/// (see [`crate::intent::mean_spread`]). It mirrors the replay's
/// `apply_entry_spread_floor` window so worker and replay size the floor off the
/// same statistic.
///
/// Returns `None` (caller falls back to the live quote) when:
/// - `enter_granularity` is absent (webhook / blackout-restore paths have no
///   plan timeframe to fetch on),
/// - the broker's `get_bidask_candles` errors or is the default no-op (a broker
///   with no two-sided feed), or
/// - every candle in the window has a degenerate spread (`mean_spread` → `None`).
///
/// The window is a count-back: `since = now − (window + 2) × bar`, giving a
/// little slack so at least `window` closed bars return; the **last** `window`
/// of them (the most recent, including the just-closed entry bar) feed the mean.
async fn windowed_entry_spread<B: Broker>(
    broker: &B,
    instrument: &str,
    intent: &crate::intent::Intent,
    now: chrono::DateTime<chrono::Utc>,
    enter_granularity: Option<crate::broker::Granularity>,
) -> Option<(f64, usize)> {
    let granularity = enter_granularity?;
    let window = intent
        .spread_window
        .unwrap_or(crate::intent::DEFAULT_SPREAD_WINDOW)
        .max(1);
    // Count-back with slack so >= `window` closed bars land in the range.
    let lookback_bars = (window as i64) + 2;
    let since = now - chrono::Duration::seconds(granularity.seconds() * lookback_bars);
    let candles = match broker
        .get_bidask_candles(instrument, granularity, since, now)
        .await
    {
        Ok(c) if !c.is_empty() => c,
        Ok(_) => {
            tracing::info!(
                "sl-spread-floor: windowed spread read returned no candles for {instrument} — falling back to live quote"
            );
            return None;
        }
        Err(err) => {
            tracing::info!(
                "sl-spread-floor: windowed spread read failed for {instrument}: {err} — falling back to live quote"
            );
            return None;
        }
    };
    // Reduce via the SHARED trailing-window mean (the same fn the replay's
    // Fire-builder calls on candles from the same `get_bidask_candles`
    // provider), so worker and replay size the floor off an identical statistic.
    crate::broker::trailing_spread_mean(&candles, window)
}

#[cfg(test)]
mod fmt_tests {
    use super::fmt_price_trim;

    #[test]
    fn trims_float_dust_and_trailing_zeros() {
        // Index-scale level with float dust → trimmed to its real precision.
        assert_eq!(fmt_price_trim(209.9930432131929), "209.993043");
        // A clean whole-ish level keeps no trailing zeros or dot.
        assert_eq!(fmt_price_trim(209.99), "209.99");
        assert_eq!(fmt_price_trim(10.0), "10");
        // Sub-tick FX precision (5dp) survives.
        assert_eq!(fmt_price_trim(1.10345), "1.10345");
        // Non-finite falls back to the default float render, not a panic.
        assert_eq!(fmt_price_trim(f64::NAN), "NaN");
    }
}
