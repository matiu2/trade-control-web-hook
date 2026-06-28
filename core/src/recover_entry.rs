//! Broker-side stop-entry recovery (`recover_entry`, `#19-10` path).
//!
//! When a stop-entry's resting placement is rejected because the trigger
//! has already been overtaken by price (TradeNation `#19-10` — see
//! [`trade_control_core::broker::EntryError::EntryTooCloseToMarket`]),
//! the worker can optionally recover instead of dropping the entry. The
//! desired behaviour travels in the signed intent
//! ([`trade_control_core::intent::RecoverEntry`]) and resolves to
//! [`trade_control_core::intent::ResolvedRecoverEntry`].
//!
//! This module holds the **pure** decision logic so it can be unit
//! tested off-wasm without a broker or KV store:
//!
//! - [`outcome_for_entry_error`] renders the distinct outcome string
//!   the dispatcher records (Step 1 — observability). A `#19-10`
//!   failure must stay `ActionResult::Failed` (a Skip in `seen_decision`)
//!   so the seen-id is never poisoned and the next bar can retry.
//! - [`recover_entry_plan`] decides whether a `#19-10` rejection should
//!   be recovered, and how — re-place as a market order (the
//!   slippage-guarded chase), as a limit order resting at the original
//!   trigger (R-preserving), or skip — given the `recover_entry` config
//!   and the current market price.
//!
//! Both are KV-free and broker-free; the actual broker re-place and KV
//! bookkeeping live in `run_enter` (`src/lib.rs`).

