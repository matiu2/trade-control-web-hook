//! Market-hours entry blackout — the pure window + policy types.
//!
//! # The incident this fixes
//!
//! A resting stop order placed on a US-index *rolling future* sat unfilled
//! through the whole closed session, then triggered on the next open's gap and
//! was stopped out. The thesis was fine — the trade was killed purely by a
//! resting order surviving the close→open gap.
//!
//! The fix is a **market-hours entry blackout**: a recurring daily,
//! per-instrument window during which the worker (a) **rejects new entries**
//! and (b) **acts on the instrument's still-pending resting order** per an
//! operator-chosen [`BlackoutCloseAction`].
//!
//! # What lives here vs. elsewhere
//!
//! This module is **pure data + a pure predicate**, with no I/O. It holds:
//!
//! - [`NoEntryWindow`] — a resolved, daily-recurring **UTC** window, stored as
//!   open/close minute-of-day so it can be compared to `now` with no timezone
//!   math in the (WASM) worker. A daily cron derives it from the broker's
//!   `market_info` feed (which is already DST-correct for the current season)
//!   and writes it to KV; the worker only ever *reads* and compares.
//! - [`BlackoutCloseAction`] — the operator's per-trade choice of what to do
//!   with the *resting order* when the window opens. Carried as a signed field
//!   on the [`Intent`](crate::intent::Intent), the single field both the
//!   webhook enter path and the engine's fired enter share.
//! - [`is_inside_window`] — the pure `now ∈ window?` test, handling the
//!   midnight-wrap (a window like 21:00→01:00) and degenerate (open == close)
//!   cases.
//!
//! The KV get/set, the daily cron that derives the window, and the reject gate
//! / sweep that consume it live in the worker crate (they need `worker::Env`).
//! Keeping the decision logic here keeps it unit-testable off-wasm.

use serde::{Deserialize, Serialize};

/// Number of minutes in a day — the modulus for a minute-of-day clock.
pub const MINUTES_PER_DAY: u32 = 24 * 60;

/// What the worker does with an instrument's **resting order** when the
/// market-hours blackout window opens. Chosen by the operator at arm time and
/// signed onto the enter [`Intent`](crate::intent::Intent), so it can't be
/// flipped in flight.
///
/// Both variants **always** cancel the unfilled resting order — the difference
/// is whether an *already-filled* position is also closed:
///
/// - [`CancelResting`](Self::CancelResting) (**default**, the incident fix):
///   cancel only the unfilled stop/limit order; leave any filled position
///   alone (its own SL protects it). This variant must **never** close a
///   position — see the `veto_close_only_when_thesis_invalidated` rule.
/// - [`CancelAndClose`](Self::CancelAndClose): also market-close an open
///   position so nothing is held across the close→open gap.
///
/// `#[serde(default)]` → `CancelResting`, so intents minted before this field
/// existed deserialize to the safe incident-fix behaviour and the wire form
/// stays byte-identical to pre-feature intents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlackoutCloseAction {
    /// Cancel the unfilled resting order only; leave any open position. Default.
    #[default]
    CancelResting,
    /// Cancel the resting order **and** market-close an open position.
    CancelAndClose,
}

/// A resolved, daily-recurring market-hours entry blackout window, in **UTC**.
///
/// Stored as open/close **minute-of-day** (0..[`MINUTES_PER_DAY`)) rather than
/// absolute timestamps so the worker compares it to `now` with a single
/// modular-arithmetic test and zero timezone handling. The daily cron resolves
/// the broker's current-season session hours into this UTC pair (so DST is
/// baked in by the feed, not computed in the worker) and writes it to KV.
///
/// A window may **wrap midnight** (e.g. close 21:00 UTC → reopen 01:00 UTC the
/// next day gives a blackout window `[18:00 … 02:00]` after buffers), in which
/// case `open_min > close_min`. [`is_inside_window`] handles both orientations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoEntryWindow {
    /// Window start, UTC minute-of-day (0..1440). Entry is blocked from here.
    pub open_min: u32,
    /// Window end, UTC minute-of-day (0..1440). Entry resumes at/after here.
    pub close_min: u32,
}

impl NoEntryWindow {
    /// Build a window from UTC minute-of-day bounds, wrapping each into the
    /// valid `0..MINUTES_PER_DAY` range so a caller's `+buffer` arithmetic that
    /// overshoots a day boundary normalises cleanly (e.g. `1380 + 120 = 1500`
    /// → `60`).
    pub fn new(open_min: u32, close_min: u32) -> Self {
        Self {
            open_min: open_min % MINUTES_PER_DAY,
            close_min: close_min % MINUTES_PER_DAY,
        }
    }

    /// True when the window spans midnight (`open_min > close_min`), i.e. the
    /// blackout starts late one day and ends early the next.
    pub fn wraps_midnight(&self) -> bool {
        self.open_min > self.close_min
    }
}

