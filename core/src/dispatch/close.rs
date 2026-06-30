//! The `Close` dispatch path (reversal-close, with optional `veto_on_reversal`).

use super::action_result::ActionResult;
use super::shared::record_control_event_for;
use crate::allow_close_gate;
use crate::broker::Broker;
use crate::incoming;
use crate::intent::{Intent, REVERSAL_VETO_NAME};
use crate::state::{StateStore, veto_ttl_seconds};

/// Dispatch a `Close` intent. The close reaches the broker only when
/// **every** layer of gating agrees:
///
/// 1. **Contextual window** (OR-composed) — the close is "at a real
///    reversal point". Up to two windows may be listed; *at least one*
///    must pass.
///      - News window — an active `news:<trade_id>:<news_id>` pair.
///      - Price window — broker's current price sits inside one of
///        `sr_bands`.
///
///    The new wire form is `inside_window: [news, price]` +
///    `sr_bands: [[lo, hi]]`. The deprecated form
///    (`require_news_window` + `require_price_in_ranges`) is still
///    accepted; validation guarantees an intent only carries one form.
/// 2. **Candle quality** (AND-composed) — `needs_golden` and
///    `needs_confirmed` shell-checks. Promoted to typed fields so the
///    consolidated reversal close can require a golden / confirmed
///    candle without dropping into Rhai.
/// 3. **`allow_close` script** (AND-composed) — operator's Tunable<bool>
///    sees the shell-anchor scope (same scope `allow_entry` sees, minus
///    derived geometry — closes don't compute SL/TP).
///
/// Both contextual gates are evaluated even after one passes, so the
/// log line records the full state and the outcome string can name
/// every failed gate when none passes.
pub async fn run_close<B: Broker, S: StateStore>(
    broker: &B,
    store: &S,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> ActionResult {
    // News window. Old form: `require_news_window: Some(true)`. New
    // form: `inside_window` contains `News`. Mutual exclusion is
    // enforced at validate time, so at most one branch fires.
    let want_news = verified.intent.require_news_window == Some(true)
        || verified
            .intent
            .inside_window
            .contains(&crate::intent::EventWindow::News);
    let news_outcome = if want_news {
        let Some(tid) = verified.intent.trade_id.as_deref() else {
            return ActionResult::Rejected {
                status: 400,
                body: "close with news-window gate requires `trade_id`".to_string(),
                outcome: "rejected: missing-trade-id".into(),
            };
        };
        match store.list_news_windows_for_trade(tid).await {
            Ok(windows) if windows.is_empty() => GateOutcome::Failed("no-news-window"),
            Ok(windows) => {
                let names: Vec<String> = windows
                    .iter()
                    .map(|w| match &w.reason {
                        Some(r) => format!("{}({r})", w.news_id),
                        None => w.news_id.clone(),
                    })
                    .collect();
                tracing::info!(
                    "close news-window gate passed: trade {tid} active=[{}]",
                    names.join(", ")
                );
                GateOutcome::Passed
            }
            Err(err) => {
                tracing::error!("KV list_news_windows_for_trade: {err}");
                return ActionResult::Rejected {
                    status: 500,
                    body: "state error".to_string(),
                    outcome: "rejected: state-error".into(),
                };
            }
        }
    } else {
        GateOutcome::NotSet
    };
    // Price window. Old form: `require_price_in_ranges: Some(ranges)`.
    // New form: `inside_window` contains `Price` (with bands in
    // `sr_bands`). Validation guarantees `sr_bands` is non-empty
    // exactly when `inside_window` lists Price.
    let price_ranges: Option<&[[f64; 2]]> = match verified.intent.require_price_in_ranges.as_deref()
    {
        Some(ranges) => Some(ranges),
        None if verified
            .intent
            .inside_window
            .contains(&crate::intent::EventWindow::Price) =>
        {
            Some(verified.intent.sr_bands.as_slice())
        }
        None => None,
    };
    let price_outcome = match price_ranges {
        Some(ranges) => match broker.get_current_price(&verified.intent.instrument).await {
            Ok(price) => match price_band_hit(price, ranges) {
                Some([lo, hi]) => {
                    tracing::info!(
                        "close price-range gate passed: {} price={price} in [{lo}, {hi}]",
                        verified.intent.instrument
                    );
                    GateOutcome::Passed
                }
                None => {
                    tracing::info!(
                        "close price-range gate failed: {} price={price} outside all bands {ranges:?}",
                        verified.intent.instrument
                    );
                    GateOutcome::Failed("price-out-of-range")
                }
            },
            Err(err) => {
                tracing::error!(
                    "broker get_current_price for {}: {err:?}",
                    verified.intent.instrument
                );
                return ActionResult::Rejected {
                    status: 500,
                    body: "price-fetch failed".to_string(),
                    outcome: "rejected: price-fetch-failed".into(),
                };
            }
        },
        None => GateOutcome::NotSet,
    };
    // Contextual gate (OR-composed).
    if let GateDecision::Reject { reason_code } = evaluate_close_gates(news_outcome, price_outcome)
    {
        return ActionResult::Rejected {
            status: 423,
            body: "close gates not satisfied".to_string(),
            outcome: format!("rejected: {reason_code}"),
        };
    }
    // Candle quality + allow_close script (AND-composed with the
    // contextual gate). Pulled into `allow_close_gate::evaluate` so the
    // gate-mapping logic lives next to the entry-side analogue.
    match allow_close_gate::evaluate(&verified.intent, &verified.shell) {
        allow_close_gate::AllowCloseOutcome::Proceed => {}
        allow_close_gate::AllowCloseOutcome::Blocked => {
            tracing::info!(
                "close rejected: allow_close returned false (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                status: 412,
                body: "close blocked".to_string(),
                outcome: "rejected: allow-close-false".into(),
            };
        }
        allow_close_gate::AllowCloseOutcome::NeedsGoldenUnmet => {
            tracing::info!(
                "close rejected: needs_golden set but shell.golden != Some(true) (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                status: 412,
                body: "close blocked: needs-golden".to_string(),
                outcome: "rejected: needs-golden".into(),
            };
        }
        allow_close_gate::AllowCloseOutcome::NeedsConfirmedUnmet => {
            tracing::info!(
                "close rejected: needs_confirmed set but shell.signal_confirmed != Some(true) (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                status: 412,
                body: "close blocked: needs-confirmed".to_string(),
                outcome: "rejected: needs-confirmed".into(),
            };
        }
        allow_close_gate::AllowCloseOutcome::ScriptError { kind, message } => {
            tracing::error!(
                "allow_close script error (id={}): {message}",
                verified.intent.id
            );
            return ActionResult::Rejected {
                status: 412,
                body: "close blocked: script error".to_string(),
                outcome: format!("rejected: allow-close-{kind}"),
            };
        }
    }
    let ok = broker.close_positions(&verified.intent.instrument).await;
    // veto_on_reversal (experimental, opt-in): a reversal-close whose
    // gate passed is a real reversal signal. If the operator armed this
    // flag, also record a `reversal` veto for this trade_id so a *later*
    // enter is blocked — the case where the reversal lands before entry
    // and `close_positions` was a no-op. Written on every gate-pass
    // (idempotent key, TTL refreshed); independent of whether a position
    // was actually open. The close result itself still drives the
    // response below. Validation guarantees veto_on_reversal implies a
    // price window and a Close action; a missing trade_id is the only
    // remaining reason we'd skip — log it rather than fail the close.
    if verified.intent.veto_on_reversal {
        write_reversal_veto(store, verified, now).await;
    }
    if ok {
        ActionResult::Ok("closed".into())
    } else {
        ActionResult::Failed("close-failed".into())
    }
}

/// The veto a gate-passed reversal-close should write under the
/// `veto_on_reversal` hook. Borrowed from the intent so the KV call is a
/// thin wrapper; `None` means there's no `trade_id` to scope the veto to
/// (we log + skip rather than write a global veto). Pulled out of the
/// KV-calling path so the decision is unit-testable without a KV fixture.
struct ReversalVetoPlan<'a> {
    account: Option<&'a str>,
    trade_id: &'a str,
    instrument: &'a str,
    ttl_seconds: u64,
}

