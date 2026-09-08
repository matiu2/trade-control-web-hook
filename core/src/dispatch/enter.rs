//! The `Enter` dispatch path: gates → sizing → broker placement → recovery.

use super::action_result::ActionResult;
use super::shared::record_control_event_for;
use crate::allow_entry_gate;
use crate::broker::{Broker, EntryError, EntryRequest};
use crate::dispatch_config::DispatchConfig;
use crate::incoming;
use crate::intent::{
    MW_CANCEL_VETO_NAME, MwAnchors, MwUpdate, ResolveError, Resolved, effective_mw_params,
    plan_mw_update,
};
use crate::recover_entry;
use crate::spread_blackout;
use crate::state::{StateStore, veto_ttl_seconds};

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
#[allow(clippy::too_many_arguments)]
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
    // `true` when this call is the spread-hour lifecycle **restoring** a resting
    // order it earlier cancelled (RAIL 7), NOT a fresh alert fire or a multi-shot
    // re-entry. A restore re-places the SAME order the lifecycle owns the
    // cancel→restore correlation for, so it must bypass the retry gate entirely —
    // exactly as a single-shot enter already does. Skipping the gate:
    //   * avoids the same-bar `is_retry_fire_seen` dedup that would otherwise
    //     `retry-fire-replay`-REJECT the re-drive (the original fire on this
    //     `shell.time` was already marked seen when it first placed), and
    //   * consumes NO `max_retries` slot / records no new `EntryAttempt` (the
    //     re-placement continues the original attempt, it is not a new re-entry).
    // Every non-restore caller passes `false` and keeps the full gate. This is the
    // `restoring` flag the blackout-restore docstring anticipated as the correct
    // long-term answer to "a multi-shot re-drive shouldn't burn a slot".
    restore: bool,
) -> ActionResult {
    // Blackout gate — if any pause for this trade_id is active, reject
    // before doing any other work. Pauses are intentionally cheap to
    // check (one prefix list on the trade's own keys) so they can sit
    // ahead of the cooldown/prep/veto chain. Trades minted
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

    // Prep gate — every slot in `requires_preps` must be satisfied, and the
    // satisfying preps' `set_at` timestamps must be strictly increasing in slot
    // order.
    //
    // A slot is a `PrepReq`: `All(step)` requires that one prep; `Any(alts)` is
    // satisfied by whichever listed alternative is set (either/or — e.g. a retest
    // OR a pullback). For an `Any` slot we take the **earliest** alternative whose
    // `set_at` is strictly after the previous slot's, keeping the ordered chain as
    // permissive as possible. A single-member group behaves exactly like `All`, so
    // the legacy flat `[break-and-close, retest]` list is byte-for-byte the same
    // decision it was before this generalisation.
    let mut prev_ts: Option<chrono::DateTime<chrono::Utc>> = None;
    for slot in &verified.intent.requires_preps {
        let alts = slot.alternatives();
        // Look up each alternative's prep `set_at` once, in wire order, then let
        // the pure `resolve_slot` decide the ordered-OR outcome. A store error on
        // any lookup is fail-closed (reject), same as before.
        let mut alt_set_ats = Vec::with_capacity(alts.len());
        for step in alts {
            match store
                .get_prep(
                    verified.intent.account.as_deref(),
                    &verified.intent.instrument,
                    step,
                )
                .await
            {
                Ok(set_at) => alt_set_ats.push(set_at),
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
        // Label for diagnostics: the single prep name for an `All` slot, or the
        // pipe-joined alternatives for an `Any` group (e.g. `retest|pullback`).
        let label = alts.join("|");
        match crate::intent::resolve_slot(&alt_set_ats, prev_ts) {
            crate::intent::SlotOutcome::Satisfied(set_at) => prev_ts = Some(set_at),
            crate::intent::SlotOutcome::OutOfOrder => {
                tracing::info!(
                    "entry rejected: prep {label} not after previous (id={})",
                    verified.intent.id
                );
                return ActionResult::Rejected {
                    status: 412,
                    body: "prep order violated".to_string(),
                    outcome: format!("rejected: prep-order-violated ({label})"),
                };
            }
            crate::intent::SlotOutcome::Missing => {
                tracing::info!(
                    "entry rejected: missing prep {label} (id={})",
                    verified.intent.id
                );
                return ActionResult::Rejected {
                    status: 412,
                    body: "missing prep".to_string(),
                    outcome: format!("rejected: missing-prep ({label})"),
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

    // Tick size precedence mirrors pip: baked signed intent (tv-arm reads it
    // from `instrument-lookup`) → `cfg.tick_size` (edge-resolved) → `pip_size`.
    // The pip fallback is a safe coarser over-approximation (`tick <= pip` for
    // every asset class) so a legacy intent with no baked tick still rounds
    // acceptably and still places — fail-open, never fail-closed. The resolver
    // treats a non-positive tick as identity, so worst case is today's
    // behaviour (no rounding). Used to snap order prices onto the instrument's
    // grid before the broker rejects over-precision.
    let tick_size = verified
        .intent
        .tick_size
        .or(cfg.tick_size)
        .unwrap_or(pip_size);

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
        Some(mw) => Resolved::from_mw_intent(&verified.intent, &verified.shell, mw, tick_size),
        // Everything else (and M/W with no trade_id / no `open`): the
        // standard dispatch, which itself routes baked M/W to from_mw_intent.
        None => Resolved::from_intent(&verified.intent, &verified.shell, pip_size, tick_size),
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
    // FAIL OPEN: an instrument not in the baked market-hours table (an
    // uncatalogued symbol) must never block a legitimate entry —
    // `market_hours_blocked` returns `false` for an unknown symbol. There is no
    // KV read and no daily refresh anymore: the mask is candle-derived and baked
    // (`intent::market_hours_blocked`), weekday-aware, so it blocks a real
    // Friday-night / mid-week-daily-close bar without touching a same-clock-time
    // mid-week bar. See the `market-hours-blackout-weekly-gap-bug` memory.
    //
    // RESTORE BYPASS: a `restore` re-drive re-places an order the lifecycle
    // already cancelled for a spread hour — it is NOT a fresh entry, so it must
    // skip the two blackout REJECT gates (this market-hours one and the
    // spread-blackout one below), same discipline as the retry-gate bypass above.
    // Otherwise a restore that lands while the daily blackout window is open (or
    // while the ~3h NY-close spread window is open) would be rejected and the
    // order silently DROPPED (`restore_one_order` cleans up on any non-Ok), never
    // re-placed even on the next clean bar — a live-money bug for
    // blackout-cancelled resting orders. The lifecycle's own `is_spread_hour` /
    // `off_now` timing already governs WHEN a restore may run.
    if !restore {
        if crate::intent::market_hours_blocked(&resolved.instrument, now) {
            tracing::info!(
                "entry rejected: market-blackout instrument={} now={now} (id={})",
                resolved.instrument,
                verified.intent.id
            );
            return ActionResult::Rejected {
                status: 423,
                body: "entry blocked: market-hours blackout".to_string(),
                outcome: "rejected: market-blackout".into(),
            };
        }

        // System 1 of the spread blackout: reject a brand-new entry that
        // fires during the post-NY-close liquidity trough when the live
        // spread on THIS instrument is elevated. Runs here — after every
        // reject-capable gate (cooldown/prep/veto/allow_entry) and
        // `Resolved::from_intent`. It is itself reject-capable, so it stays
        // ABOVE the retry gate, which is now last (see the rail there). The
        // pure decision lives in
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
                        // figures come from the candle-derived baseline table;
                        // absent for an uncatalogued instrument (then we only have
                        // the flat threshold to show).
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
    // The stop as DRAWN, before the floor below may widen it. `resolved.stop_loss`
    // is mutated in place by the widen, so this is the only point the operator's
    // own level is still available — and a later shrink has nothing to shrink
    // *toward* without it. Snapshotted onto the attempt row via
    // `OrderControlSnapshot::original_stop_loss`.
    let drawn_stop_loss = resolved.stop_loss;
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
                // PARK, don't discard. Before stored orders existed this
                // returned a terminal 422 and the setup was gone: a rejection
                // left no trace (`EntryAttempt.broker_order_id` is non-`Option`,
                // written only on `Ok`), so a later fire re-derived the same
                // verdict from scratch and died the same way. That is the
                // `sgdjpy-spread-floor-min-r-block` 0R loss — three fires over
                // 17h, each independently rejected, plan dead at expiry.
                //
                // The spread being wide *now* is a reason not to place *now*,
                // not a reason to throw the setup away. Parking keeps the
                // signed body and the drawn geometry so the trade is re-checked
                // every candle and placed the moment it clears its R-floor.
                // Still rejected (nothing was placed) — but recoverable.
                let parked = park_stored_entry(
                    store,
                    verified,
                    crate::order_control::StoredReason::BelowMinR,
                    trade_id,
                    &resolved.instrument,
                    sl_distance,
                    (entry_price - resolved.take_profit).abs(),
                    min_r,
                    raw_body,
                    enter_granularity,
                    now,
                )
                .await;
                let outcome = format!(
                    "rejected: sl-widen-below-min-r (spread={spread_str} widened_sl_lvl={widened_lvl_str} r_at_widen={r_at_widen:.2} < min_r={min_r:.2}{parked})",
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
        contract_multiplier: resolved.contract_multiplier,
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
    //
    // ## Why this gate is LAST — the rail: never cancel an order you cannot
    // re-place
    //
    // This gate is the only one on the entry path with a **broker side effect**:
    // its `AttemptState::Pending` arm CANCELS the prior attempt's still-resting
    // order (`retry_gate::evaluate`) on the understanding that the caller will
    // immediately place a fresh one in its stead. That bargain only holds if the
    // placement is actually attempted — so every gate that can *reject* the fire
    // must have already run by the time we get here.
    //
    // It used to sit at the top of `run_enter`, ahead of the cooldown, prep,
    // veto, entry-level-veto, `allow_entry`, expiry-bars, market-hours,
    // spread-blackout and SL-spread-floor gates. Any one of them rejecting after
    // the gate had cancelled destroyed a live resting order with nothing placed
    // and no restore. That is what happened on 2026-08-07 (OANDA
    // `101-011-31142393-003`, plan `hs-eur-cad-08ca0693`): the 17:00:01 `05-enter`
    // fire cancelled resting limit order 2318 at 17:00:23Z and was then rejected
    // by the prep gate with `prep-order-violated (retest)`. 2318 was never
    // replaced and the setup was forfeited.
    //
    // The rail itself is not new — `order_control::reprice`'s module docs state
    // it outright ("never cancel an order you cannot re-place": the signed body
    // is recovered and verified *before* the cancel, and a body that won't verify
    // aborts with the order left resting), and `pending_lifecycle`'s cancel pass
    // follows the same store-first ordering. The retry gate was the one path that
    // did not honour it.
    //
    // Reordering rather than compensating is deliberate: an "un-cancel" is
    // another failure mode (the re-place can itself be rejected, and the broker
    // may have filled in between), whereas moving the gate removes the window
    // entirely. Nothing between here and `place_entry` can reject, and nothing
    // above depends on the gate's output — `retry_attempt_no` is read only
    // *after* a successful placement, to stamp the `EntryAttempt` row.
    //
    // Adding a new reject-capable gate BELOW this point reopens the hole. Put it
    // above, with the others.
    //
    // A restore re-places an order the lifecycle already cancelled — it is
    // neither a fresh fire nor a new multi-shot re-entry, so it skips the retry
    // gate entirely (like single-shot). This is what un-blocks the cancel→restore
    // sequence: without it, the re-drive of a multi-shot resting order is
    // `retry-fire-replay`-rejected on its own already-seen `shell.time` and the
    // order is never re-placed (a live-money bug for multi-shot resting orders,
    // hidden until now behind the replay's phantom fill).
    let retry_attempt_no = if !restore
        && !matches!(
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

    // First placement. On `EntryTooCloseToMarket` (TN `#19-10`), the
    // stop trigger was overtaken by price; the optional `recover_entry`
    // policy may recover with a *single* synchronous market re-place
    // (never a loop — a too-close means price is moving). The re-place
    // is the SAME intended entry, so it shares `retry_attempt_no` and
    // does not consume an extra multi-shot slot.
    // Set by the fallback when it declines to recover, so the failure
    // outcome can name *why* (not just that the entry was forfeited).
    let mut recover_skip_reason: Option<&'static str> = None;
    let placement = match broker
        .place_entry(max_risk_pct, max_open_positions, &entry_request)
        .await
    {
        Ok(placed) => Ok(placed),
        Err(EntryError::EntryTooCloseToMarket) => {
            place_entry_too_close_fallback(
                broker,
                &resolved,
                &verified.intent.id,
                max_risk_pct,
                max_open_positions,
                &mut recover_skip_reason,
            )
            .await
        }
        Err(err) => Err(err),
    };

    match placement {
        Ok(placed) => {
            let order_id = placed.order_id.clone();
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
                    // The geometry the every-candle order-control re-check needs
                    // (rule 7). `original_stop_loss` is the DRAWN level captured
                    // before the spread floor may have widened
                    // `resolved.stop_loss` in place — a shrink must never tighten
                    // past it.
                    let order_control = Some(crate::state::OrderControlSnapshot {
                        original_stop_loss: drawn_stop_loss,
                        take_profit_price: resolved.take_profit,
                        min_r: resolved.min_r,
                        bar_seconds: enter_granularity.map(crate::broker::Granularity::seconds),
                    });
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
                        order_control,
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
                // The operator's trade log reads this string verbatim (it rides
                // `DispatchOutcome.outcome` into `plan-timeline` / `journal`),
                // so the size + requested rate go here rather than only into
                // the broker's own log line. Empty when the broker reported
                // neither.
                //
                // The VERB is chosen from the order type, because a placement is
                // not a fill — see `placement_verb`.
                ActionResult::Ok(format!(
                    "{}: order={order_id}{}",
                    placement_verb(&resolved.entry),
                    placed.describe_fill()
                ))
            }
        }
        // Sizing floored to nothing. Unlike every other `EntryError` this is a
        // deterministic function of (equity, stop distance, contract multiplier)
        // — none of which can change before the next bar — so leaving it to the
        // generic `Failed` arm below means re-running the identical computation
        // on every fire, failing identically, with no operator signal and no
        // termination. Futures make it acute: one contract is 100% granularity.
        //
        // PARK instead, exactly as the sub-min-R rejection above does: the
        // setup is preserved with its signed body, re-checked once per new bar
        // (see `StoredReason::rechecked_per_bar` — the *only* honest cadence,
        // because the `Broker` trait exposes no equity to re-test against), and
        // dropped at its own `drop_at` rather than retried forever.
        //
        // Still `Rejected`, not `Ok`: nothing was placed. 422 rather than the
        // generic 502 because the request is well-formed and the broker is
        // healthy — the account simply cannot carry this trade at this size.
        Err(EntryError::UnitsBelowMinimum) => {
            let parked = park_stored_entry(
                store,
                verified,
                crate::order_control::StoredReason::BelowMinSize,
                trade_id,
                &resolved.instrument,
                r_distance,
                tp_distance,
                resolved.min_r,
                raw_body,
                enter_granularity,
                now,
            )
            .await;
            tracing::info!(
                "entry rejected: units-below-minimum instrument={} sl_distance={r_distance} \
                 (id={}){parked}",
                resolved.instrument,
                verified.intent.id,
            );
            ActionResult::Rejected {
                status: 422,
                body: format!(
                    "entry blocked: computed position size is below the broker minimum \
                     (sl_distance {r_distance})",
                ),
                outcome: format!("rejected: units-below-minimum{parked}"),
            }
        }
        Err(err) => {
            // Stays `ActionResult::Failed` (a Skip in `seen_decision`):
            // a too-close / broker failure must never poison the seen-id
            // so the next signal bar can retry. The too-close case gets
            // a distinct outcome string for log-grep observability.
            let outcome = recover_entry::outcome_for_entry_failure(&err, recover_skip_reason);
            tracing::error!("entry failed: {err} ({outcome})");
            ActionResult::Failed(outcome)
        }
    }
}

/// The verb for a successful placement's operator-facing outcome string:
/// `entered` for an order that has **filled**, `placed` for one that is merely
/// **resting**.
///
/// # Why this is not one word
///
/// `run_enter` reported `entered: order=<id>` for every order type the moment
/// `place_entry` returned. For a **market** order that is true — the broker
/// fills it on receipt, and a position exists. For a **stop** or **limit** order
/// it is not: the order rests on the book at a trigger price and may never fill
/// at all. Both produced a byte-identical line, so no reader — human or code —
/// could tell a live position from an untouched resting order.
///
/// That is exactly how the 2026-08-07 incident (OANDA
/// `101-011-31142393-003`, plan `hs-eur-cad-08ca0693`) reached the operator's
/// journal as a **WIN +2.63R**. Two limit orders were placed, both logged
/// `entered:`, both rested about an hour unfilled, and both were cancelled. No
/// position ever existed; the realised result was 0R. The only later signal was
/// a `close-failed` on a reversal fire, which reads as a *closing* problem
/// rather than an entry one, and the discrepancy stood for eleven days.
///
/// The manual CLI path learned the same lesson in v135
/// (`BUG-market-entry-no-broker-confirmation-trail.md`): a terse, overstated
/// line had an operator believe a position existed for nine days. The rule
/// distilled from it — **never let the log claim more than the broker
/// confirmed** — is what this encodes for the engine path.
///
/// # Why the ORDER TYPE, and not a fill price from the broker
///
/// The type is the honest discriminator available at this moment, and it is
/// exact: [`ResolvedEntry`] is the same value handed to `place_entry`, so the
/// verb cannot drift from the order actually sent.
///
/// A fill price is *not* available here, on either adapter, and inventing one
/// would recreate the very overstatement this fixes:
///
/// * [`Placement::price`](crate::broker::Placement::price) is documented as the
///   **requested** rate — the market reference, or the stop/limit trigger — and
///   never the fill.
/// * The OANDA adapter returns the pre-computed `reference_price` for all three
///   order types (`broker-oanda/src/oanda.rs`), and its order id is the
///   *submission* transaction; a fill is a separate later transaction.
/// * The TradeNation adapter reports `price: None` for a market entry outright,
///   since TN fills at its own live bid/ask.
///
/// So the outcome states what is known — an order of a given type reached the
/// book — and leaves the fill to be confirmed by the paths that can actually
/// observe one (`lookup_attempt_state`, `plan timeline`).
///
/// # Consumers
///
/// `journal`'s `derive_entry_ts` / `derive_outcome` key on the leading verb to
/// decide whether a plan ever entered and whether to colour the result as a
/// success. Feeding them `placed` for a resting order is the point: a rested,
/// never-filled attempt stops counting as a fill.
fn placement_verb(entry: &crate::intent::ResolvedEntry) -> &'static str {
    match entry {
        // Filled on receipt — a position exists by the time we log.
        crate::intent::ResolvedEntry::Market { .. } => "entered",
        // Resting on the book at a trigger. May never fill.
        crate::intent::ResolvedEntry::Stop { .. } | crate::intent::ResolvedEntry::Limit { .. } => {
            "placed"
        }
    }
}

/// Park an entry the spread floor wouldn't let us place, so a later candle can
/// promote it instead of the setup being lost.
///
/// Returns a short suffix for the reject `outcome` line — the offline replay
/// surfaces `outcome` verbatim, so the operator can see at a glance whether the
/// setup was parked or genuinely gone.
///
/// **Best-effort.** A store failure costs the park, never the (already-decided)
/// rejection, so it is logged and swallowed — exactly the discipline
/// `put_order_body` uses on the placement path above.
///
/// Requires the signed body: without it there is nothing to re-drive later, so
/// a park would be a promise we couldn't keep.
#[allow(clippy::too_many_arguments)]
async fn park_stored_entry<S: StateStore>(
    store: &S,
    verified: &incoming::Verified,
    reason: crate::order_control::StoredReason,
    trade_id: &str,
    instrument: &str,
    original_sl_distance: f64,
    tp_distance: f64,
    min_r: f64,
    raw_body: Option<&str>,
    enter_granularity: Option<crate::broker::Granularity>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    // Prefer the exact signed bytes; fall back to re-serialising the verified
    // intent. The engine path (and the offline replay) build `Verified` from a
    // registered plan rather than from signed YAML, so they have no `raw_body` —
    // but the intent in hand is the same one that was just verified, so
    // re-serialising it loses nothing a promotion needs. Without this fallback
    // the park would silently never happen on the plan-driven path, which is
    // every trade the engine fires.
    let body = match raw_body {
        Some(b) => b.to_string(),
        None => match serde_yaml::to_string(&verified.intent) {
            Ok(yaml) => yaml,
            Err(err) => {
                tracing::error!(
                    "stored-order: cannot serialise intent for trade={trade_id}: {err} — not \
                     parking (nothing to re-drive later)",
                );
                return String::new();
            }
        },
    };
    // Three bars before the alert window closes: promoting into the last moments
    // leaves no runway for the thesis to play out.
    // No plan granularity in hand (webhook / restore re-drive) → assume H1, the
    // most common timeframe. A wrong guess only shifts the drop deadline by a
    // few bars; it never places or blocks a trade on its own.
    let bar_seconds = enter_granularity.map_or(3600, crate::broker::Granularity::seconds);
    let drop_at = crate::order_control::drop_at(verified.intent.not_after, bar_seconds, now);
    let order = crate::order_control::StoredOrder {
        signed_intent: body,
        reason,
        original_sl_distance,
        // The R numerator + threshold the promotion re-check re-tests every
        // candle. Captured here because the cron has no intent in hand, and
        // because `min_r` may be a per-trade override rather than the floor.
        tp_distance,
        min_r,
        stored_at: now,
        drop_at,
        shell_time: verified.shell.time,
        // The clock a per-bar re-check counts in. `enter_granularity` is None on
        // the webhook path, and a size park then keeps waiting rather than
        // promoting on a guessed cadence — the `bar_seconds` docs spell out why
        // that is the safe direction.
        bar_seconds: enter_granularity.map(crate::broker::Granularity::seconds),
    };
    match crate::order_control::park_order(
        store,
        trade_id,
        instrument,
        verified.intent.account.as_deref(),
        order,
        verified.intent.not_after,
        now,
    )
    .await
    {
        Ok(()) => {
            tracing::info!(
                "stored-order: PARKED trade={trade_id} instrument={instrument} \
                 original_sl_distance={original_sl_distance} drop_at={drop_at} — will be \
                 re-checked each candle and placed when the spread allows",
            );
            format!(" stored until {drop_at}")
        }
        Err(err) => {
            tracing::error!(
                "stored-order: park FAILED for trade={trade_id}: {err} — setup is lost as before",
            );
            String::new()
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
/// `skip_reason` is an **out-parameter**: on any path that declines to
/// recover it is set to the short `recover-entry-*` token naming why, so
/// the caller can record it in the ledger outcome. It stays `None` when a
/// recovery is actually attempted (the outcome then describes the
/// re-placement, not a skip).
///
/// It is an out-param rather than a richer return type because the error
/// channel is [`EntryError`] — shared with both broker crates (one a
/// separate repo) and matched exhaustively in 27 places. Widening it to
/// carry a reason would ripple through all of that for one string; the
/// reason is dispatcher-local telemetry, not a broker concept.
async fn place_entry_too_close_fallback<B: Broker>(
    broker: &B,
    resolved: &crate::intent::Resolved,
    intent_id: &str,
    max_risk_pct: f64,
    max_open_positions: u32,
    skip_reason: &mut Option<&'static str>,
) -> Result<crate::broker::Placement, EntryError> {
    use crate::intent::ResolvedEntry;

    // Only stop entries carry the fallback; a too-close on anything else
    // (shouldn't happen) is terminal.
    let trigger_price = match &resolved.entry {
        ResolvedEntry::Stop { trigger_price } => *trigger_price,
        _ => {
            // Not a shape we can recover. Named distinctly from the
            // policy skips below: this is "the fallback does not apply
            // to this entry type", not "the policy declined".
            *skip_reason = Some("recover-entry-not-a-stop-entry");
            return Err(EntryError::EntryTooCloseToMarket);
        }
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
            *skip_reason = Some("recover-entry-price-read-failed");
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
            // Carry the reason to the ledger, not just the log: a
            // `recover-entry-slippage` (the guard correctly refused a
            // runaway chase) and a `recover-entry-price-unavailable` (we
            // bailed blind) are very different post-mortems and used to
            // be indistinguishable downstream.
            *skip_reason = Some(reason);
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
                // Same trade, same contract: a recovery re-placement must size
                // on the SAME multiplier as the original, or the recovered
                // position is a different size than the one authorised.
                contract_multiplier: resolved.contract_multiplier,
            };
            match broker
                .place_entry(max_risk_pct, max_open_positions, &market_request)
                .await
            {
                Ok(placed) => {
                    tracing::info!(
                        "too-close fallback: market re-place succeeded (id={intent_id} order={})",
                        placed.order_id
                    );
                    Ok(placed)
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
                // Same trade, same contract: a recovery re-placement must size
                // on the SAME multiplier as the original, or the recovered
                // position is a different size than the one authorised.
                contract_multiplier: resolved.contract_multiplier,
            };
            match broker
                .place_entry(max_risk_pct, max_open_positions, &limit_request)
                .await
            {
                Ok(placed) => {
                    tracing::info!(
                        "too-close fallback: limit re-place succeeded (id={intent_id} order={})",
                        placed.order_id
                    );
                    Ok(placed)
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
        recover_entry::RecoverEntryPlan::Stop { trigger_price } => {
            tracing::info!(
                "too-close fallback: re-placing as STOP at original trigger (id={intent_id} trigger={trigger_price} price={current_price})"
            );
            // Mirror of the Limit arm: the entry reference (trigger) is
            // unchanged, so sizing is identical. A stop at the original level
            // catches the continuation through it (used when a *limit* was
            // wrong-side).
            let stop_request = EntryRequest {
                instrument: &resolved.instrument,
                direction: resolved.direction,
                entry: ResolvedEntry::Stop { trigger_price },
                stop_loss: resolved.stop_loss,
                take_profit: resolved.take_profit,
                risk: resolved.risk,
                dry_run: resolved.dry_run,
                // Same trade, same contract: a recovery re-placement must size
                // on the SAME multiplier as the original, or the recovered
                // position is a different size than the one authorised.
                contract_multiplier: resolved.contract_multiplier,
            };
            match broker
                .place_entry(max_risk_pct, max_open_positions, &stop_request)
                .await
            {
                Ok(placed) => {
                    tracing::info!(
                        "too-close fallback: stop re-place succeeded (id={intent_id} order={})",
                        placed.order_id
                    );
                    Ok(placed)
                }
                Err(err) => {
                    tracing::error!(
                        "too-close fallback: stop re-place failed: {err} (id={intent_id})"
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
    // Count-back in BARS OF MARKET, not hours of wall-clock. A fixed `+2` bars of
    // slack cannot clear a weekend: a Monday fire looking back 7h for 5 H1 bars
    // reaches into the Friday-close..Sunday-open hole and gets two or three, so
    // the floor is sized off a sample that is both too small (one spike dominates
    // it) and discontinuous. Over-fetching is free — `trailing_spread_mean` keeps
    // only the tail — so the padding is deliberately generous.
    let lookback_bars = crate::order_control::lookback_bars(window, granularity.seconds());
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
mod gate_order_tests {
    use super::*;
    use crate::broker::{
        AmendError, AttemptState, CancelError, Candle, CandleError, EntryError, EntryRequest,
        Granularity, LookupError, OpenPosition, PendingOrder, Placement, Quote,
    };
    use crate::dispatch_config::DispatchConfig;
    use crate::intent::{Direction, Intent, Shell};
    use crate::state::{EntryAttempt, MemStateStore, PrepStamp, StateStore};
    use chrono::{DateTime, Utc};
    use std::cell::RefCell;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid test timestamp")
            .with_timezone(&Utc)
    }

    /// Records the broker traffic the entry path produced. The assertions in
    /// this module are all about *which* calls reached the broker and in what
    /// order — a cancel with no matching place is the bug.
    pub(super) struct SpyBroker {
        cancels: RefCell<Vec<String>>,
        places: RefCell<Vec<String>>,
        /// What `lookup_attempt_state` reports for a prior attempt. `Pending`
        /// is the state that makes the retry gate cancel.
        attempt_state: AttemptState,
    }

    impl SpyBroker {
        /// A broker holding one still-resting prior order — the state that
        /// makes the retry gate issue a cancel.
        fn pending_prior() -> Self {
            Self {
                cancels: RefCell::new(Vec::new()),
                places: RefCell::new(Vec::new()),
                attempt_state: AttemptState::Pending,
            }
        }
        /// A broker with no prior attempt to resolve. The Stage 6 wording
        /// tests fire single-shot enters, so the retry gate is skipped and
        /// `attempt_state` is never consulted — it is set to a terminal,
        /// non-blocking value rather than `Pending` so that if a future change
        /// DID route these through the gate, the test would still exercise the
        /// placement path rather than silently start cancelling.
        pub(super) fn no_prior() -> Self {
            Self {
                cancels: RefCell::new(Vec::new()),
                places: RefCell::new(Vec::new()),
                attempt_state: AttemptState::Cancelled,
            }
        }
        fn cancelled(&self) -> Vec<String> {
            self.cancels.borrow().clone()
        }
        fn placed(&self) -> Vec<String> {
            self.places.borrow().clone()
        }
    }

    impl Broker for SpyBroker {
        async fn place_entry(
            &self,
            _max_risk_pct: f64,
            _max_open_positions: u32,
            req: &EntryRequest<'_>,
        ) -> Result<Placement, EntryError> {
            self.places.borrow_mut().push(req.instrument.to_string());
            Ok(Placement::id_only("order-new"))
        }
        async fn close_positions(&self, _instrument: &str) -> crate::broker::CloseOutcome {
            crate::broker::CloseOutcome::NothingOpen
        }
        async fn cancel_pending_for_instrument(&self, _instrument: &str) -> usize {
            0
        }
        async fn lookup_attempt_state(
            &self,
            _instrument: &str,
            _broker_order_id: &str,
            _broker_trade_id: Option<&str>,
        ) -> Result<AttemptState, LookupError> {
            Ok(self.attempt_state.clone())
        }
        async fn cancel_order(
            &self,
            _account_id: &str,
            broker_order_id: &str,
        ) -> Result<(), CancelError> {
            self.cancels.borrow_mut().push(broker_order_id.to_string());
            Ok(())
        }
        async fn get_quote(&self, _instrument: &str) -> Result<Quote, LookupError> {
            // A tight spread, so the SL-spread floor neither widens nor rejects
            // and the tests stay about gate ORDER.
            Ok(Quote {
                bid: 1.10000,
                ask: 1.10002,
            })
        }
        async fn list_open_positions(
            &self,
            _account_id: &str,
        ) -> Result<Vec<OpenPosition>, LookupError> {
            Ok(vec![])
        }
        async fn amend_stop(
            &self,
            _account_id: &str,
            _position_or_order_id: &str,
            _new_stop: f64,
        ) -> Result<(), AmendError> {
            Ok(())
        }
        async fn list_pending_orders(
            &self,
            _account_id: &str,
        ) -> Result<Vec<PendingOrder>, LookupError> {
            Ok(vec![])
        }
        async fn get_candles(
            &self,
            _instrument: &str,
            _granularity: Granularity,
            _since: DateTime<Utc>,
            _now: DateTime<Utc>,
        ) -> Result<Vec<Candle>, CandleError> {
            Ok(vec![])
        }
    }

    pub(super) fn cfg() -> DispatchConfig {
        DispatchConfig {
            worker_max_risk_pct: 1.0,
            worker_max_open_positions: 3,
            pip_size: 0.0001,
            tick_size: None,
            caps: Default::default(),
        }
    }

    /// A multi-shot (`max_retries: 2`) long enter for trade `t-1` requiring the
    /// `retest` prep — the shape of the incident's `05-enter`.
    ///
    /// Built by DESERIALISING the wire JSON rather than by struct literal, so a
    /// test can't quietly diverge from what a signed alert actually carries.
    fn enter_verified(vetos: &str, requires_preps: &str) -> crate::incoming::Verified {
        enter_verified_full(
            vetos,
            requires_preps,
            r#"{ "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.5900 }"#,
        )
    }

    /// The same enter with a caller-chosen `entry` block, so a test can vary
    /// the ORDER TYPE (market / stop / limit) — the axis Stage 6 turns on —
    /// without a second copy of the wire JSON drifting from this one.
    ///
    /// Single-shot (`max_retries` is left at its default `0`): these tests are
    /// about the outcome WORDING on a plain first placement, so the retry gate
    /// and its prior-attempt lookup stay out of the picture entirely.
    pub(super) fn enter_verified_with_entry(entry: &str) -> crate::incoming::Verified {
        let mut v = enter_verified_full("[]", "[]", entry);
        v.intent.max_retries = crate::tunable::Tunable::Static(0);
        v
    }

    fn enter_verified_full(
        vetos: &str,
        requires_preps: &str,
        entry: &str,
    ) -> crate::incoming::Verified {
        let json = format!(
            r#"{{
                "v": 1,
                "id": "t-1-enter",
                "not_after": "2026-08-09T00:00:00Z",
                "action": "enter",
                "instrument": "EUR_CAD",
                "direction": "long",
                "entry": {entry},
                "stop_loss": {{ "absolute": 1.5850 }},
                "take_profit": {{ "absolute": 1.6100 }},
                "broker": "oanda",
                "trade_id": "t-1",
                "pip_size": 0.0001,
                "max_retries": 2,
                "vetos": {vetos},
                "requires_preps": {requires_preps}
            }}"#
        );
        let intent: Intent = serde_json::from_str(&json).expect("valid multi-shot enter intent");
        let shell = Shell::from_candle(&Candle {
            time: at("2026-08-07T17:00:00Z"),
            o: 1.5880,
            h: 1.5905,
            l: 1.5875,
            c: 1.5895,
        });
        crate::incoming::Verified { shell, intent }
    }

    /// The prior attempt whose resting order the retry gate would cancel —
    /// order `2318` in the incident.
    fn prior_attempt() -> EntryAttempt {
        EntryAttempt {
            trade_id: "t-1".into(),
            account: None,
            instrument: "EUR_CAD".into(),
            attempt_no: 1,
            broker_order_id: "2318".into(),
            broker_trade_id: None,
            direction: Direction::Long,
            placed_at: at("2026-08-07T16:00:41Z"),
            shell_time: at("2026-08-07T16:00:00Z"),
            expires_at: at("2026-08-09T01:00:00Z"),
            stop_loss_price: Some(1.5850),
            cancel_at: None,
            pip_size: Some(0.0001),
            blackout_close: Default::default(),
            breakeven: None,
            order_control: None,
        }
    }

    /// A store whose clock is pinned to the incident's timeline. Without this
    /// the store judges TTLs against real wall-clock, so a prep/veto stamped in
    /// 2026-08 is already expired and the gate under test never runs.
    pub(super) fn store_at_incident() -> MemStateStore {
        let store = MemStateStore::new();
        store.set_clock(now());
        store
    }

    async fn seed_prior_attempt(store: &MemStateStore) {
        store
            .record_entry_attempt(prior_attempt())
            .await
            .expect("record prior attempt");
    }

    pub(super) fn now() -> DateTime<Utc> {
        at("2026-08-07T17:00:01Z")
    }

    /// A day, so state stamped hours before the fire is still live at it. A
    /// short TTL would silently expire the prep/veto and the gate under test
    /// would never run — a green-for-the-wrong-reason trap.
    const TTL: u64 = 86_400;

    /// `ActionResult` deliberately carries no `Debug` (it is the dispatch
    /// outcome carrier, not a diagnostic type), so render it here rather than
    /// widening the public type just for these assertions.
    pub(super) fn describe(r: &ActionResult) -> String {
        match r {
            ActionResult::Ok(o) => format!("Ok({o})"),
            ActionResult::Failed(o) => format!("Failed({o})"),
            ActionResult::Rejected {
                status, outcome, ..
            } => {
                format!("Rejected({status}, {outcome})")
            }
        }
    }

    /// THE INCIDENT. A fire whose prep chain is unsatisfiable must not cost the
    /// resting order: the prep gate rejects, and because it now runs ahead of
    /// the retry gate, `cancel_order` is never reached.
    #[test]
    fn prep_reject_leaves_the_prior_resting_order_untouched() {
        let broker = SpyBroker::pending_prior();
        let store = store_at_incident();
        let verified = enter_verified("[]", r#"["break-and-close", "retest"]"#);
        pollster::block_on(async {
            seed_prior_attempt(&store).await;
            // `break-and-close` is set but `retest` never was → `Missing`, the
            // same slot failure the incident logged as `prep-order-violated`.
            store
                .set_prep(
                    None,
                    "EUR_CAD",
                    "break-and-close",
                    PrepStamp::at(now()),
                    TTL,
                    "test",
                )
                .await
                .expect("set prep");
            let result =
                run_enter(&broker, &store, &verified, &cfg(), now(), None, None, false).await;
            assert!(
                matches!(&result, ActionResult::Rejected { outcome, .. }
                    if outcome.starts_with("rejected: missing-prep")),
                "expected the prep gate to reject, got {}",
                describe(&result)
            );
        });
        assert!(
            broker.cancelled().is_empty(),
            "RAIL: a rejected fire must not cancel the prior resting order, \
             but cancel_order was called for {:?}",
            broker.cancelled()
        );
        assert!(broker.placed().is_empty(), "a rejected fire places nothing");
    }

    /// Same rail, the ORDER-VIOLATED arm — the incident's literal outcome
    /// (`prep-order-violated (retest)`): both preps are set, but out of order.
    #[test]
    fn prep_out_of_order_reject_leaves_the_prior_resting_order_untouched() {
        let broker = SpyBroker::pending_prior();
        let store = store_at_incident();
        let verified = enter_verified("[]", r#"["break-and-close", "retest"]"#);
        pollster::block_on(async {
            seed_prior_attempt(&store).await;
            // `retest` stamped BEFORE `break-and-close` ⇒ the chain is not
            // strictly increasing ⇒ `prep-order-violated (retest)`.
            store
                .set_prep(
                    None,
                    "EUR_CAD",
                    "retest",
                    PrepStamp::at(at("2026-08-07T15:00:00Z")),
                    TTL,
                    "test",
                )
                .await
                .expect("set retest");
            store
                .set_prep(
                    None,
                    "EUR_CAD",
                    "break-and-close",
                    PrepStamp::at(at("2026-08-07T16:00:00Z")),
                    TTL,
                    "test",
                )
                .await
                .expect("set break-and-close");
            let result =
                run_enter(&broker, &store, &verified, &cfg(), now(), None, None, false).await;
            assert!(
                matches!(&result, ActionResult::Rejected { outcome, .. }
                    if outcome == "rejected: prep-order-violated (retest)"),
                "expected the incident's exact outcome, got {}",
                describe(&result)
            );
        });
        assert!(
            broker.cancelled().is_empty(),
            "RAIL: `prep-order-violated` must not cost the resting order \
             (cancelled {:?})",
            broker.cancelled()
        );
    }

    /// The veto gate — same hazard, same rail.
    #[test]
    fn veto_reject_leaves_the_prior_resting_order_untouched() {
        let broker = SpyBroker::pending_prior();
        let store = store_at_incident();
        let verified = enter_verified(r#"["too-low"]"#, "[]");
        pollster::block_on(async {
            seed_prior_attempt(&store).await;
            store
                .set_veto(None, "t-1", "EUR_CAD", "too-low", TTL)
                .await
                .expect("set veto");
            let result =
                run_enter(&broker, &store, &verified, &cfg(), now(), None, None, false).await;
            assert!(
                matches!(&result, ActionResult::Rejected { outcome, .. }
                    if outcome == "rejected: veto-active (too-low)"),
                "expected the veto gate to reject, got {}",
                describe(&result)
            );
        });
        assert!(
            broker.cancelled().is_empty(),
            "RAIL: an active veto must not cost the resting order (cancelled {:?})",
            broker.cancelled()
        );
    }

    /// The cooldown gate — same hazard, same rail.
    #[test]
    fn cooldown_reject_leaves_the_prior_resting_order_untouched() {
        let broker = SpyBroker::pending_prior();
        let store = store_at_incident();
        let verified = enter_verified("[]", "[]");
        pollster::block_on(async {
            seed_prior_attempt(&store).await;
            store
                .set_cooldown(None, "EUR_CAD", 24, now())
                .await
                .expect("set cooldown");
            let result =
                run_enter(&broker, &store, &verified, &cfg(), now(), None, None, false).await;
            assert!(
                matches!(&result, ActionResult::Rejected { outcome, .. }
                    if outcome == "rejected: cooled-down"),
                "expected the cooldown gate to reject, got {}",
                describe(&result)
            );
        });
        assert!(
            broker.cancelled().is_empty(),
            "RAIL: a cooldown must not cost the resting order (cancelled {:?})",
            broker.cancelled()
        );
    }

    /// The `allow_entry` script gate — a Phase-2 gate, so it needs the resolved
    /// geometry and cannot move above `Resolved::from_intent`. It still must
    /// sit ahead of the retry gate, which is what this pins.
    #[test]
    fn allow_entry_false_leaves_the_prior_resting_order_untouched() {
        let broker = SpyBroker::pending_prior();
        let store = store_at_incident();
        let mut verified = enter_verified("[]", "[]");
        verified.intent.allow_entry = Some(crate::tunable::Tunable::Static(false));
        pollster::block_on(async {
            seed_prior_attempt(&store).await;
            let result =
                run_enter(&broker, &store, &verified, &cfg(), now(), None, None, false).await;
            assert!(
                matches!(&result, ActionResult::Rejected { outcome, .. }
                    if outcome == "rejected: allow-entry-false"),
                "expected allow_entry to block, got {}",
                describe(&result)
            );
        });
        assert!(
            broker.cancelled().is_empty(),
            "RAIL: allow_entry=false must not cost the resting order (cancelled {:?})",
            broker.cancelled()
        );
    }

    /// The entry-level veto gate (Bug #12) — also Phase 2, also ahead of the
    /// retry gate.
    #[test]
    fn entry_level_veto_leaves_the_prior_resting_order_untouched() {
        let broker = SpyBroker::pending_prior();
        let store = store_at_incident();
        let mut verified = enter_verified("[]", "[]");
        // The resolved long entry is 1.5900; a `too-high` cap at 1.5800 is
        // already breached, so the continuous veto rejects.
        verified.intent.entry_level_vetos = vec![crate::intent::EntryLevelVeto {
            name: "too-high".into(),
            level: 1.5800,
            past: crate::intent::VetoSide::Above,
        }];
        pollster::block_on(async {
            seed_prior_attempt(&store).await;
            let result =
                run_enter(&broker, &store, &verified, &cfg(), now(), None, None, false).await;
            assert!(
                matches!(&result, ActionResult::Rejected { outcome, .. }
                    if outcome == "rejected: veto-active (too-high)"),
                "expected the entry-level veto to reject, got {}",
                describe(&result)
            );
        });
        assert!(
            broker.cancelled().is_empty(),
            "RAIL: an entry-level veto must not cost the resting order (cancelled {:?})",
            broker.cancelled()
        );
    }

    /// REGRESSION — the legitimate multi-shot path must keep working. A fire
    /// that clears every gate and finds a prior `Pending` attempt still cancels
    /// it and places a fresh order. This is 2316→2318 in the incident, which was
    /// correct behaviour and must not be broken by the reordering.
    #[test]
    fn all_gates_pass_still_cancels_the_pending_prior_and_places_fresh() {
        let broker = SpyBroker::pending_prior();
        let store = store_at_incident();
        let verified = enter_verified("[]", r#"["break-and-close", "retest"]"#);
        pollster::block_on(async {
            seed_prior_attempt(&store).await;
            // A satisfied, strictly-increasing prep chain.
            store
                .set_prep(
                    None,
                    "EUR_CAD",
                    "break-and-close",
                    PrepStamp::at(at("2026-08-07T15:00:00Z")),
                    TTL,
                    "test",
                )
                .await
                .expect("set break-and-close");
            store
                .set_prep(
                    None,
                    "EUR_CAD",
                    "retest",
                    PrepStamp::at(at("2026-08-07T16:00:00Z")),
                    TTL,
                    "test",
                )
                .await
                .expect("set retest");
            let result =
                run_enter(&broker, &store, &verified, &cfg(), now(), None, None, false).await;
            // `placed:`, not `entered:` — this enter is a STOP order, which
            // rests rather than filling (Stage 6). The assertion here is about
            // a placement having HAPPENED, so it tracks the resting verb.
            assert!(
                matches!(&result, ActionResult::Ok(o) if o.starts_with("placed: order=")),
                "expected a placement, got {}",
                describe(&result)
            );
        });
        assert_eq!(
            broker.cancelled(),
            vec!["2318".to_string()],
            "the superseded resting order is still cancelled when placement follows"
        );
        assert_eq!(
            broker.placed(),
            vec!["EUR_CAD".to_string()],
            "and a fresh order is placed in its stead"
        );
    }

    /// The cancel must be *paired* with a placement, which is only true if the
    /// gate sits last. Asserting on the counts alone (as above) would pass even
    /// if the cancel happened first and the place merely also happened; this
    /// asserts the fire that gets cancelled is the fire that gets placed, by
    /// checking the attempt row the placement recorded.
    #[test]
    fn a_cancel_is_always_followed_by_a_recorded_new_attempt() {
        let broker = SpyBroker::pending_prior();
        let store = store_at_incident();
        let verified = enter_verified("[]", "[]");
        pollster::block_on(async {
            seed_prior_attempt(&store).await;
            run_enter(&broker, &store, &verified, &cfg(), now(), None, None, false).await;
            let attempts = store
                .list_entry_attempts(None, "t-1")
                .await
                .expect("list attempts");
            assert_eq!(
                attempts.len(),
                2,
                "the cancelled attempt #1 is superseded by a recorded attempt #2"
            );
            assert_eq!(attempts[1].broker_order_id, "order-new");
        });
        assert_eq!(broker.cancelled(), vec!["2318".to_string()]);
    }

    /// BUG C, END TO END through the two real entry points. The preps are set
    /// by `handle_prep` (not by poking the store), both under the SINGLE `now`
    /// a multi-bar catch-up tick dispatches every fire with, and then
    /// `run_enter` is asked for the entry.
    ///
    /// Because `handle_prep` now stamps `set_at` from the triggering bar rather
    /// than wall-clock, the chain is strictly increasing and the entry is
    /// PLACED. Under the wall-clock stamp both rows carried an identical
    /// timestamp and this returned `rejected: prep-order-violated (retest)` —
    /// the incident's literal outcome, on perfectly correct geometry.
    #[test]
    fn multi_bar_catch_up_preps_let_the_entry_through() {
        let broker = SpyBroker::pending_prior();
        let store = store_at_incident();
        let verified = enter_verified("[]", r#"["break-and-close", "retest"]"#);
        pollster::block_on(async {
            seed_prior_attempt(&store).await;
            // Two bars, ONE wall-clock `now()` — exactly what the cron does when
            // a tick catches up over more than one closed bar.
            for (step, bar) in [
                ("break-and-close", "2026-08-07T15:00:00Z"),
                ("retest", "2026-08-07T16:00:00Z"),
            ] {
                let result =
                    crate::dispatch::handle_prep(&store, &prep_verified(step, bar), now()).await;
                assert!(result.is_success(), "prep {step}: {}", result.body);
            }
            let result =
                run_enter(&broker, &store, &verified, &cfg(), now(), None, None, false).await;
            // Again `placed:` — a resting stop entry (Stage 6).
            assert!(
                matches!(&result, ActionResult::Ok(o) if o.starts_with("placed: order=")),
                "BUG C: a two-bar prep chain caught up in one tick must place, \
                 got {}",
                describe(&result)
            );
        });
        assert_eq!(
            broker.placed(),
            vec!["EUR_CAD".to_string()],
            "the entry the incident lost must actually reach the broker"
        );
    }

    /// The same end-to-end route must keep rejecting a genuinely stale chain:
    /// `retest` on an EARLIER bar than `break-and-close`. Bar-time stamping is a
    /// re-stamp, not a loosened comparison, so the gate keeps its teeth — and
    /// per the Stage 1 rail the rejected fire still costs no resting order.
    #[test]
    fn genuinely_stale_prep_chain_still_rejects_end_to_end() {
        let broker = SpyBroker::pending_prior();
        let store = store_at_incident();
        let verified = enter_verified("[]", r#"["break-and-close", "retest"]"#);
        pollster::block_on(async {
            seed_prior_attempt(&store).await;
            for (step, bar) in [
                ("retest", "2026-08-07T15:00:00Z"),
                ("break-and-close", "2026-08-07T16:00:00Z"),
            ] {
                crate::dispatch::handle_prep(&store, &prep_verified(step, bar), now()).await;
            }
            let result =
                run_enter(&broker, &store, &verified, &cfg(), now(), None, None, false).await;
            assert!(
                matches!(&result, ActionResult::Rejected { outcome, .. }
                    if outcome == "rejected: prep-order-violated (retest)"),
                "a genuinely out-of-order chain must still reject, got {}",
                describe(&result)
            );
        });
        assert!(broker.placed().is_empty(), "a rejected fire places nothing");
        assert!(
            broker.cancelled().is_empty(),
            "RAIL: a rejected fire must not cost the resting order (cancelled {:?})",
            broker.cancelled()
        );
    }

    /// A `prep` intent for `step`, triggered by the bar at `bar_time`. Built by
    /// deserialising the wire JSON and synthesising the shell with
    /// [`Shell::from_candle`], exactly as the cron's `dispatch_fired` does.
    fn prep_verified(step: &str, bar_time: &str) -> crate::incoming::Verified {
        let json = format!(
            r#"{{
                "v": 1,
                "id": "t-1-prep-{step}",
                "not_after": "2026-08-09T00:00:00Z",
                "action": "prep",
                "instrument": "EUR_CAD",
                "step": "{step}",
                "trade_id": "t-1",
                "ttl_hours": 24
            }}"#
        );
        let intent: Intent = serde_json::from_str(&json).expect("valid prep intent");
        let shell = Shell::from_candle(&Candle {
            time: at(bar_time),
            o: 1.5880,
            h: 1.5905,
            l: 1.5875,
            c: 1.5895,
        });
        crate::incoming::Verified { shell, intent }
    }
}

/// Stage 6 — **a placement is not a fill.**
///
/// `run_enter` used to report `entered: order=<id>` the moment `place_entry`
/// returned, for every order type. For a **market** order that is honest: the
/// broker fills it there and then. For a **stop** or **limit** order it is not —
/// the order merely *rests*, and may sit unfilled for hours before being
/// cancelled, having never opened a position at all.
///
/// The two cases produced a byte-identical log line, so nothing downstream could
/// tell them apart. That is how the incident of 2026-08-07 (OANDA
/// `101-011-31142393-003`, plan `hs-eur-cad-08ca0693`) reached the operator's
/// trade journal as a **WIN +2.63R**: two limit orders (2316, 2318) were placed,
/// each logged `entered:`, each rested ~1h unfilled, and each was cancelled.
/// No position ever existed. The only later signal was `close-failed`, which
/// reads as a *closing* problem rather than an entry one, and the discrepancy
/// went unnoticed for eleven days.
///
/// The same class of overstatement was fixed for the manual CLI path in v135
/// (`BUG-market-entry-no-broker-confirmation-trail.md`), where a terse
/// wrong-sounding line led an operator to believe a position existed for nine
/// days. This is the engine path's half of that lesson.
///
/// These tests assert at the `run_enter` entry point rather than on the helper
/// below it — a guard one level down would mask a survivor
/// (`[[mutation_test_the_entry_point_not_just_the_layer_below]]`).
#[cfg(test)]
mod placement_is_not_a_fill_tests {
    use super::gate_order_tests::{
        SpyBroker, cfg, describe, enter_verified_with_entry, now, store_at_incident,
    };
    use super::*;

    /// A **stop** entry rests at the broker. It has not filled, so the outcome
    /// must not claim it did.
    #[test]
    fn a_resting_stop_order_is_placed_not_entered() {
        let broker = SpyBroker::no_prior();
        let store = store_at_incident();
        let verified = enter_verified_with_entry(
            r#"{ "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.5900 }"#,
        );
        let result = pollster::block_on(run_enter(
            &broker,
            &store,
            &verified,
            &cfg(),
            now(),
            None,
            None,
            false,
        ));
        let outcome = match &result {
            ActionResult::Ok(o) => o.clone(),
            other => panic!("expected a placement, got {}", describe(other)),
        };
        assert!(
            outcome.starts_with("placed: order="),
            "a resting stop must report `placed:`, got {outcome}"
        );
        assert!(
            !outcome.contains("entered"),
            "a resting stop must not claim it entered, got {outcome}"
        );
    }

    /// A **limit** entry rests too — this is the incident's own order type
    /// (2316 and 2318 were both `LIMIT_ORDER`, `opened_trade_id: null`).
    #[test]
    fn a_resting_limit_order_is_placed_not_entered() {
        let broker = SpyBroker::no_prior();
        let store = store_at_incident();
        let verified = enter_verified_with_entry(
            r#"{ "type": "limit", "from": "close", "offset_pips": 0.0, "at": 1.5900 }"#,
        );
        let result = pollster::block_on(run_enter(
            &broker,
            &store,
            &verified,
            &cfg(),
            now(),
            None,
            None,
            false,
        ));
        let outcome = match &result {
            ActionResult::Ok(o) => o.clone(),
            other => panic!("expected a placement, got {}", describe(other)),
        };
        assert!(
            outcome.starts_with("placed: order="),
            "a resting limit must report `placed:`, got {outcome}"
        );
        assert!(
            !outcome.contains("entered"),
            "THE INCIDENT: a resting limit must not read like a fill, got {outcome}"
        );
    }

    /// A **market** entry fills immediately, so `entered:` is the honest word.
    /// This is the other half of the discrimination: the change must not simply
    /// rename every placement.
    #[test]
    fn a_market_order_is_entered_not_merely_placed() {
        let broker = SpyBroker::no_prior();
        let store = store_at_incident();
        let verified = enter_verified_with_entry(
            r#"{ "type": "market", "from": "close", "offset_pips": 0.0 }"#,
        );
        let result = pollster::block_on(run_enter(
            &broker,
            &store,
            &verified,
            &cfg(),
            now(),
            None,
            None,
            false,
        ));
        let outcome = match &result {
            ActionResult::Ok(o) => o.clone(),
            other => panic!("expected a placement, got {}", describe(other)),
        };
        assert!(
            outcome.starts_with("entered: order="),
            "a market order does fill, so it keeps `entered:`, got {outcome}"
        );
    }

    /// The order id is still carried in both wordings — every downstream
    /// consumer (`plan timeline`, the journal, the broker-evidence pull) joins
    /// on it, so a rename must not drop it.
    #[test]
    fn both_wordings_still_carry_the_broker_order_id() {
        let store = store_at_incident();
        for entry in [
            r#"{ "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.5900 }"#,
            r#"{ "type": "market", "from": "close", "offset_pips": 0.0 }"#,
        ] {
            let broker = SpyBroker::no_prior();
            let verified = enter_verified_with_entry(entry);
            let result = pollster::block_on(run_enter(
                &broker,
                &store,
                &verified,
                &cfg(),
                now(),
                None,
                None,
                false,
            ));
            let outcome = match &result {
                ActionResult::Ok(o) => o.clone(),
                other => panic!("expected a placement for {entry}, got {}", describe(other)),
            };
            assert!(
                outcome.contains("order=order-new"),
                "the broker order id must survive the rename, got {outcome}"
            );
        }
    }
}

#[cfg(test)]
mod units_below_minimum_tests {
    //! Pins the [`EntryError::UnitsBelowMinimum`] arm of [`run_enter`].
    //!
    //! Before this, the error fell through to the generic `Err(err)` arm and
    //! became `ActionResult::Failed` — a `SeenDecision::Skip`, so the identical
    //! fire re-ran every bar, failed identically, and produced no operator
    //! signal and no termination. Position size is a deterministic function of
    //! (equity, stop distance, contract multiplier), so nothing about that retry
    //! could ever succeed. Futures make it acute: one contract is 100%
    //! granularity.

    use super::*;
    use crate::broker::{
        AttemptState, CancelError, Candle, CloseOutcome, EntryRequest, Granularity, LookupError,
        OpenPosition, PendingOrder, Placement, Quote,
    };
    use crate::dispatch_config::DispatchConfig;
    use crate::order_control::{StoredReason, stored_order};
    use crate::state::MemStateStore;
    use chrono::{DateTime, Utc};

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid test timestamp")
            .with_timezone(&Utc)
    }

    /// Returns whichever entry error the test asked for, so the arm under test
    /// is selected by the broker's verdict exactly as it is live.
    struct FailingBroker(fn() -> EntryError);

    impl Broker for FailingBroker {
        async fn place_entry(
            &self,
            _max_risk_pct: f64,
            _max_open_positions: u32,
            _req: &EntryRequest<'_>,
        ) -> Result<Placement, EntryError> {
            Err((self.0)())
        }
        async fn close_positions(&self, _instrument: &str) -> CloseOutcome {
            CloseOutcome::NothingOpen
        }
        async fn cancel_pending_for_instrument(&self, _instrument: &str) -> usize {
            0
        }
        async fn lookup_attempt_state(
            &self,
            _instrument: &str,
            _broker_order_id: &str,
            _broker_trade_id: Option<&str>,
        ) -> Result<AttemptState, LookupError> {
            Ok(AttemptState::Unknown)
        }
        async fn cancel_order(
            &self,
            _account_id: &str,
            _broker_order_id: &str,
        ) -> Result<(), CancelError> {
            Ok(())
        }
        async fn get_quote(&self, _instrument: &str) -> Result<Quote, LookupError> {
            // A tight spread, so the SL-spread floor never fires and the
            // dispatch reaches the broker — the arm under test.
            Ok(Quote {
                bid: 1.0999,
                ask: 1.1001,
            })
        }
        async fn list_open_positions(
            &self,
            _account_id: &str,
        ) -> Result<Vec<OpenPosition>, LookupError> {
            Ok(vec![])
        }
        async fn amend_stop(
            &self,
            _account_id: &str,
            _position_or_order_id: &str,
            _new_stop: f64,
        ) -> Result<(), crate::broker::AmendError> {
            Ok(())
        }
        async fn list_pending_orders(
            &self,
            _account_id: &str,
        ) -> Result<Vec<PendingOrder>, LookupError> {
            Ok(vec![])
        }
        async fn get_candles(
            &self,
            _instrument: &str,
            _granularity: Granularity,
            _since: DateTime<Utc>,
            _now: DateTime<Utc>,
        ) -> Result<Vec<Candle>, crate::broker::CandleError> {
            Ok(vec![])
        }
    }

    fn cfg() -> DispatchConfig {
        DispatchConfig {
            worker_max_risk_pct: 100.0,
            worker_max_open_positions: 100,
            pip_size: 0.0001,
            tick_size: None,
            caps: Default::default(),
        }
    }

    /// A market enter for `t-1`, so the entry resolves with no pending-order
    /// machinery in the way.
    fn enter_verified() -> incoming::Verified {
        use crate::intent::Shell;
        let intent: crate::intent::Intent = serde_json::from_str(
            r#"{
                "v": 1,
                "id": "t-1-enter",
                "not_after": "2026-07-24T00:00:00Z",
                "action": "enter",
                "instrument": "EUR_USD",
                "direction": "long",
                "entry": { "type": "market" },
                "stop_loss": { "absolute": 1.0980 },
                "take_profit": { "absolute": 1.1200 },
                "broker": "oanda",
                "trade_id": "t-1",
                "pip_size": 0.0001
            }"#,
        )
        .expect("valid enter intent");
        let shell = Shell::from_candle(&Candle {
            time: at("2026-07-22T13:00:00Z"),
            o: 1.0990,
            h: 1.1005,
            l: 1.0985,
            c: 1.1000,
        });
        incoming::Verified { shell, intent }
    }

    fn dispatch(err: fn() -> EntryError, store: &MemStateStore) -> ActionResult {
        pollster::block_on(run_enter(
            &FailingBroker(err),
            store,
            &enter_verified(),
            &cfg(),
            at("2026-07-22T13:00:30Z"),
            None,
            Some(Granularity::H1),
            false,
        ))
    }

    /// The fix: the setup is PARKED, not thrown away, and the outcome says so.
    ///
    /// Mutation check: delete the `UnitsBelowMinimum` arm so it falls through to
    /// the generic `Failed` arm, and this goes red.
    #[test]
    fn units_below_minimum_parks_the_setup() {
        let store = MemStateStore::default();
        let out = dispatch(|| EntryError::UnitsBelowMinimum, &store);

        let ActionResult::Rejected {
            status, outcome, ..
        } = &out
        else {
            panic!("expected a rejection that parks, got {}", out.describe());
        };
        assert_eq!(*status, 422, "well-formed request, healthy broker");
        assert!(
            outcome.starts_with("rejected: units-below-minimum"),
            "the operator greps this string: {outcome}",
        );
        assert!(
            outcome.contains("stored until"),
            "a park that left no trace is the bug, not the fix: {outcome}",
        );

        let parked = pollster::block_on(stored_order(&store, "t-1"))
            .expect("read")
            .expect("the setup must be parked, not discarded");
        assert_eq!(parked.reason, StoredReason::BelowMinSize);
        assert!(
            parked.reason.rechecked_per_bar(),
            "a size park must be re-checked per BAR — the order-control loop \
             ticks faster than a bar, so the spread gate would retry it in seconds",
        );
        assert_eq!(
            parked.bar_seconds,
            Some(3600),
            "the enter granularity must reach the park, or it can never promote",
        );
    }

    /// The neighbouring arm must be untouched: a too-close rejection stays a
    /// plain `Failed` and parks nothing. It is genuinely retryable next bar —
    /// price moves — so parking it would be wrong.
    ///
    /// Mutation check: park on `EntryTooCloseToMarket` too, and this goes red.
    #[test]
    fn entry_too_close_to_market_still_plain_fails_and_parks_nothing() {
        let store = MemStateStore::default();
        let out = dispatch(|| EntryError::EntryTooCloseToMarket, &store);

        assert!(
            matches!(out, ActionResult::Failed(ref o) if o.contains("too-close-to-market")),
            "expected the unchanged Failed arm, got {}",
            out.describe(),
        );
        assert!(
            pollster::block_on(stored_order(&store, "t-1"))
                .expect("read")
                .is_none(),
            "a too-close rejection must not park",
        );
    }

    /// Every other broker failure keeps the generic `Failed` arm too — the park
    /// is scoped to the one error that cannot change within a bar.
    #[test]
    fn an_unrelated_broker_failure_still_plain_fails() {
        let store = MemStateStore::default();
        let out = dispatch(|| EntryError::OrderRejected, &store);
        assert!(
            matches!(out, ActionResult::Failed(_)),
            "got {}",
            out.describe()
        );
        assert!(
            pollster::block_on(stored_order(&store, "t-1"))
                .expect("read")
                .is_none(),
        );
    }

    /// Both arms are a `Skip`, so NEITHER poisons the seen-id and the next bar
    /// may fire again. That is unchanged by this stage and load-bearing: the
    /// park adds a retry *cadence* and an audit trail, it does not change
    /// replay protection. Pinned so a future "tidy-up" doesn't mark the
    /// rejection seen and strand the setup.
    /// END-TO-END: the recovery skip reason must survive all the way into
    /// the recorded outcome, through `run_enter` — not merely be
    /// renderable by the pure helper.
    ///
    /// This exists because a pure-layer test does NOT catch the real
    /// regression: `outcome_for_entry_failure(&err, None)` at the
    /// dispatcher (discarding the reason, i.e. the original bug) leaves
    /// every `recover_entry.rs` unit test green. Mutating the call site
    /// must turn something red, and this is that something.
    ///
    /// Setup: a long stop at 1.5900 against a 1.1000 market. The broker
    /// rejects with `#19-10`, the `limit` policy is consulted, and the
    /// wrong-side guard declines (a long limit at 1.5900 sits far above
    /// market — a `#19-9` waiting to happen). The ledger must say which
    /// guard refused, not just that the entry failed.
    #[test]
    fn recovery_skip_reason_reaches_the_recorded_outcome() {
        let store = MemStateStore::default();
        let verified = super::gate_order_tests::enter_verified_with_entry(
            r#"{ "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.5900,
                 "recover_entry": { "action": "limit" } }"#,
        );
        let out = pollster::block_on(run_enter(
            &FailingBroker(|| EntryError::EntryTooCloseToMarket),
            &store,
            &verified,
            &cfg(),
            at("2026-07-22T13:00:30Z"),
            None,
            Some(Granularity::H1),
            false,
        ));

        let ActionResult::Failed(outcome) = &out else {
            panic!("a #19-10 must stay Failed, got {}", out.describe());
        };
        assert!(
            outcome.contains("too-close-to-market"),
            "must keep the greppable token: {outcome}"
        );
        assert!(
            outcome.contains("recover-entry-limit-wrong-side"),
            "the ledger must name WHY recovery declined, not just that the \
             entry failed — this is the whole point of the change: {outcome}"
        );
    }

    #[test]
    fn parking_does_not_change_seen_id_behaviour() {
        use crate::dispatch::seen::{SeenDecision, seen_decision};
        let store = MemStateStore::default();
        let parked = dispatch(|| EntryError::UnitsBelowMinimum, &store);
        assert!(matches!(seen_decision(&parked), SeenDecision::Skip { .. }));
        let store2 = MemStateStore::default();
        let failed = dispatch(|| EntryError::EntryTooCloseToMarket, &store2);
        assert!(matches!(seen_decision(&failed), SeenDecision::Skip { .. }));
    }
}
