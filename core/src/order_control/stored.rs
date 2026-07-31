//! **Stored** orders — intended, but deliberately not sent to the broker yet.
//!
//! # The loss this exists to stop
//!
//! `replay-fixtures/sgdjpy-spread-floor-min-r-block` is the motivating case:
//!
//! > three `05-enter` fires (13:30, 14:30, next-day 06:15), **each independently
//! > rejected** by `sl-widen-below-min-r`, nothing remembered between them, plan
//! > dead at trade-expiry. `net_r: 0.0`, `legs: []`.
//!
//! The spread was wide at 13:30 — a genuine reason not to place *then*. But the
//! setup was **thrown away**, not parked, and that was never a considered policy:
//! a rejection simply leaves no trace. [`EntryAttempt::broker_order_id`] is a
//! non-`Option` `String` written only inside the `Ok(order_id)` arm, so **there
//! is no schema slot for an intended-but-unplaced order**. The only thing
//! resembling a retry is that the seen-id isn't poisoned, so an identical signal
//! on a later candle re-runs the whole chain from scratch — which is why the
//! 17-hour-later fire re-derived the same verdict and died the same way.
//!
//! A [`StoredOrder`] is that missing slot. The trade is parked with its geometry
//! and its signed body intact, re-checked every candle, and promoted the moment
//! the spread calms enough for it to clear its R-floor.
//!
//! # What Stored is *not*
//!
//! - **Not at risk.** It lives only in our DB; the broker has never heard of it.
//! - **Not a retry slot.** Parking is not an attempt, so it must never burn a
//!   `max_retries` placement (see [`crate::retry_gate`]). Promotion is the first
//!   attempt; a supersede is a *replacement*, not an increment.
//! - **Not immortal.** It expires 3 bars before the trade's own expiry
//!   ([`StoredOrder::drop_at`]), so a stale setup can't fire into the last
//!   moments of its window when there's no room left for the thesis to play out.
//!
//! # Lifecycle
//!
//! ```text
//!   enter fires, sub-1R ──► STORED ──(spread calms, >=1R)──► PENDING ──► LIVE
//!                              │
//!                              ├──(a fresher signal arrives)──► superseded
//!                              └──(3 bars before expiry)──────► dropped
//! ```

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// How many bars before a trade's expiry a stored order stops being promotable.
///
/// A setup promoted into the last moments of its own window has no room left to
/// work: it would enter, then almost immediately hit trade-expiry and be closed
/// for whatever the market happened to be doing. Three bars is the operator's
/// call — 45 minutes on M15, 3 hours on H1.
pub const DROP_BARS_BEFORE_EXPIRY: i64 = 3;

/// An order we intend to place but have deliberately not sent to the broker.
///
/// Carries everything needed to re-drive the entry later without re-deriving it
/// from a fresh alert: the signed body (so the whole verified chain re-runs
/// unchanged), the geometry as originally drawn, and the clocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredOrder {
    /// The whole signed Intent JSON, persisted so the order can be re-driven
    /// through the normal entry path. Opaque here — the caller re-parses it,
    /// exactly as [`crate::state::CancelledOrder`] does, to avoid an `Intent`
    /// dependency cycle in the state module.
    pub signed_intent: String,
    /// Why this was parked rather than placed, for the operator-facing log.
    pub reason: StoredReason,
    /// The stop distance the trade was **drawn** with, in price units. Promotion
    /// must never place a stop tighter than this — it is the operator's level,
    /// not a computed one. Kept separately from whatever widened distance the
    /// floor currently demands, which moves with the spread.
    pub original_sl_distance: f64,
    /// When this was first parked. Distinct from the trade's own clocks so a
    /// long park is visible in `status`.
    pub stored_at: DateTime<Utc>,
    /// Hard stop on promotion: [`DROP_BARS_BEFORE_EXPIRY`] bars before the
    /// trade's expiry. Past this the order is dropped with a log line rather
    /// than placed. See [`drop_at`].
    pub drop_at: DateTime<Utc>,
    /// The firing bar's `shell.time` for the fire that parked this. Lets a
    /// later fire recognise itself as a *fresher* signal for the same setup and
    /// supersede rather than duplicate.
    pub shell_time: DateTime<Utc>,
}

/// Why an order is Stored rather than Pending.
///
/// A closed enum, not a string: every reason here is a condition some part of
/// the system decided, so an unrecognised one in a stored body should be a loud
/// decode error rather than a silently-dropped park — the same reasoning as
/// [`crate::hold::HoldReason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoredReason {
    /// The spread floor forced a stop so wide the trade fell under its R-floor.
    /// This is the `sl-widen-below-min-r` case — the sgdjpy loss.
    BelowMinR,
    /// The *forecast* spread for the coming hour would push the trade under its
    /// R-floor, even though the measured spread right now would not. The
    /// synthetic pre-check (`sl_target` fed the expected spread) — this is what
    /// replaces the boolean spread-hour gate, parking per-trade instead of
    /// suppressing per-instrument-hour.
    BelowMinRForecast,
}

impl StoredReason {
    /// A short, stable slug for logs and `status` output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BelowMinR => "below-min-r",
            Self::BelowMinRForecast => "below-min-r-forecast",
        }
    }
}

/// The instant a stored order stops being promotable: [`DROP_BARS_BEFORE_EXPIRY`]
/// bars before `expiry`.
///
/// Clamped at `stored_at`, so a trade whose window is already shorter than three
/// bars yields a `drop_at` that is simply "now" rather than a time in the past —
/// the order is then dropped on its next evaluation instead of being promotable
/// forever via a negative comparison.
pub fn drop_at(expiry: DateTime<Utc>, bar_seconds: i64, stored_at: DateTime<Utc>) -> DateTime<Utc> {
    let lead = Duration::seconds(bar_seconds.max(0) * DROP_BARS_BEFORE_EXPIRY);
    (expiry - lead).max(stored_at)
}

