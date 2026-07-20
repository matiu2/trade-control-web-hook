//! Pure predicates for the cron **order sweep** — the decisions the live
//! worker's `sweep_pending_orders` (`src/cron/sweep.rs`) makes when it walks
//! each still-pending `EntryAttempt` and decides whether to cancel its resting
//! order.
//!
//! A resting stop/limit order is swept (cancelled) when any of:
//!
//! * its alert window (`expires_at` / `not_after`) has passed,
//! * its bar-based `cancel_at` (`expiry_bars` after the fire bar) has passed,
//! * it sits inside a market-hours close→open blackout window, or
//! * current price has overtaken its stop-loss (the setup invalidated before it
//!   ever filled).
//!
//! These predicates lived in the worker crate (`src/cron/sweep.rs`), which is a
//! `cdylib` the `cli` / `engine` cannot depend on — so the offline replay could
//! not tell *why* an order "never filled" (an order the worker would have
//! actively swept looks identical to one that simply never triggered). Moving
//! them here lets **both** the worker and the replay share one source of truth
//! (the `[[strategy_changes_in_both_replayer_and_worker]]` rule). The worker
//! re-exports these so its call sites are byte-unchanged; the replay's
//! `sweep_reason` (in `trade_control_engine::simulator`) reuses them to label a
//! `NeverFilled` outcome.

use chrono::{DateTime, Timelike, Utc};

use crate::intent::{Direction, NoEntryWindow, is_inside_any, market_hours_blocked};

/// Why the live cron sweep would have cancelled a still-resting entry order.
///
/// Mirrors the four act-branches of the worker's `sweep_one`. Carried by the
/// replay's `sweep_reason` to explain a `NeverFilled` outcome — an order the
/// worker would have *swept* is materially different from one that simply never
/// triggered, and a faithful replay must distinguish them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepReason {
    /// The alert window itself died (`expires_at`/`not_after` passed) while the
    /// order was still resting.
    Expired,
    /// The bar-based `cancel_at` (`expiry_bars` bars after the fire bar) passed
    /// with the order still resting.
    BarExpiry,
    /// Current price overtook the resting order's stop-loss before it filled —
    /// the setup invalidated before entry.
    SlBreached,
    /// The order was resting inside a market-hours close→open blackout window.
    /// (The offline replay can't always reconstruct the no-entry windows; it
    /// returns `None` rather than this variant when they're unavailable.)
    Blackout,
}

/// Minutes-of-day [0, 1440) for `now` in **UTC** — the coordinate the stored
/// [`NoEntryWindow`]s use. The daily deriver converts the broker's Brisbane
/// session hours to this same UTC minute-of-day axis, so the gate compares
/// like-for-like.
///
/// Lives in `core` (was worker-only in `src/market_blackout.rs`) so both the
/// reject gate / sweep (worker) and the replay share one definition.
pub fn now_utc_minute_of_day(now: DateTime<Utc>) -> u32 {
    now.hour() * 60 + now.minute()
}

/// Pure breach predicate. Long is breached when current ≤ SL; short
/// when current ≥ SL. Kept tiny and pure so it's trivially testable.
pub fn breach_detected(direction: Direction, current_price: f64, stop_loss: f64) -> bool {
    match direction {
        Direction::Long => current_price <= stop_loss,
        Direction::Short => current_price >= stop_loss,
    }
}

/// Pure bar-expiry predicate: true iff the row carries a `cancel_at`
/// that has passed. Mirrors [`breach_detected`] — tiny and pure so the
/// sweep ordering can be asserted without a broker/env.
pub fn bar_expiry_due(cancel_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    cancel_at.is_some_and(|c| c < now)
}

/// Pure market-hours-blackout predicate: true iff `now` falls inside any of
/// the instrument's derived no-entry windows. Tiny and pure (delegates to
/// the core [`is_inside_any`]) so the sweep ordering can be asserted without
/// a broker/env. Empty `windows` (24h markets / unparseable session text /
/// not-yet-refreshed) ⇒ `false` — the sweep leaves the order alone, matching
/// the reject gate's fail-open.
pub fn market_blackout_due(windows: &[NoEntryWindow], now: DateTime<Utc>) -> bool {
    let now_min = now_utc_minute_of_day(now);
    is_inside_any(now_min, windows)
}

