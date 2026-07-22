//! The `Close` dispatch path (reversal-close). The `veto_on_reversal`
//! flag is retained on the wire as a dormant no-op: a gate-passed
//! reversal-close is **exit-only** (2026-07-19) — it flattens the open
//! position and never writes a `reversal` veto. See `run_close`.

use super::action_result::ActionResult;
use crate::allow_close_gate;
use crate::broker::Broker;
use crate::incoming;
use crate::state::StateStore;

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
    // `now` is kept for signature parity with the other dispatch handlers
    // (`run_enter` etc., routed via `dispatch_action`). It was only used by
    // the removed reversal-veto write (now exit-only) so it's unused here.
    _now: chrono::DateTime<chrono::Utc>,
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
    // Price window. Two forms with two price sources:
    //
    // - **New `sr_bands` form** (`inside_window` contains `Price`): tests the
    //   reversal candle's pattern-aware **band anchor** off the shell
    //   (`Shell::band_anchor` — open for engulfers, wick-50% for pinbars), NOT a
    //   live broker price. This keys the "off an S/R level" test on where the
    //   candle *rejected*, matching the engine's `close_windows_pass` exactly so
    //   replay == live. A candle that merely closed-into the band (continuation)
    //   no longer trips it (UK 100 2026-07-17). The cron engine builds this
    //   shell via `Shell::from_candle_and_signal`, so `signal_kind` + `open` are
    //   present.
    // - **Deprecated `require_price_in_ranges` form**: predates `sr_bands` and
    //   the pattern anchor; keeps its original live-price semantics. Nothing new
    //   emits it, so it's left untouched.
    //
    // Validation guarantees `sr_bands` is non-empty exactly when `inside_window`
    // lists Price, and that the two forms are mutually exclusive.
    let price_outcome = if let Some(ranges) = verified.intent.require_price_in_ranges.as_deref() {
        // Deprecated form → live broker price.
        match broker.get_current_price(&verified.intent.instrument).await {
            Ok(price) => match price_band_hit(price, ranges) {
                Some([lo, hi]) => {
                    tracing::info!(
                        "close price-range gate passed (legacy): {} price={price} in [{lo}, {hi}]",
                        verified.intent.instrument
                    );
                    GateOutcome::Passed
                }
                None => {
                    tracing::info!(
                        "close price-range gate failed (legacy): {} price={price} outside all bands {ranges:?}",
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
        }
    } else if verified
        .intent
        .inside_window
        .contains(&crate::intent::EventWindow::Price)
    {
        // New form → pattern-aware band anchor off the shell.
        let ranges = verified.intent.sr_bands.as_slice();
        match verified.shell.band_anchor() {
            Some(anchor) => match price_band_hit(anchor, ranges) {
                Some([lo, hi]) => {
                    tracing::info!(
                        "close price-range gate passed: {} anchor={anchor} in [{lo}, {hi}]",
                        verified.intent.instrument
                    );
                    GateOutcome::Passed
                }
                None => {
                    tracing::info!(
                        "close price-range gate failed: {} anchor={anchor} outside all bands {ranges:?}",
                        verified.intent.instrument
                    );
                    GateOutcome::Failed("price-out-of-range")
                }
            },
            // A Price-windowed close whose shell can't yield an anchor (no
            // signal_kind / no open) can't be evaluated for "off an S/R level".
            // Fail the gate closed — never flatten a position on an
            // un-anchorable band test.
            None => {
                tracing::warn!(
                    "close price-range gate: {} shell has no band anchor (signal_kind/open missing) — failing closed",
                    verified.intent.instrument
                );
                GateOutcome::Failed("no-band-anchor")
            }
        }
    } else {
        GateOutcome::NotSet
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
    // veto_on_reversal is EXIT-ONLY (2026-07-19): a gate-passed
    // reversal-close flattens the open position and does NOTHING else. It
    // no longer writes a `reversal` veto to block a future enter. The
    // operator's model is that a reversal candle is a reason to *exit*,
    // not a reason to *stay out* — once flat, a fresh signal may re-enter.
    // Blocking future entries is the job of the independent invalidation
    // caps (`too-high`/`too-low`) and the 80%-to-TP `pcl-exhausted` abort,
    // which fire on their own and (correctly) don't close the position.
    // The flag/field is retained as a dormant no-op; see the enter-builder
    // (`cli/src/trade_patterns.rs`), which no longer attaches `reversal` to
    // the enter's `vetos`. Removing the write here also closes a replay↔live
    // divergence: the offline replay never ran `run_close`, so it never
    // wrote this veto, and a later multi-shot enter passed offline but
    // rejected live. With no veto written on either side, replay == live.
    if ok {
        ActionResult::Ok("closed".into())
    } else {
        ActionResult::Failed("close-failed".into())
    }
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
mod reversal_exit_only_tests {
    use super::run_close;
    use crate::broker::*;
    use crate::dispatch::ActionResult;
    use crate::incoming::Verified;
    use crate::intent::{Intent, REVERSAL_VETO_NAME, Shell};
    use crate::state::{MemStateStore, StateStore};
    use chrono::{DateTime, Utc};

    /// A broker whose quote sits inside the reversal band so the
    /// price-window gate passes and `run_close` reaches the (now removed)
    /// veto-write site. `close_positions` returns true so the close is
    /// `Ok` — the veto used to be written on the gate-pass regardless.
    struct InBandBroker;

    impl Broker for InBandBroker {
        async fn place_entry(
            &self,
            _max_risk_pct: f64,
            _max_open_positions: u32,
            _req: &EntryRequest<'_>,
        ) -> Result<String, EntryError> {
            Ok("noop".into())
        }
        async fn close_positions(&self, _instrument: &str) -> bool {
            true
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
            // Mid = 1.0960, inside the band [1.0950, 1.0970] below.
            Ok(Quote {
                bid: 1.0959,
                ask: 1.0961,
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

    fn now() -> DateTime<Utc> {
        "2026-05-13T12:00:00Z".parse().unwrap()
    }

    fn armed_close() -> Verified {
        let intent: Intent = serde_yaml::from_str(
            "
            v: 1
            id: rev-close
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            trade_id: eurusd-hs-1
            account: reversals
            inside_window: [price]
            sr_bands: [[1.0950, 1.0970]]
            veto_on_reversal: true
        ",
        )
        .expect("close intent parses");
        // A reversal-candle shell (no needs_golden / needs_confirmed set on
        // the intent, so the candle-quality gate is a no-op). The price window
        // now tests the pattern-aware band anchor off this shell: a bearish
        // regular engulfer anchors on its OPEN (1.0960), which sits inside the
        // band [1.0950, 1.0970], so the price gate passes and the close reaches
        // the (removed) veto-write site.
        let shell: Shell = serde_yaml::from_str(
            "
            open: 1.0960
            close: 1.0952
            high: 1.0965
            low: 1.0950
            time: \"2026-05-13T12:00:00Z\"
            signal_kind: 3
        ",
        )
        .expect("shell parses");
        Verified { shell, intent }
    }

    #[test]
    fn gate_passed_close_with_veto_on_reversal_writes_no_veto() {
        // EXIT-ONLY: a reversal-close whose gate passes flattens the
        // position and writes NO `reversal` veto. Before this fix the
        // gate-pass wrote a trade-scoped `reversal` veto that blocked a
        // later enter (live), which the offline replay never wrote —
        // a replay↔live divergence. Now neither side writes it.
        let store = MemStateStore::default();
        let verified = armed_close();

        let result = pollster::block_on(run_close(&InBandBroker, &store, &verified, now()));
        assert!(
            matches!(result, ActionResult::Ok(_)),
            "gate should pass and close succeed, got {}",
            result.describe()
        );

        let vetoed = pollster::block_on(store.is_vetoed(
            Some("reversals"),
            "eurusd-hs-1",
            "EUR_USD",
            REVERSAL_VETO_NAME,
        ))
        .expect("is_vetoed query");
        assert!(
            !vetoed,
            "reversal-close must NOT write a `reversal` veto (exit-only)"
        );
    }

    /// The UK 100 2026-07-17 shape: a bearish engulfer whose CLOSE lands inside
    /// the band but whose OPEN is above it — price fell *into* the level
    /// (continuation), not bounced *off* it. The pattern-aware anchor (= open
    /// for an engulfer) is out of band, so the price gate fails and the position
    /// is NOT flattened. Under the old close-in-band rule this wrongly closed.
    #[test]
    fn engulfer_that_closed_into_band_but_opened_above_does_not_close() {
        let store = MemStateStore::default();
        let mut verified = armed_close();
        // Open 1.0980 (above band top 1.0970), close 1.0960 (in band).
        verified.shell = serde_yaml::from_str(
            "
            open: 1.0980
            close: 1.0960
            high: 1.0982
            low: 1.0958
            time: \"2026-05-13T12:00:00Z\"
            signal_kind: 3
        ",
        )
        .expect("shell parses");

        let result = pollster::block_on(run_close(&InBandBroker, &store, &verified, now()));
        assert!(
            matches!(result, ActionResult::Rejected { .. }),
            "an engulfer that opened above the band (continuation) must NOT close, got {}",
            result.describe()
        );
    }
}
