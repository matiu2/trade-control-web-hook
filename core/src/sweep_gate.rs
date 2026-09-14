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
//! * price has **traded past** its stop-loss at any point since placement (the
//!   setup invalidated before it ever filled) — wick semantics, not a
//!   point-in-time reading. See [`update_adverse_extreme`].
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
    /// Price traded past the resting order's stop-loss before it filled — the
    /// setup invalidated before entry. Judged on the adverse extreme (live: the
    /// running one on the row; replay: the bar's), never on a point-in-time
    /// close/spot reading — see [`update_adverse_extreme`].
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
///
/// # What to FEED this (2026-09-14)
///
/// The predicate itself is correct and unchanged. What changed is the *price*
/// each caller hands it, because the rule is:
///
/// > **has price TRADED past the stop since placement** — wick semantics, over
/// > the whole life of the resting order.
///
/// Not "is spot past the stop *right now*". Feed this the **adverse extreme**:
///
/// * live — the running extreme persisted on the row
///   ([`EntryAttempt::adverse_extreme`](crate::state::EntryAttempt::adverse_extreme)),
///   maintained by [`update_adverse_extreme`];
/// * replay — the **bar's** adverse extreme (`l` for a Long, `h` for a Short).
///
/// See [`update_adverse_extreme`] for the full rationale; that doc is the one
/// place this decision is written down.
pub fn breach_detected(direction: Direction, current_price: f64, stop_loss: f64) -> bool {
    match direction {
        Direction::Long => current_price <= stop_loss,
        Direction::Short => current_price >= stop_loss,
    }
}

/// Fold one observed price into a resting order's **running adverse extreme** —
/// the worst price seen *against* the trade since the order was placed. Long
/// keeps the lowest, Short keeps the highest. `prior` is `None` for the first
/// observation (and for a legacy row that predates the field), which seeds from
/// `observed` — a missing extreme is **never** read as "breached".
///
/// # Why this exists — read before "simplifying" it away
///
/// The pre-fill SL-breach sweep cancels a resting entry order whose stop-loss
/// price has already overtaken it: the setup's thesis is falsified before we
/// ever got in, so entering now would mean entering a trade whose invalidation
/// level price has ALREADY proven it can reach.
///
/// The live sweep used to read `Broker::get_current_price` — an **instantaneous
/// spot quote**, sampled on the ~900s upkeep loop — and pass that straight to
/// [`breach_detected`]. That is not a rule, it is a sampling lottery: whether a
/// REAL order gets cancelled depended on where the cron tick happened to land
/// relative to the price path. Two identical price paths gave different answers,
/// and any excursion that opened and closed between two ticks was invisible.
///
/// Carrying the extreme on the persisted row makes the decision **monotonic**:
/// once the extreme is past the stop it stays past, so the sweep answers "has
/// price traded past the stop since placement" rather than "where is spot this
/// instant". That persistence is the whole point — it cannot be recovered by
/// re-reading the quote, which is why this is a stored field and not a
/// recomputation.
///
/// The replay's analogue is the **bar's** adverse extreme (low for a Long, high
/// for a Short). Close-sampling was explicitly **REJECTED**: it lets a bar trade
/// clean through the stop and back inside with the order surviving, which is the
/// exact case the rule exists to catch.
///
/// # The fixture corpus CANNOT justify this rule — do not "simplify" on green
///
/// Measured over the full 2847-cell corpus
/// (`EXPERIMENT-pre-fill-sl-breach-sweep.md`): close-mode truncation fired on
/// **854 orders** and changed the outcome of **ZERO** of them — disabling the
/// rule entirely was byte-identical. The reason is narrow and specific: the
/// sweep only changes an outcome when a breached order would *later* come back
/// through its trigger and fill, and inside an alert window that essentially
/// never happens. It is a no-op on *outcomes* in that corpus, **not** a no-op in
/// mechanism, and emphatically not evidence the rule is pointless. The goldens
/// were themselves recorded under close-sampling, so by construction they
/// contain almost no bar that wicked past the stop and closed back inside — no
/// fixture is evidence about a sampling rule the fixtures were generated under.
///
/// So: a green corpus after deleting this proves nothing. The justification is
/// the operator's thesis-falsification rationale above, plus live's per-tick
/// sampling reaching states no bar-grain replay can.
pub fn update_adverse_extreme(direction: Direction, prior: Option<f64>, observed: f64) -> f64 {
    match prior {
        None => observed,
        Some(prior) => match direction {
            Direction::Long => prior.min(observed),
            Direction::Short => prior.max(observed),
        },
    }
}