/// What to do with a stored order on this candle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StoredVerdict {
    /// Place it now — the floor is satisfied and the trade clears its R-floor.
    Promote,
    /// Keep waiting; re-check next candle.
    KeepWaiting,
    /// Too close to expiry to be worth entering. Drop it with a log line.
    Drop,
}

/// Should this stored order be promoted, kept, or dropped?
///
/// `clears_min_r` is the caller's verdict from
/// [`sl_target`](super::sl_target) — passed in rather than recomputed so there
/// is exactly one place the R decision is made, and so this stays pure.
///
/// Expiry is checked **first**: an order past its drop deadline is dropped even
/// if the spread has calmed and it would otherwise promote. Entering three bars
/// before expiry is the thing the deadline exists to prevent, and a calm spread
/// doesn't buy back the missing runway.
pub fn stored_verdict(
    order: &StoredOrder,
    now: DateTime<Utc>,
    clears_min_r: bool,
) -> StoredVerdict {
    if now >= order.drop_at {
        return StoredVerdict::Drop;
    }
    if clears_min_r {
        StoredVerdict::Promote
    } else {
        StoredVerdict::KeepWaiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid test timestamp")
            .with_timezone(&Utc)
    }

    fn order(stored_at: &str, drop_at: &str) -> StoredOrder {
        StoredOrder {
            signed_intent: "{}".to_string(),
            reason: StoredReason::BelowMinR,
            original_sl_distance: 0.0020,
            stored_at: at(stored_at),
            drop_at: at(drop_at),
            shell_time: at(stored_at),
        }
    }

    /// The sgdjpy shape: parked while the spread is wide, promoted when it
    /// calms. Today this trade is discarded three times for 0R.
    #[test]
    fn parks_while_sub_1r_then_promotes_when_the_spread_calms() {
        let o = order("2026-07-22T13:30:00Z", "2026-07-24T00:00:00Z");
        assert_eq!(
            stored_verdict(&o, at("2026-07-22T13:30:00Z"), false),
            StoredVerdict::KeepWaiting,
        );
        assert_eq!(
            stored_verdict(&o, at("2026-07-23T06:15:00Z"), true),
            StoredVerdict::Promote,
            "the later fire that today re-derives the same reject and dies",
        );
    }

    /// Expiry beats a calm spread: an order past its deadline is dropped even
    /// when it would otherwise promote.
    ///
    /// Mutation check: move the expiry check below the `clears_min_r` branch
    /// and this goes red.
    #[test]
    fn expiry_wins_over_a_promotable_spread() {
        let o = order("2026-07-22T13:30:00Z", "2026-07-23T21:00:00Z");
        assert_eq!(
            stored_verdict(&o, at("2026-07-23T22:00:00Z"), true),
            StoredVerdict::Drop,
            "past the deadline there is no runway left, however calm the spread",
        );
    }

    #[test]
    fn drop_at_is_three_bars_before_expiry() {
        let stored = at("2026-07-22T00:00:00Z");
        let expiry = at("2026-07-23T00:00:00Z");
        // H1 bars → 3h of lead.
        assert_eq!(drop_at(expiry, 3600, stored), at("2026-07-22T21:00:00Z"));
        // M15 bars → 45m of lead (23:15 on the 22nd, not the 23rd).
        assert_eq!(drop_at(expiry, 900, stored), at("2026-07-22T23:15:00Z"));
    }

    /// A window shorter than three bars must not produce a `drop_at` in the
    /// past — that would read as "already expired" via a negative comparison
    /// and could never be reasoned about cleanly.
    #[test]
    fn drop_at_never_precedes_stored_at() {
        let stored = at("2026-07-22T12:00:00Z");
        let expiry = at("2026-07-22T13:00:00Z"); // only 1 H1 bar of window
        assert_eq!(drop_at(expiry, 3600, stored), stored);
        // ...and such an order is dropped on its very next evaluation.
        let o = StoredOrder {
            drop_at: drop_at(expiry, 3600, stored),
            ..order("2026-07-22T12:00:00Z", "2026-07-22T12:00:00Z")
        };
        assert_eq!(stored_verdict(&o, stored, true), StoredVerdict::Drop);
    }

    /// Reasons round-trip as stable kebab-case slugs, and an unrecognised one
    /// is a hard decode error rather than a silently-dropped park.
    #[test]
    fn reason_serialises_as_a_stable_slug() {
        let json = serde_json::to_string(&StoredReason::BelowMinRForecast).expect("serialise");
        assert_eq!(json, "\"below-min-r-forecast\"");
        assert_eq!(
            serde_json::from_str::<StoredReason>("\"below-min-r\"").expect("decode"),
            StoredReason::BelowMinR,
        );
        assert!(
            serde_json::from_str::<StoredReason>("\"who-knows\"").is_err(),
            "an unrecognised reason must be loud, never silently dropped",
        );
    }

    /// The whole record round-trips through `jsonb` unchanged — it is persisted
    /// as one body on `HeldTradeRecord`, so serde is the schema.
    #[test]
    fn stored_order_round_trips() {
        let o = order("2026-07-22T13:30:00Z", "2026-07-23T21:00:00Z");
        let json = serde_json::to_string(&o).expect("serialise");
        let back: StoredOrder = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(o, back);
    }
}