/// Weekday-aware market-hours-blackout predicate, keyed on the broker-native
/// `symbol` (the successor to [`market_blackout_due`]). True iff `now` falls in
/// the instrument's baked [`WeekMask`](crate::intent::WeekMask) — the universal
/// weekend halt plus any per-instrument mid-week daily close. Fail-open for an
/// uncatalogued symbol (returns `false`), matching the reject gate. This is what
/// both the worker sweep and the replay call now that the window deriver is
/// retired; no KV read, no daily refresh, no timezone math.
pub fn market_blackout_due_symbol(symbol: &str, now: DateTime<Utc>) -> bool {
    market_hours_blocked(symbol, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_breach_when_price_at_or_below_sl() {
        assert!(breach_detected(Direction::Long, 1.0500, 1.0500));
        assert!(breach_detected(Direction::Long, 1.0499, 1.0500));
        assert!(!breach_detected(Direction::Long, 1.0501, 1.0500));
    }

    #[test]
    fn short_breach_when_price_at_or_above_sl() {
        assert!(breach_detected(Direction::Short, 1.0500, 1.0500));
        assert!(breach_detected(Direction::Short, 1.0501, 1.0500));
        assert!(!breach_detected(Direction::Short, 1.0499, 1.0500));
    }

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn bar_expiry_due_when_cancel_at_passed() {
        let now = ts("2026-05-13T15:00:00Z");
        assert!(bar_expiry_due(Some(ts("2026-05-13T14:59:59Z")), now));
    }

    #[test]
    fn bar_expiry_not_due_when_cancel_at_future() {
        let now = ts("2026-05-13T15:00:00Z");
        assert!(!bar_expiry_due(Some(ts("2026-05-13T15:00:01Z")), now));
    }

    #[test]
    fn bar_expiry_not_due_when_unset() {
        // Legacy rows / orders without a bar-expiry carry None — the
        // sweep must fall through to the SL/expires_at paths untouched.
        let now = ts("2026-05-13T15:00:00Z");
        assert!(!bar_expiry_due(None, now));
    }

    #[test]
    fn market_blackout_not_due_when_no_windows() {
        // 24h markets / unparseable session text / not-yet-refreshed all
        // surface as an empty window set — the sweep must leave the order
        // alone (fail-open), matching the reject gate.
        let now = ts("2026-05-13T15:00:00Z");
        assert!(!market_blackout_due(&[], now));
    }

    #[test]
    fn market_blackout_due_when_now_inside_window() {
        // Window 14:00–16:00 UTC (840..960 minutes-of-day); 15:00 is inside.
        let windows = [NoEntryWindow {
            open_min: 14 * 60,
            close_min: 16 * 60,
        }];
        let now = ts("2026-05-13T15:00:00Z");
        assert!(market_blackout_due(&windows, now));
    }

    #[test]
    fn market_blackout_not_due_when_now_outside_window() {
        // 15:00 UTC is outside an 18:00–20:00 window.
        let windows = [NoEntryWindow {
            open_min: 18 * 60,
            close_min: 20 * 60,
        }];
        let now = ts("2026-05-13T15:00:00Z");
        assert!(!market_blackout_due(&windows, now));
    }

    #[test]
    fn market_blackout_due_matches_any_of_several_windows() {
        // Several daily gaps (e.g. a maintenance gap + the overnight gap):
        // due iff inside ANY one of them. 15:00 hits the second.
        let windows = [
            NoEntryWindow {
                open_min: 2 * 60,
                close_min: 3 * 60,
            },
            NoEntryWindow {
                open_min: 14 * 60,
                close_min: 16 * 60,
            },
        ];
        let now = ts("2026-05-13T15:00:00Z");
        assert!(market_blackout_due(&windows, now));
    }

    // --- now_utc_minute_of_day (moved from src/market_blackout.rs) ----------

    use chrono::TimeZone;

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 18, hour, minute, 0).unwrap()
    }

    #[test]
    fn midnight_is_zero() {
        assert_eq!(now_utc_minute_of_day(at(0, 0)), 0);
    }

    #[test]
    fn one_minute_past_midnight() {
        assert_eq!(now_utc_minute_of_day(at(0, 1)), 1);
    }

    #[test]
    fn noon_is_seven_twenty() {
        assert_eq!(now_utc_minute_of_day(at(12, 0)), 720);
    }

    #[test]
    fn last_minute_of_day() {
        // 23:59 = 1439, strictly inside [0, 1440).
        assert_eq!(now_utc_minute_of_day(at(23, 59)), 1439);
    }

    #[test]
    fn seconds_are_ignored() {
        let with_secs = Utc.with_ymd_and_hms(2026, 6, 18, 9, 30, 45).unwrap();
        assert_eq!(now_utc_minute_of_day(with_secs), 9 * 60 + 30);
    }
}