/// Decide the reversal veto for a gate-passed reversal-close. Returns
/// `None` when the intent carries no `trade_id` (vetos are trade-scoped;
/// a global reversal veto would bleed across setups). TTL follows the
/// same rule as a `too-high` veto — live for the life of the alert
/// window (`not_after` tail), with a zero `ttl_hours` component since a
/// reversal-close fires mid-window.
fn reversal_veto_plan<'a>(
    intent: &'a Intent,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<ReversalVetoPlan<'a>> {
    let trade_id = intent.trade_id.as_deref()?;
    Some(ReversalVetoPlan {
        account: intent.account.as_deref(),
        trade_id,
        instrument: &intent.instrument,
        ttl_seconds: veto_ttl_seconds(0, intent.not_after, now),
    })
}

/// Write the experimental `reversal` veto for a gate-passed
/// reversal-close. Best-effort: a KV failure or a missing `trade_id` is
/// logged and swallowed — the close has already happened and the veto is
/// an additive guard, not a precondition for it.
async fn write_reversal_veto<S: StateStore>(
    store: &S,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) {
    let Some(plan) = reversal_veto_plan(&verified.intent, now) else {
        tracing::info!(
            "veto_on_reversal set but close has no trade_id (id={}); skipping reversal veto",
            verified.intent.id
        );
        return;
    };
    if let Err(err) = store
        .set_veto(
            plan.account,
            plan.trade_id,
            plan.instrument,
            REVERSAL_VETO_NAME,
            plan.ttl_seconds,
        )
        .await
    {
        tracing::error!("KV set_veto (reversal): {err}");
        return;
    }
    record_control_event_for(
        store,
        plan.account,
        Some(plan.trade_id),
        crate::control_event::ControlKind::Veto,
        REVERSAL_VETO_NAME,
        plan.instrument,
        plan.ttl_seconds,
        now,
        None,
    )
    .await;
    tracing::info!(
        "reversal veto set: instrument={} account={} trade_id={} name={REVERSAL_VETO_NAME} ttl={}s",
        plan.instrument,
        plan.account.unwrap_or("<global>"),
        plan.trade_id,
        plan.ttl_seconds,
    );
}

