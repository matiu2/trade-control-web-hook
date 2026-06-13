//! Stop-entry "too close to market" fallback (`on_too_close`).
//!
//! When a stop-entry's resting placement is rejected because the trigger
//! has already been overtaken by price (TradeNation `#19-10` — see
//! [`trade_control_core::broker::EntryError::EntryTooCloseToMarket`]),
//! the worker can optionally recover instead of dropping the entry. The
//! desired behaviour travels in the signed intent
//! ([`trade_control_core::intent::OnTooClose`]) and resolves to
//! [`trade_control_core::intent::ResolvedOnTooClose`].
//!
//! This module holds the **pure** decision logic so it can be unit
//! tested off-wasm without a broker or KV store:
//!
//! - [`outcome_for_entry_error`] renders the distinct outcome string
//!   the dispatcher records (Step 1 — observability). A too-close
//!   failure must stay `ActionResult::Failed` (a Skip in `seen_decision`)
//!   so the seen-id is never poisoned and the next bar can retry.
//! - [`market_replace_plan`] decides whether a `#19-10` rejection should
//!   be recovered by re-placing as a market order, and at what
//!   reference price, given the `on_too_close` config and the current
//!   market price (Step 3 — the slippage-guarded recovery path).
//!
//! Both are KV-free and broker-free; the actual broker re-place and KV
//! bookkeeping live in `run_enter` (`src/lib.rs`).

use trade_control_core::broker::EntryError;
use trade_control_core::intent::{Direction, OnTooCloseAction, ResolvedOnTooClose};

/// Distinct outcome string for an entry-placement failure. The
/// too-close case gets its own label so a `#19-10` rejection is a
/// 30-second log grep instead of an opaque `broker rejected the order`.
/// Every other error keeps the generic rendering.
pub fn outcome_for_entry_error(err: &EntryError) -> String {
    match err {
        EntryError::EntryTooCloseToMarket => "entry-failed: too-close-to-market".to_string(),
        other => format!("entry-failed: {other}"),
    }
}