use crate::broker::EntryError;
use crate::intent::{Direction, RecoverEntryAction, ResolvedRecoverEntry};

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
pub enum RecoverEntryPlan {
    /// Re-place as a market order at `reference_price` (the current
    /// market price). The caller re-runs sizing against this reference
    /// — a worse fill changes the stop distance and therefore the
    /// position size, so the original stop-trigger math must not be
    /// reused.
    Market { reference_price: f64 },
    /// Re-place as a **limit** order resting at `trigger_price` (the
    /// original stop trigger). The break already happened, so a limit at
    /// the original level waits for a pullback to the intended entry —
    /// preserving the planned R exactly (a limit can't fill worse than
    /// its price), at the cost of possibly never filling. No fresh
    /// sizing is needed: the entry reference is unchanged, so the caller
    /// reuses the original stop-distance math. The resting limit is
    /// recorded as a normal `EntryAttempt`, so the cron sweep cancels it
    /// when the alert window / `expiry_bars` lapses — no broker-native
    /// GTD required.
    Limit { trigger_price: f64 },
    /// Don't recover — keep the failure terminal (`Failed` → 502, no
    /// seen-id poison, next bar retries). `reason` is a short
    /// telemetry-friendly suffix for the log / outcome string.
    Skip { reason: &'static str },
}

/// Decide how to handle a too-close rejection.
///
/// `recover_entry` is the resolved recovery carried on the trade (`None`
/// when the operator didn't opt in). `trigger_price` is the original
/// stop trigger; `current_price` is a fresh read of the market.
///
/// Rules:
/// - No fallback, or `skip` → [`RecoverEntryPlan::Skip`] (today's behaviour).
/// - `limit` → re-place a limit at the original trigger, **only if** the
///   limit would rest on the correct side of the market (long: trigger
///   at/below current; short: at/above). In a genuine `#19-10` the price
///   has overrun the stop trigger so the original trigger IS the correct
///   side — but a degenerate / non-overrun case (trigger still on the
///   wrong side) would create a `#19-9`, so guard it and Skip with
///   `recover-entry-limit-wrong-side`. No slippage bound applies — a
///   limit can't fill worse than its price, so R is preserved.
/// - `market` with no slippage bound (the resolver derives one when the
///   intent omits it, so this is only the defensive path) → Skip — never
///   chase an unbounded fill.
/// - `market` with a bound → re-place **only if** the current price is
///   within `max_slippage_price` of the trigger on the breakout side
///   (long: price ran *up* past the trigger; short: *down*). Out of
///   threshold → Skip with `recover-entry-slippage`.
///
/// A non-finite current price is treated as out-of-threshold (Skip).
pub fn recover_entry_plan(
    recover_entry: Option<&ResolvedRecoverEntry>,
    direction: Direction,
    trigger_price: f64,
    current_price: f64,
) -> RecoverEntryPlan {
    let Some(rec) = recover_entry else {
        return RecoverEntryPlan::Skip {
            reason: "recover-entry-none",
        };
    };

    match rec.action {
        RecoverEntryAction::Skip => RecoverEntryPlan::Skip {
            reason: "recover-entry-skip",
        },
        RecoverEntryAction::Limit => {
            if !current_price.is_finite() || !trigger_price.is_finite() {
                return RecoverEntryPlan::Skip {
                    reason: "recover-entry-price-unavailable",
                };
            }
            // A long limit must rest at/below the market, a short limit
            // at/above — otherwise it's a `#19-9` ("limit on the wrong
            // side"), the sibling of the `#19-10` we're recovering from.
            // For a genuine too-close the price overran the trigger
            // (long: current >= trigger; short: current <= trigger), so
            // the original trigger is the correct side and fills on a
            // pullback. The `>=` / `<=` allow the equality (price exactly
            // at the trigger) — the broker treats that as a marketable
            // limit, which is acceptable.
            let correct_side = match direction {
                Direction::Long => current_price >= trigger_price,
                Direction::Short => current_price <= trigger_price,
            };
            if correct_side {
                RecoverEntryPlan::Limit { trigger_price }
            } else {
                RecoverEntryPlan::Skip {
                    reason: "recover-entry-limit-wrong-side",
                }
            }
        }
        RecoverEntryAction::Market => {
            let Some(max_slip) = rec.max_slippage_price else {
                return RecoverEntryPlan::Skip {
                    reason: "recover-entry-market-no-slippage-bound",
                };
            };
            if !current_price.is_finite() || !trigger_price.is_finite() {
                return RecoverEntryPlan::Skip {
                    reason: "recover-entry-price-unavailable",
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
                RecoverEntryPlan::Market {
                    reference_price: current_price,
                }
            } else {
                RecoverEntryPlan::Skip {
                    reason: "recover-entry-slippage",
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec_cfg(
        action: RecoverEntryAction,
        max_slippage_price: Option<f64>,
    ) -> ResolvedRecoverEntry {
        ResolvedRecoverEntry {
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
        let plan = recover_entry_plan(None, Direction::Long, 1.1000, 1.1005);
        assert_eq!(
            plan,
            RecoverEntryPlan::Skip {
                reason: "recover-entry-none"
            }
        );
    }

    #[test]
    fn skip_action_skips() {
        let cfg = rec_cfg(RecoverEntryAction::Skip, None);
        let plan = recover_entry_plan(Some(&cfg), Direction::Long, 1.1000, 1.1005);
        assert!(
            matches!(plan, RecoverEntryPlan::Skip { reason } if reason == "recover-entry-skip")
        );
    }

    #[test]
    fn limit_long_correct_side_rests_at_trigger() {
        // Genuine #19-10: long trigger 1.1000 overrun, price now 1.1005
        // (above). A limit at 1.1000 is below market → correct side,
        // rests for a pullback. No slippage bound needed; R preserved.
        let cfg = rec_cfg(RecoverEntryAction::Limit, None);
        let plan = recover_entry_plan(Some(&cfg), Direction::Long, 1.1000, 1.1005);
        match plan {
            RecoverEntryPlan::Limit { trigger_price } => {
                assert!((trigger_price - 1.1000).abs() < 1e-12);
            }
            other => panic!("expected Limit, got {other:?}"),
        }
    }

    #[test]
    fn limit_short_correct_side_rests_at_trigger() {
        // Short trigger 1.1000 overrun, price now 1.0994 (below). A
        // limit at 1.1000 is above market → correct side for a short.
        let cfg = rec_cfg(RecoverEntryAction::Limit, None);
        let plan = recover_entry_plan(Some(&cfg), Direction::Short, 1.1000, 1.0994);
        assert!(matches!(plan, RecoverEntryPlan::Limit { .. }));
    }

    #[test]
    fn limit_long_wrong_side_skips() {
        // Not a genuine overrun: long trigger 1.1000, price 1.0995 (still
        // *below* the trigger). A long limit at 1.1000 would sit above
        // market → #19-9. Guard skips instead of placing a doomed order.
        let cfg = rec_cfg(RecoverEntryAction::Limit, None);
        let plan = recover_entry_plan(Some(&cfg), Direction::Long, 1.1000, 1.0995);
        assert!(
            matches!(plan, RecoverEntryPlan::Skip { reason } if reason == "recover-entry-limit-wrong-side")
        );
    }

    #[test]
    fn limit_short_wrong_side_skips() {
        // Short trigger 1.1000, price 1.1005 (above) → a short limit at
        // 1.1000 would sit below market → #19-9. Skip.
        let cfg = rec_cfg(RecoverEntryAction::Limit, None);
        let plan = recover_entry_plan(Some(&cfg), Direction::Short, 1.1000, 1.1005);
        assert!(
            matches!(plan, RecoverEntryPlan::Skip { reason } if reason == "recover-entry-limit-wrong-side")
        );
    }

    #[test]
    fn limit_at_exact_trigger_rests() {
        // Equality (price exactly at the trigger) is allowed for both
        // directions — a marketable limit, acceptable.
        let cfg = rec_cfg(RecoverEntryAction::Limit, None);
        assert!(matches!(
            recover_entry_plan(Some(&cfg), Direction::Long, 1.1000, 1.1000),
            RecoverEntryPlan::Limit { .. }
        ));
        assert!(matches!(
            recover_entry_plan(Some(&cfg), Direction::Short, 1.1000, 1.1000),
            RecoverEntryPlan::Limit { .. }
        ));
    }

    #[test]
    fn limit_non_finite_price_skips() {
        let cfg = rec_cfg(RecoverEntryAction::Limit, None);
        let plan = recover_entry_plan(Some(&cfg), Direction::Long, 1.1000, f64::NAN);
        assert!(
            matches!(plan, RecoverEntryPlan::Skip { reason } if reason == "recover-entry-price-unavailable")
        );
    }

    #[test]
    fn market_within_threshold_replaces_at_current_price() {
        // Long: trigger 1.1000, price ran to 1.1005 = 5 pips slip;
        // bound is 8 pips (0.0008). Within → replace at 1.1005.
        let cfg = rec_cfg(RecoverEntryAction::Market, Some(0.0008));
        let plan = recover_entry_plan(Some(&cfg), Direction::Long, 1.1000, 1.1005);
        match plan {
            RecoverEntryPlan::Market { reference_price } => {
                assert!((reference_price - 1.1005).abs() < 1e-12);
            }
            other => panic!("expected Market, got {other:?}"),
        }
    }

    #[test]
    fn market_out_of_threshold_skips() {
        // Long: 65 pips of slip against an 8-pip bound — the GBP/NZD
        // chase the guard exists to prevent.
        let cfg = rec_cfg(RecoverEntryAction::Market, Some(0.0008));
        let plan = recover_entry_plan(Some(&cfg), Direction::Long, 1.1000, 1.1065);
        assert!(
            matches!(plan, RecoverEntryPlan::Skip { reason } if reason == "recover-entry-slippage")
        );
    }

    #[test]
    fn market_short_direction_uses_downside_slip() {
        // Short: trigger 1.1000, price ran *down* to 1.0994 = 6 pips
        // slip; bound 8 pips → within. (An upside move would be
        // negative slip — also within.)
        let cfg = rec_cfg(RecoverEntryAction::Market, Some(0.0008));
        let plan = recover_entry_plan(Some(&cfg), Direction::Short, 1.1000, 1.0994);
        assert!(matches!(plan, RecoverEntryPlan::Market { .. }));

        // 12 pips down → out of threshold.
        let plan = recover_entry_plan(Some(&cfg), Direction::Short, 1.1000, 1.0988);
        assert!(
            matches!(plan, RecoverEntryPlan::Skip { reason } if reason == "recover-entry-slippage")
        );
    }

    #[test]
    fn market_at_exact_threshold_replaces() {
        // Boundary: exactly 8 pips slip with an 8-pip bound → allowed.
        let cfg = rec_cfg(RecoverEntryAction::Market, Some(0.0008));
        let plan = recover_entry_plan(Some(&cfg), Direction::Long, 1.1000, 1.1008);
        assert!(matches!(plan, RecoverEntryPlan::Market { .. }));
    }

    #[test]
    fn market_without_bound_skips_defensively() {
        // Validation should reject this upstream, but if a malformed
        // intent slips through the worker must not chase unbounded.
        let cfg = rec_cfg(RecoverEntryAction::Market, None);
        let plan = recover_entry_plan(Some(&cfg), Direction::Long, 1.1000, 1.1005);
        assert!(
            matches!(plan, RecoverEntryPlan::Skip { reason } if reason == "recover-entry-market-no-slippage-bound")
        );
    }

    #[test]
    fn market_non_finite_price_skips() {
        let cfg = rec_cfg(RecoverEntryAction::Market, Some(0.0008));
        let plan = recover_entry_plan(Some(&cfg), Direction::Long, 1.1000, f64::NAN);
        assert!(
            matches!(plan, RecoverEntryPlan::Skip { reason } if reason == "recover-entry-price-unavailable")
        );
    }
}
