//! Bar-based pending-order expiry: turn a signed `expiry_bars` count plus
//! the Pine-filled forward bar-close menu on the shell into a concrete
//! `cancel_at` timestamp for the cron sweep.
//!
//! Why a menu instead of the worker doing the math: a resting order gets
//! no further webhooks, and only Pine knows the symbol's session calendar
//! — so "the bar N ahead" of a Friday close (which must skip the weekend
//! gap) is computed Pine-side via `time_close(timeframe.period,
//! bars_back=-N)` and shipped as `next_candle_timestamp_1..5`. This module
//! just selects slot N, caps it at `not_after`, and reports range errors.

use chrono::{DateTime, Utc};

use super::Shell;

/// The number of forward bar-close slots Pine ships
/// (`next_candle_timestamp_1..=MAX_EXPIRY_BARS`). `expiry_bars` must land
/// in `1..=MAX_EXPIRY_BARS`; anything else is a config error we refuse to
/// guess at.
pub const MAX_EXPIRY_BARS: u32 = 5;

/// Why a `cancel_at` couldn't be derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpiryError {
    /// `expiry_bars` was 0 or greater than [`MAX_EXPIRY_BARS`] — there is
    /// no menu slot to honour it, so the entry is rejected rather than
    /// silently clamped.
    OutOfRange { requested: u32, max: u32 },
}

impl core::fmt::Display for ExpiryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfRange { requested, max } => write!(
                f,
                "expiry_bars {requested} is out of range (must be 1..={max})"
            ),
        }
    }
}

impl std::error::Error for ExpiryError {}

/// Derive the pending-order `cancel_at` for a resolved `expiry_bars`.
///
/// - `expiry_bars` outside `1..=MAX_EXPIRY_BARS` → [`ExpiryError::OutOfRange`]
///   (the caller rejects the entry; it must NOT mark the intent id seen).
/// - The selected `next_candle_timestamp_N` is missing (`na` on a
///   non-time chart, or simply absent) → fall back to `not_after` so the
///   order still gets the existing alert-window bound rather than no
///   expiry at all.
/// - Otherwise → `min(menu[expiry_bars], not_after)`: the bar-expiry,
///   never allowed to outlive the alert window.
pub fn resolve_cancel_at(
    expiry_bars: u32,
    shell: &Shell,
    not_after: DateTime<Utc>,
) -> Result<DateTime<Utc>, ExpiryError> {
    if expiry_bars == 0 || expiry_bars > MAX_EXPIRY_BARS {
        return Err(ExpiryError::OutOfRange {
            requested: expiry_bars,
            max: MAX_EXPIRY_BARS,
        });
    }
    let picked = shell
        .next_candle_timestamp(expiry_bars)
        .unwrap_or(not_after);
    Ok(picked.min(not_after))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// A shell whose menu is filled with hourly forward bar-closes from
    /// 13:00Z..17:00Z. `not_after` defaults well past the menu unless a
    /// test overrides it.
    fn shell_with_menu() -> Shell {
        Shell {
            close: 1.1,
            high: 1.1,
            low: 1.1,
            open: None,
            time: ts("2026-05-13T12:00:00Z"),
            signal_high: None,
            signal_low: None,
            signal_range: None,
            signal_start_time: None,
            signal_kind: None,
            band_anchor: None,
            golden: None,
            atr: None,
            signal_confirmed: None,
            recent_high: None,
            recent_low: None,
            next_candle_timestamp_1: Some(ts("2026-05-13T13:00:00Z")),
            next_candle_timestamp_2: Some(ts("2026-05-13T14:00:00Z")),
            next_candle_timestamp_3: Some(ts("2026-05-13T15:00:00Z")),
            next_candle_timestamp_4: Some(ts("2026-05-13T16:00:00Z")),
            next_candle_timestamp_5: Some(ts("2026-05-13T17:00:00Z")),
        }
    }

    #[test]
    fn picks_the_requested_slot() {
        let shell = shell_with_menu();
        let not_after = ts("2026-05-13T20:00:00Z");
        let got = resolve_cancel_at(3, &shell, not_after).unwrap();
        assert_eq!(got, ts("2026-05-13T15:00:00Z"));
    }

    #[test]
    fn zero_is_out_of_range() {
        let shell = shell_with_menu();
        let not_after = ts("2026-05-13T20:00:00Z");
        assert_eq!(
            resolve_cancel_at(0, &shell, not_after),
            Err(ExpiryError::OutOfRange {
                requested: 0,
                max: MAX_EXPIRY_BARS
            })
        );
    }

    #[test]
    fn above_menu_is_out_of_range() {
        let shell = shell_with_menu();
        let not_after = ts("2026-05-13T20:00:00Z");
        assert_eq!(
            resolve_cancel_at(8, &shell, not_after),
            Err(ExpiryError::OutOfRange {
                requested: 8,
                max: MAX_EXPIRY_BARS
            })
        );
    }

    #[test]
    fn missing_slot_falls_back_to_not_after() {
        let mut shell = shell_with_menu();
        shell.next_candle_timestamp_2 = None; // Pine shipped na for slot 2.
        let not_after = ts("2026-05-13T20:00:00Z");
        let got = resolve_cancel_at(2, &shell, not_after).unwrap();
        assert_eq!(got, not_after);
    }

    #[test]
    fn capped_at_not_after() {
        let shell = shell_with_menu();
        // not_after sits before the slot-5 timestamp — cap wins.
        let not_after = ts("2026-05-13T16:30:00Z");
        let got = resolve_cancel_at(5, &shell, not_after).unwrap();
        assert_eq!(got, not_after);
    }
}