/// The **bar-resolution** analogue of the live running extreme: the worst price
/// a single bar traded at, against the trade. Long → the low, Short → the high.
///
/// This is what the replay feeds [`breach_detected`], so both sides answer the
/// same question ("did price trade past the stop") and differ only in
/// resolution. The mid book is deliberate — the live sweep's spot quote is a mid
/// quote, and the rule is about where the *market* went, not which book an order
/// would have filled on.
///
/// See [`update_adverse_extreme`] for why close-sampling was rejected.
pub fn bar_adverse_extreme(direction: Direction, high: f64, low: f64) -> f64 {
    match direction {
        Direction::Long => low,
        Direction::Short => high,
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

    // --- running adverse extreme -------------------------------------------

    /// A Long's adverse extreme is the LOWEST price seen. Mutation guard: a
    /// direction swap (`max` here) makes this red — the swapped version would
    /// track the favourable extreme and never breach.
    #[test]
    fn long_extreme_keeps_the_lowest_price_seen() {
        let d = Direction::Long;
        let e = update_adverse_extreme(d, None, 1.1000);
        assert_eq!(e, 1.1000, "the first observation seeds the extreme");
        let e = update_adverse_extreme(d, Some(e), 1.0900);
        assert_eq!(e, 1.0900, "a worse (lower) price advances a Long's extreme");
        let e = update_adverse_extreme(d, Some(e), 1.1200);
        assert_eq!(e, 1.0900, "a better price must NOT retract the extreme");
    }

    /// Mirror for Short: the HIGHEST price seen. Both signs are pinned because a
    /// direction-hardcoded implementation otherwise survives (a short's resting
    /// bars sit below its SL, so the wrong branch can still yield "not
    /// breached").
    #[test]
    fn short_extreme_keeps_the_highest_price_seen() {
        let d = Direction::Short;
        let e = update_adverse_extreme(d, None, 1.1000);
        assert_eq!(e, 1.1000);
        let e = update_adverse_extreme(d, Some(e), 1.1100);
        assert_eq!(
            e, 1.1100,
            "a worse (higher) price advances a Short's extreme"
        );
        let e = update_adverse_extreme(d, Some(e), 1.0800);
        assert_eq!(e, 1.1100, "a better price must NOT retract the extreme");
    }

    /// The whole point of persisting the extreme: an excursion past the stop
    /// that has since RECOVERED still reads as breached. An instantaneous spot
    /// read (the old behaviour) would say "not breached" here — that is the bug.
    #[test]
    fn a_recovered_excursion_still_reads_as_breached() {
        let (d, sl) = (Direction::Long, 1.0950);
        // Tick 1: well above the stop. Tick 2: an excursion through it.
        // Tick 3: fully recovered — spot alone says "fine".
        let e = update_adverse_extreme(d, None, 1.1000);
        let e = update_adverse_extreme(d, Some(e), 1.0900);
        let e = update_adverse_extreme(d, Some(e), 1.1050);
        assert!(
            breach_detected(d, e, sl),
            "the extreme is monotonic — a recovered excursion stays a breach",
        );
        assert!(
            !breach_detected(d, 1.1050, sl),
            "spot alone reads 'not breached' — this is precisely the divergence",
        );
    }

    /// A legacy row (no extreme yet) seeds from the observation and must NOT be
    /// treated as breached on the strength of its absence.
    #[test]
    fn a_missing_extreme_seeds_and_is_not_a_breach() {
        let (d, sl) = (Direction::Short, 1.1000);
        let e = update_adverse_extreme(d, None, 1.0900);
        assert_eq!(e, 1.0900);
        assert!(!breach_detected(d, e, sl));
    }

    /// The bar-grain analogue: low for a Long, high for a Short. Pinned both
    /// ways because a direction hardcode is the mutation that survives.
    #[test]
    fn bar_adverse_extreme_reads_the_wick_per_direction() {
        assert_eq!(bar_adverse_extreme(Direction::Long, 1.1200, 1.0800), 1.0800);
        assert_eq!(
            bar_adverse_extreme(Direction::Short, 1.1200, 1.0800),
            1.1200
        );
    }

    /// The close↔wick divergence in one assertion: a bar that trades through the
    /// stop and closes back inside. Wick semantics breach; close semantics do
    /// not. This is the case the rule exists for and the one close-sampling
    /// silently let through.
    #[test]
    fn a_bar_that_wicks_through_and_closes_back_is_a_breach() {
        let (d, sl) = (Direction::Long, 1.0950);
        let (high, low, close) = (1.1100, 1.0900, 1.1050);
        assert!(breach_detected(d, bar_adverse_extreme(d, high, low), sl));
        assert!(
            !breach_detected(d, close, sl),
            "close-sampling misses it — the rejected design",
        );
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