/// The recovery decision for a `#19-10` rejection.
#[derive(Debug, Clone, PartialEq)]
pub enum TooClosePlan {
    /// Re-place as a market order at `reference_price` (the current
    /// market price). The caller re-runs sizing against this reference
    /// — a worse fill changes the stop distance and therefore the
    /// position size, so the original stop-trigger math must not be
    /// reused.
    Market { reference_price: f64 },
    /// Don't recover — keep the failure terminal (`Failed` → 502, no
    /// seen-id poison, next bar retries). `reason` is a short
    /// telemetry-friendly suffix for the log / outcome string.
    Skip { reason: &'static str },
}

/// Decide how to handle a too-close rejection.
///
/// `on_too_close` is the resolved fallback carried on the trade (`None`
/// when the operator didn't opt in). `trigger_price` is the original
/// stop trigger; `current_price` is a fresh read of the market.
///
/// Rules:
/// - No fallback, or `skip` → [`TooClosePlan::Skip`] (today's behaviour).
/// - `limit` → Skip for now (Step 4, not yet implemented) so the entry
///   stays retryable rather than silently dropped.
/// - `market` with no slippage bound (validation should have caught it)
///   → Skip — never chase an unbounded fill.
/// - `market` with a bound → re-place **only if** the current price is
///   within `max_slippage_price` of the trigger on the breakout side
///   (long: price ran *up* past the trigger; short: *down*). Out of
///   threshold → Skip with `too-close-slippage`.
///
/// A non-finite current price is treated as out-of-threshold (Skip).
pub fn market_replace_plan(
    on_too_close: Option<&ResolvedOnTooClose>,
    direction: Direction,
    trigger_price: f64,
    current_price: f64,
) -> TooClosePlan {
    let Some(otc) = on_too_close else {
        return TooClosePlan::Skip {
            reason: "too-close-no-fallback",
        };
    };

    match otc.action {
        OnTooCloseAction::Skip => TooClosePlan::Skip {
            reason: "too-close-skip",
        },
        OnTooCloseAction::Limit => TooClosePlan::Skip {
            reason: "too-close-limit-unimplemented",
        },
        OnTooCloseAction::Market => {
            let Some(max_slip) = otc.max_slippage_price else {
                return TooClosePlan::Skip {
                    reason: "too-close-market-no-slippage-bound",
                };
            };
            if !current_price.is_finite() || !trigger_price.is_finite() {
                return TooClosePlan::Skip {
                    reason: "too-close-price-unavailable",
                };
            }
            // The slippage is the distance the breakout has run *past*
            // the trigger on the entry side: long entries trigger when
            // price rises into them, so an overtaken long trigger sits
            // *below* the current price (slippage = current - trigger);
            // a short trigger sits *above* (slippage = trigger -
            // current). A signed value <= 0 means price hasn't actually
            // overtaken the trigger on that side — unexpected for a
            // genuine `#19-10`, but harmless: it's well within bound, so
            // we proceed.
            let slippage = match direction {
                Direction::Long => current_price - trigger_price,
                Direction::Short => trigger_price - current_price,
            };
            if slippage <= max_slip {
                TooClosePlan::Market {
                    reference_price: current_price,
                }
            } else {
                TooClosePlan::Skip {
                    reason: "too-close-slippage",
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn otc(action: OnTooCloseAction, max_slippage_price: Option<f64>) -> ResolvedOnTooClose {
        ResolvedOnTooClose {
            action,
            max_slippage_price,
        }
    }

    #[test]
    fn too_close_error_renders_distinct_outcome() {
        assert_eq!(
            outcome_for_entry_error(&EntryError::EntryTooCloseToMarket),
            "entry-failed: too-close-to-market"
        );
    }

    #[test]
    fn other_errors_keep_generic_outcome() {
        let s = outcome_for_entry_error(&EntryError::OrderRejected);
        assert!(s.starts_with("entry-failed: "));
        assert!(!s.contains("too-close-to-market"));
    }

    #[test]
    fn no_fallback_skips() {
        let plan = market_replace_plan(None, Direction::Long, 1.1000, 1.1005);
        assert_eq!(
            plan,
            TooClosePlan::Skip {
                reason: "too-close-no-fallback"
            }
        );
    }

    #[test]
    fn skip_action_skips() {
        let cfg = otc(OnTooCloseAction::Skip, None);
        let plan = market_replace_plan(Some(&cfg), Direction::Long, 1.1000, 1.1005);
        assert!(matches!(plan, TooClosePlan::Skip { reason } if reason == "too-close-skip"));
    }

    #[test]
    fn limit_action_skips_until_implemented() {
        let cfg = otc(OnTooCloseAction::Limit, None);
        let plan = market_replace_plan(Some(&cfg), Direction::Long, 1.1000, 1.1005);
        assert!(
            matches!(plan, TooClosePlan::Skip { reason } if reason == "too-close-limit-unimplemented")
        );
    }

    #[test]
    fn market_within_threshold_replaces_at_current_price() {
        // Long: trigger 1.1000, price ran to 1.1005 = 5 pips slip;
        // bound is 8 pips (0.0008). Within → replace at 1.1005.
        let cfg = otc(OnTooCloseAction::Market, Some(0.0008));
        let plan = market_replace_plan(Some(&cfg), Direction::Long, 1.1000, 1.1005);
        match plan {
            TooClosePlan::Market { reference_price } => {
                assert!((reference_price - 1.1005).abs() < 1e-12);
            }
            other => panic!("expected Market, got {other:?}"),
        }
    }

    #[test]
    fn market_out_of_threshold_skips() {
        // Long: 65 pips of slip against an 8-pip bound — the GBP/NZD
        // chase the guard exists to prevent.
        let cfg = otc(OnTooCloseAction::Market, Some(0.0008));
        let plan = market_replace_plan(Some(&cfg), Direction::Long, 1.1000, 1.1065);
        assert!(matches!(plan, TooClosePlan::Skip { reason } if reason == "too-close-slippage"));
    }

    #[test]
    fn market_short_direction_uses_downside_slip() {
        // Short: trigger 1.1000, price ran *down* to 1.0994 = 6 pips
        // slip; bound 8 pips → within. (An upside move would be
        // negative slip — also within.)
        let cfg = otc(OnTooCloseAction::Market, Some(0.0008));
        let plan = market_replace_plan(Some(&cfg), Direction::Short, 1.1000, 1.0994);
        assert!(matches!(plan, TooClosePlan::Market { .. }));

        // 12 pips down → out of threshold.
        let plan = market_replace_plan(Some(&cfg), Direction::Short, 1.1000, 1.0988);
        assert!(matches!(plan, TooClosePlan::Skip { reason } if reason == "too-close-slippage"));
    }

    #[test]
    fn market_at_exact_threshold_replaces() {
        // Boundary: exactly 8 pips slip with an 8-pip bound → allowed.
        let cfg = otc(OnTooCloseAction::Market, Some(0.0008));
        let plan = market_replace_plan(Some(&cfg), Direction::Long, 1.1000, 1.1008);
        assert!(matches!(plan, TooClosePlan::Market { .. }));
    }

    #[test]
    fn market_without_bound_skips_defensively() {
        // Validation should reject this upstream, but if a malformed
        // intent slips through the worker must not chase unbounded.
        let cfg = otc(OnTooCloseAction::Market, None);
        let plan = market_replace_plan(Some(&cfg), Direction::Long, 1.1000, 1.1005);
        assert!(
            matches!(plan, TooClosePlan::Skip { reason } if reason == "too-close-market-no-slippage-bound")
        );
    }

    #[test]
    fn market_non_finite_price_skips() {
        let cfg = otc(OnTooCloseAction::Market, Some(0.0008));
        let plan = market_replace_plan(Some(&cfg), Direction::Long, 1.1000, f64::NAN);
        assert!(
            matches!(plan, TooClosePlan::Skip { reason } if reason == "too-close-price-unavailable")
        );
    }
}