/// Per-gate evaluation result. `Failed` carries a short reason code
/// that lands in the outcome string when *no* set gate passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateOutcome {
    /// Gate was not configured on this intent.
    NotSet,
    /// Gate was configured and its condition was met.
    Passed,
    /// Gate was configured and its condition was not met.
    Failed(&'static str),
}

/// OR-composed gate decision. Used by [`run_close`] and unit tests.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GateDecision {
    /// At least one set gate passed (or no gates were set).
    Pass,
    /// One or more gates were set and all of them failed. The
    /// `reason_code` joins each failing gate's short code with `|`
    /// so an operator reading the seen index sees what was tried.
    Reject { reason_code: String },
}

/// Compose per-gate outcomes into a single Pass/Reject decision using
/// **OR** semantics: pass when no gates are set, pass when at least
/// one set gate passed, reject only when every set gate failed.
fn evaluate_close_gates(news: GateOutcome, price: GateOutcome) -> GateDecision {
    let outcomes = [news, price];
    let any_passed = outcomes.iter().any(|o| matches!(o, GateOutcome::Passed));
    let any_set = outcomes.iter().any(|o| !matches!(o, GateOutcome::NotSet));
    if any_passed || !any_set {
        return GateDecision::Pass;
    }
    let reason_code = outcomes
        .iter()
        .filter_map(|o| match o {
            GateOutcome::Failed(code) => Some(*code),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("|");
    GateDecision::Reject { reason_code }
}

/// Find the first `[lo, hi]` band in `ranges` that contains `price`
/// (inclusive on both ends). Returns `None` when `price` sits outside
/// every band. Pulled out of [`run_close`] so the gate logic itself
/// can be unit-tested without standing up a full broker + KV fixture.
fn price_band_hit(price: f64, ranges: &[[f64; 2]]) -> Option<[f64; 2]> {
    ranges
        .iter()
        .copied()
        .find(|[lo, hi]| price >= *lo && price <= *hi)
}

#[cfg(test)]
mod price_band_tests {
    use super::price_band_hit;

    #[test]
    fn price_inside_single_band_hits() {
        let ranges = [[1.0950, 1.0970]];
        assert_eq!(price_band_hit(1.0960, &ranges), Some([1.0950, 1.0970]));
    }

    #[test]
    fn price_on_band_endpoints_hits() {
        let ranges = [[1.0950, 1.0970]];
        assert!(price_band_hit(1.0950, &ranges).is_some());
        assert!(price_band_hit(1.0970, &ranges).is_some());
    }

    #[test]
    fn price_outside_all_bands_misses() {
        let ranges = [[1.0950, 1.0970], [1.1000, 1.1020]];
        assert_eq!(price_band_hit(1.0980, &ranges), None);
        assert_eq!(price_band_hit(1.0900, &ranges), None);
        assert_eq!(price_band_hit(1.1100, &ranges), None);
    }

    #[test]
    fn price_picks_first_matching_band_when_multiple_overlap() {
        let ranges = [[1.0950, 1.0970], [1.0960, 1.0980]];
        assert_eq!(price_band_hit(1.0965, &ranges), Some([1.0950, 1.0970]));
    }

    #[test]
    fn empty_ranges_always_misses() {
        assert_eq!(price_band_hit(1.0, &[]), None);
    }
}

#[cfg(test)]
mod close_gate_tests {
    use super::{GateDecision, GateOutcome, evaluate_close_gates};

    #[test]
    fn no_gates_set_passes() {
        assert_eq!(
            evaluate_close_gates(GateOutcome::NotSet, GateOutcome::NotSet),
            GateDecision::Pass,
        );
    }

    #[test]
    fn single_news_gate_passing_passes() {
        assert_eq!(
            evaluate_close_gates(GateOutcome::Passed, GateOutcome::NotSet),
            GateDecision::Pass,
        );
    }

    #[test]
    fn single_news_gate_failing_rejects_with_its_code() {
        assert_eq!(
            evaluate_close_gates(GateOutcome::Failed("no-news-window"), GateOutcome::NotSet),
            GateDecision::Reject {
                reason_code: "no-news-window".into(),
            },
        );
    }

    #[test]
    fn single_price_gate_failing_rejects_with_its_code() {
        assert_eq!(
            evaluate_close_gates(
                GateOutcome::NotSet,
                GateOutcome::Failed("price-out-of-range"),
            ),
            GateDecision::Reject {
                reason_code: "price-out-of-range".into(),
            },
        );
    }

    #[test]
    fn both_gates_set_news_passes_price_fails_passes() {
        assert_eq!(
            evaluate_close_gates(
                GateOutcome::Passed,
                GateOutcome::Failed("price-out-of-range"),
            ),
            GateDecision::Pass,
        );
    }

    #[test]
    fn both_gates_set_news_fails_price_passes_passes() {
        assert_eq!(
            evaluate_close_gates(GateOutcome::Failed("no-news-window"), GateOutcome::Passed),
            GateDecision::Pass,
        );
    }

    #[test]
    fn both_gates_set_both_pass_passes() {
        assert_eq!(
            evaluate_close_gates(GateOutcome::Passed, GateOutcome::Passed),
            GateDecision::Pass,
        );
    }

    #[test]
    fn both_gates_set_both_fail_rejects_with_joined_codes() {
        assert_eq!(
            evaluate_close_gates(
                GateOutcome::Failed("no-news-window"),
                GateOutcome::Failed("price-out-of-range"),
            ),
            GateDecision::Reject {
                reason_code: "no-news-window|price-out-of-range".into(),
            },
        );
    }
}

#[cfg(test)]
mod reversal_veto_tests {
    use super::reversal_veto_plan;
    use crate::intent::{Intent, REVERSAL_VETO_NAME};

    fn close_intent(yaml_extra: &str) -> Intent {
        let yaml = format!(
            "
            v: 1
            id: rev-close
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            inside_window: [price]
            sr_bands: [[1.0950, 1.0970]]
            veto_on_reversal: true
{yaml_extra}
        "
        );
        serde_yaml::from_str(&yaml).expect("close intent parses")
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        "2026-05-13T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn plan_is_none_without_trade_id() {
        // No trade_id → no trade-scoped veto to write (we don't fall back
        // to a global reversal veto that would bleed across setups).
        let intent = close_intent("");
        assert!(reversal_veto_plan(&intent, now()).is_none());
    }

    #[test]
    fn plan_scopes_to_trade_id_and_account() {
        let intent =
            close_intent("            trade_id: eurusd-hs-1\n            account: reversals");
        let plan = reversal_veto_plan(&intent, now()).expect("plan present");
        assert_eq!(plan.trade_id, "eurusd-hs-1");
        assert_eq!(plan.account, Some("reversals"));
        assert_eq!(plan.instrument, "EUR_USD");
    }

    #[test]
    fn plan_account_is_none_when_unset() {
        let intent = close_intent("            trade_id: eurusd-hs-1");
        let plan = reversal_veto_plan(&intent, now()).expect("plan present");
        assert_eq!(plan.account, None);
    }

    #[test]
    fn plan_ttl_lives_to_window_end() {
        // not_after is 2026-05-13T20:00:00Z, now is 12:00:00Z → 8h window.
        // veto_ttl_seconds(0, ..) = ttl_hours(0) + remaining(8h) = 8h, so
        // the reversal veto lives exactly to the end of the alert window —
        // killing this setup's remaining entries, no longer.
        let intent = close_intent("            trade_id: eurusd-hs-1");
        let plan = reversal_veto_plan(&intent, now()).expect("plan present");
        assert_eq!(plan.ttl_seconds, 8 * 3600);
    }

    #[test]
    fn veto_name_is_reversal() {
        assert_eq!(REVERSAL_VETO_NAME, "reversal");
    }
}