/// Pure `now ∈ window?` test. `now_min` is the current UTC minute-of-day.
///
/// Semantics:
/// - **Half-open** `[open, close)` — blocked at `open_min`, allowed again at
///   `close_min`. (Symmetric with the buffers: a +1h after-open buffer means
///   the window *ends* exactly one hour after the open, and entry is allowed at
///   that minute.)
/// - **Midnight wrap** (`open_min > close_min`): inside when `now >= open` OR
///   `now < close`.
/// - **Degenerate** `open_min == close_min`: an **empty** window — never
///   inside. A zero-width window must not block all day; a genuine all-day
///   blackout is not something this feature ever writes (24h / no-range
///   sessions write *no* window at all — see the cron derivation).
pub fn is_inside_window(now_min: u32, w: &NoEntryWindow) -> bool {
    let now = now_min % MINUTES_PER_DAY;
    if w.open_min == w.close_min {
        // Degenerate / empty — never inside.
        false
    } else if w.wraps_midnight() {
        now >= w.open_min || now < w.close_min
    } else {
        now >= w.open_min && now < w.close_min
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- BlackoutCloseAction ------------------------------------------------

    #[test]
    fn close_action_defaults_to_cancel_resting() {
        assert_eq!(
            BlackoutCloseAction::default(),
            BlackoutCloseAction::CancelResting
        );
    }

    #[test]
    fn close_action_wire_form_is_snake_case() {
        assert_eq!(
            serde_yaml::to_string(&BlackoutCloseAction::CancelResting)
                .unwrap()
                .trim(),
            "cancel_resting"
        );
        assert_eq!(
            serde_yaml::to_string(&BlackoutCloseAction::CancelAndClose)
                .unwrap()
                .trim(),
            "cancel_and_close"
        );
    }

    #[test]
    fn close_action_round_trips() {
        for a in [
            BlackoutCloseAction::CancelResting,
            BlackoutCloseAction::CancelAndClose,
        ] {
            let s = serde_yaml::to_string(&a).unwrap();
            let back: BlackoutCloseAction = serde_yaml::from_str(&s).unwrap();
            assert_eq!(back, a);
        }
    }

    // --- NoEntryWindow construction -----------------------------------------

    #[test]
    fn new_normalises_overshoot_into_day() {
        // 23:00 + 2h buffer = 1500 min → wraps to 60 (01:00).
        let w = NoEntryWindow::new(1500, 2000);
        assert_eq!(w.open_min, 60);
        assert_eq!(w.close_min, 2000 % MINUTES_PER_DAY);
    }

    #[test]
    fn wraps_midnight_detects_orientation() {
        // 18:00 → 02:00 wraps.
        assert!(NoEntryWindow::new(18 * 60, 2 * 60).wraps_midnight());
        // 02:00 → 06:00 does not.
        assert!(!NoEntryWindow::new(2 * 60, 6 * 60).wraps_midnight());
        // Equal bounds: not "wrapping" (open is not > close).
        assert!(!NoEntryWindow::new(120, 120).wraps_midnight());
    }

    // --- is_inside_window: non-wrapping -------------------------------------

    #[test]
    fn non_wrapping_window_blocks_inside_allows_outside() {
        // Blackout 09:00 → 17:00 UTC.
        let w = NoEntryWindow::new(9 * 60, 17 * 60);
        assert!(
            !is_inside_window(8 * 60 + 59, &w),
            "just before open: allowed"
        );
        assert!(
            is_inside_window(9 * 60, &w),
            "at open: blocked (half-open lower)"
        );
        assert!(is_inside_window(12 * 60, &w), "mid-window: blocked");
        assert!(
            is_inside_window(16 * 60 + 59, &w),
            "just before close: blocked"
        );
        assert!(
            !is_inside_window(17 * 60, &w),
            "at close: allowed (half-open upper)"
        );
        assert!(!is_inside_window(18 * 60, &w), "after close: allowed");
    }

    // --- is_inside_window: midnight wrap ------------------------------------

    #[test]
    fn wrapping_window_blocks_across_midnight() {
        // Blackout 18:00 → 02:00 UTC (the rolling-future close→open shape).
        let w = NoEntryWindow::new(18 * 60, 2 * 60);
        assert!(w.wraps_midnight());
        assert!(!is_inside_window(17 * 60 + 59, &w), "before open: allowed");
        assert!(is_inside_window(18 * 60, &w), "at open: blocked");
        assert!(is_inside_window(23 * 60, &w), "late evening: blocked");
        assert!(is_inside_window(0, &w), "midnight: blocked");
        assert!(
            is_inside_window(60 + 59, &w),
            "just before close (01:59): blocked"
        );
        assert!(!is_inside_window(2 * 60, &w), "at close: allowed");
        assert!(!is_inside_window(6 * 60, &w), "morning: allowed");
    }

    // --- is_inside_window: degenerate ---------------------------------------

    #[test]
    fn degenerate_window_is_never_inside() {
        // open == close → empty window, must never block (no all-day blackout).
        let w = NoEntryWindow::new(13 * 60, 13 * 60);
        assert!(
            !is_inside_window(13 * 60, &w),
            "at the point itself: allowed"
        );
        assert!(!is_inside_window(0, &w));
        assert!(!is_inside_window(12 * 60, &w));
        assert!(!is_inside_window(23 * 60 + 59, &w));
    }

    #[test]
    fn now_min_is_taken_modulo_day() {
        // A caller passing an un-normalised minute (e.g. minute-of-week math)
        // still compares correctly.
        let w = NoEntryWindow::new(9 * 60, 17 * 60);
        assert!(
            is_inside_window(MINUTES_PER_DAY + 12 * 60, &w),
            "1440+12:00 == 12:00"
        );
    }
}
