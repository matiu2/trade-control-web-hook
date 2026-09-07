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

/// Serde default for [`StoredOrder::min_r`] — see the field docs for why this is
/// the R-floor and not `0.0`.
fn default_min_r() -> f64 {
    crate::intent::MIN_R_FLOOR
}

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
    /// Take-profit distance from entry, in price units — the numerator of the
    /// R the promotion re-check tests every candle.
    ///
    /// Defaults to `0.0` on a body written before this field existed. Paired
    /// with the `min_r` default below, that yields `R = 0.0 < 1.0` ⇒
    /// [`SlAction::BelowMinR`](super::SlAction::BelowMinR), so a legacy park
    /// **stays parked** and drops at its own deadline rather than being placed
    /// on geometry we can't read.
    #[serde(default)]
    pub tp_distance: f64,
    /// The trade's effective R-floor, so promotion applies the same threshold
    /// the entry gate rejected it against rather than a hardcoded 1.0.
    ///
    /// ⚠️ Defaults to [`MIN_R_FLOOR`], **not** to `0.0`. A `0.0` default is
    /// the tempting one and it fails *open*: with a legacy body's
    /// `tp_distance: 0.0` it gives `0.0 < 0.0 == false`, so `sl_target` reports
    /// the order as clearing its floor and the first tick after deploy promotes
    /// every parked order on unknown geometry. Pinned by
    /// `a_legacy_park_stays_parked_rather_than_promoting_blind`.
    #[serde(default = "default_min_r")]
    pub min_r: f64,
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
    /// Seconds per bar at the enter's granularity, so a per-bar re-check
    /// ([`StoredReason::rechecked_per_bar`]) can tell "a new bar has started"
    /// from "the same bar, a few seconds later" — the order-control loop ticks
    /// faster than a bar.
    ///
    /// `None` on a body written before this field existed, and on the webhook
    /// path which has no plan granularity. Read fail-closed: with no bar clock
    /// a size park keeps waiting rather than promoting on an unknown cadence,
    /// and still drops at its own `drop_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bar_seconds: Option<i64>,
}

impl StoredOrder {
    /// Has a signal bar strictly later than the one that parked this order
    /// begun?
    ///
    /// The comparison is by **bar bucket**, not raw instant: the order-control
    /// loop ticks several times within one bar, so `bar > shell_time` alone
    /// would read a tick 5 seconds into the same bar as "a new bar" whenever the
    /// caller passes `now` rather than an exact bar open. Both sides are floored
    /// to a multiple of [`Self::bar_seconds`] and compared as buckets.
    ///
    /// Fail-closed on anything unjudgeable — no bar in hand, no bar clock, or a
    /// non-positive one — because promoting on an unknown cadence is the failure
    /// this whole per-bar path exists to prevent.
    pub fn is_a_later_bar(&self, bar_time: Option<DateTime<Utc>>) -> bool {
        let (Some(bar), Some(secs)) = (bar_time, self.bar_seconds) else {
            return false;
        };
        if secs <= 0 {
            return false;
        }
        bucket(bar, secs) > bucket(self.shell_time, secs)
    }
}

/// Floor `t` to a multiple of `secs` since the epoch — which bar it falls in.
fn bucket(t: DateTime<Utc>, secs: i64) -> i64 {
    t.timestamp().div_euclid(secs)
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
    /// The broker's computed position size floored to zero — `EntryError::
    /// UnitsBelowMinimum`. A deterministic function of (equity, stop distance,
    /// contract multiplier), so unlike the two spread reasons above it cannot
    /// change within a bar; see [`StoredReason::rechecked_per_bar`].
    BelowMinSize,
}

impl StoredReason {
    /// A short, stable slug for logs and `status` output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BelowMinR => "below-min-r",
            Self::BelowMinRForecast => "below-min-r-forecast",
            Self::BelowMinSize => "below-min-size",
        }
    }

    /// Is this reason re-checked **once per bar** rather than every tick?
    ///
    /// The two spread reasons are re-asked every tick because the spread moves
    /// continuously and `sl_target` can genuinely answer "it has calmed" from a
    /// fresh quote. [`Self::BelowMinSize`] cannot: position size is a function
    /// of equity, stop distance and contract multiplier, none of which move
    /// within a bar, and the [`Broker`](crate::broker::Broker) trait exposes no
    /// equity to re-test against — sizing is private inside each broker's
    /// `place_entry` by design.
    ///
    /// So the only honest re-check is to try again on a **new signal bar**.
    /// Ticking a size-park on the spread gate instead would promote it on the
    /// next tick straight back into the same rejection — and because the
    /// order-control loop runs faster than a bar, that converts a once-per-bar
    /// retry into a once-per-few-seconds one: strictly worse than the bug the
    /// park exists to fix, while looking like a fix.
    pub fn rechecked_per_bar(self) -> bool {
        match self {
            Self::BelowMinR | Self::BelowMinRForecast => false,
            Self::BelowMinSize => true,
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

/// What the caller observed this tick, for [`stored_verdict`] to judge.
///
/// A named struct rather than two positional `bool`s: `clears_min_r` and
/// `bar_time` answer different questions for different reasons, and two adjacent
/// bare booleans transpose silently at a call site. Same discipline as
/// `InstrumentSizing` on the arm-time path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoredCheck {
    /// The caller's verdict from [`sl_target`](super::sl_target) against the
    /// **current** spread — passed in rather than recomputed so there is exactly
    /// one place the R decision is made, and so this stays pure. Read only for
    /// the spread reasons.
    pub clears_min_r: bool,
    /// The signal bar this tick is evaluating, when the caller knows it. Read
    /// only for [`StoredReason::rechecked_per_bar`] reasons, where promotion
    /// waits for a bar strictly newer than the one that parked the order.
    ///
    /// `None` means "no bar identity in hand" and is treated as **not** a new
    /// bar — a size-park keeps waiting rather than promoting blind, matching the
    /// fail-closed reading of a legacy park elsewhere in this module.
    pub bar_time: Option<DateTime<Utc>>,
}

/// Should this stored order be promoted, kept, or dropped?
///
/// Expiry is checked **first**: an order past its drop deadline is dropped even
/// if it would otherwise promote. Entering three bars before expiry is the thing
/// the deadline exists to prevent, and neither a calm spread nor a fresh bar
/// buys back the missing runway.
///
/// The promote question itself is **per-reason** — see
/// [`StoredReason::rechecked_per_bar`]. A spread park is re-asked every tick
/// (the spread genuinely moves); a size park is re-asked once per new signal
/// bar, because nothing it depends on can change faster than that.
pub fn stored_verdict(
    order: &StoredOrder,
    now: DateTime<Utc>,
    check: StoredCheck,
) -> StoredVerdict {
    if now >= order.drop_at {
        return StoredVerdict::Drop;
    }
    let promote = if order.reason.rechecked_per_bar() {
        order.is_a_later_bar(check.bar_time)
    } else {
        check.clears_min_r
    };
    if promote {
        StoredVerdict::Promote
    } else {
        StoredVerdict::KeepWaiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spread-reason check: `BelowMinR` reads only `clears_min_r`.
    fn spread_check(clears_min_r: bool) -> StoredCheck {
        StoredCheck {
            clears_min_r,
            bar_time: None,
        }
    }

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
            tp_distance: 0.0200,
            min_r: 1.0,
            stored_at: at(stored_at),
            drop_at: at(drop_at),
            shell_time: at(stored_at),
            bar_seconds: Some(3600),
        }
    }

    /// The sgdjpy shape: parked while the spread is wide, promoted when it
    /// calms. Today this trade is discarded three times for 0R.
    #[test]
    fn parks_while_sub_1r_then_promotes_when_the_spread_calms() {
        let o = order("2026-07-22T13:30:00Z", "2026-07-24T00:00:00Z");
        assert_eq!(
            stored_verdict(&o, at("2026-07-22T13:30:00Z"), spread_check(false)),
            StoredVerdict::KeepWaiting,
        );
        assert_eq!(
            stored_verdict(&o, at("2026-07-23T06:15:00Z"), spread_check(true)),
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
            stored_verdict(&o, at("2026-07-23T22:00:00Z"), spread_check(true)),
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
        assert_eq!(
            stored_verdict(&o, stored, spread_check(true)),
            StoredVerdict::Drop
        );
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

    /// BACK-COMPAT, and the reason `min_r` does **not** default to `0.0`.
    ///
    /// A body parked by the previous build carries neither `tp_distance` nor
    /// `min_r`. With a `0.0` default for both, `sl_target` computes `R = 0.0`
    /// and tests `0.0 < 0.0 == false` — so it reports the order as CLEARING its
    /// floor, and the first tick after deploy promotes every parked order on
    /// geometry it cannot actually read. Defaulting `min_r` to the R-floor makes
    /// the same body read as `0.0 < 1.0` ⇒ `BelowMinR`: it stays parked and
    /// drops at its own deadline.
    ///
    /// Mutation check: change the default back to `0.0` and this goes red.
    #[test]
    fn a_legacy_park_stays_parked_rather_than_promoting_blind() {
        // Exactly the fields the previous build wrote — no tp_distance, no min_r.
        let legacy = r#"{
            "signed_intent": "body",
            "reason": "below-min-r",
            "original_sl_distance": 0.0020,
            "stored_at": "2026-07-22T13:30:00Z",
            "drop_at": "2026-07-23T21:00:00Z",
            "shell_time": "2026-07-22T13:30:00Z"
        }"#;
        let o: StoredOrder = serde_json::from_str(legacy).expect("a legacy body must still decode");
        assert_eq!(o.tp_distance, 0.0, "no TP known");
        assert!(
            (o.min_r - crate::intent::MIN_R_FLOOR).abs() < 1e-12,
            "min_r must default to the R-floor, not 0.0 — got {}",
            o.min_r,
        );

        // ...and the verdict that follows from those defaults: do not promote.
        let verdict = crate::order_control::sl_target(
            crate::order_control::SpreadInputs::measured_only(0.0001),
            o.original_sl_distance,
            o.original_sl_distance,
            o.tp_distance,
            o.min_r,
        );
        assert_eq!(
            verdict.action,
            crate::order_control::SlAction::BelowMinR,
            "a park whose geometry we can't read must NOT be placed",
        );
    }

    // --- BelowMinSize: the per-bar re-check --------------------------------
    //
    // `UnitsBelowMinimum` is a deterministic function of (equity, stop distance,
    // contract multiplier). Re-asking it on the spread gate would promote it
    // straight back into the same rejection, and the order-control loop ticks
    // faster than a bar — so these pin the once-per-bar cadence.

    fn size_park(shell: &str, drop_at_s: &str) -> StoredOrder {
        StoredOrder {
            reason: StoredReason::BelowMinSize,
            shell_time: at(shell),
            drop_at: at(drop_at_s),
            stored_at: at(shell),
            bar_seconds: Some(3600),
            ..order(shell, drop_at_s)
        }
    }

    fn bar_check(bar: &str) -> StoredCheck {
        StoredCheck {
            // Deliberately TRUE: a size park has healthy geometry, so the spread
            // gate says "promote". If the verdict consulted it, every test below
            // would promote immediately — which is exactly the bug.
            clears_min_r: true,
            bar_time: Some(at(bar)),
        }
    }

    /// The regression that makes this a fix rather than an amplifier: a tick a
    /// few seconds into the SAME bar must not promote.
    ///
    /// Mutation check: make `BelowMinSize` read `clears_min_r` and this goes red.
    #[test]
    fn a_size_park_does_not_promote_within_the_same_bar() {
        let o = size_park("2026-07-22T13:00:00Z", "2026-07-24T00:00:00Z");
        for tick in [
            "2026-07-22T13:00:05Z",
            "2026-07-22T13:00:30Z",
            "2026-07-22T13:59:59Z",
        ] {
            assert_eq!(
                stored_verdict(&o, at(tick), bar_check(tick)),
                StoredVerdict::KeepWaiting,
                "tick {tick} is still inside the 13:00 bar",
            );
        }
    }

    /// ...and it DOES promote once a genuinely new bar opens, so the setup is
    /// retried rather than abandoned.
    #[test]
    fn a_size_park_promotes_on_the_next_bar() {
        let o = size_park("2026-07-22T13:00:00Z", "2026-07-24T00:00:00Z");
        assert_eq!(
            stored_verdict(
                &o,
                at("2026-07-22T14:00:00Z"),
                bar_check("2026-07-22T14:00:00Z"),
            ),
            StoredVerdict::Promote,
        );
    }

    /// Expiry still wins, so a size park can't retry forever either — the
    /// property the plain-`Failed` path lacked entirely.
    ///
    /// Mutation check: drop the `drop_at` check and this goes red.
    #[test]
    fn a_size_park_still_drops_at_its_deadline() {
        let o = size_park("2026-07-22T13:00:00Z", "2026-07-22T20:00:00Z");
        assert_eq!(
            stored_verdict(
                &o,
                at("2026-07-22T21:00:00Z"),
                bar_check("2026-07-22T21:00:00Z"),
            ),
            StoredVerdict::Drop,
            "past the deadline there is no runway left, however fresh the bar",
        );
    }

    /// No bar clock (a legacy body, or the webhook path) ⇒ keep waiting rather
    /// than promoting on an unknown cadence. Fail-closed, matching how a legacy
    /// park's unreadable geometry is treated above.
    #[test]
    fn a_size_park_without_a_bar_clock_keeps_waiting() {
        let mut o = size_park("2026-07-22T13:00:00Z", "2026-07-24T00:00:00Z");
        o.bar_seconds = None;
        assert_eq!(
            stored_verdict(
                &o,
                at("2026-07-23T13:00:00Z"),
                bar_check("2026-07-23T13:00:00Z"),
            ),
            StoredVerdict::KeepWaiting,
        );
        // ...and with no bar identity in hand at all.
        o.bar_seconds = Some(3600);
        assert_eq!(
            stored_verdict(
                &o,
                at("2026-07-23T13:00:00Z"),
                StoredCheck {
                    clears_min_r: true,
                    bar_time: None,
                },
            ),
            StoredVerdict::KeepWaiting,
        );
    }

    /// A spread park must NOT acquire the per-bar gate: its whole point is that
    /// the spread moves within a bar and is re-asked every tick.
    ///
    /// Mutation check: make `rechecked_per_bar` return true for `BelowMinR` and
    /// this goes red.
    #[test]
    fn a_spread_park_still_promotes_within_the_same_bar() {
        let o = order("2026-07-22T13:00:00Z", "2026-07-24T00:00:00Z");
        assert_eq!(o.reason, StoredReason::BelowMinR, "precondition");
        assert_eq!(
            stored_verdict(
                &o,
                at("2026-07-22T13:00:30Z"),
                StoredCheck {
                    clears_min_r: true,
                    bar_time: Some(at("2026-07-22T13:00:30Z")),
                },
            ),
            StoredVerdict::Promote,
            "the spread calmed mid-bar; that is a real signal and must be acted on",
        );
    }

    /// The slug is written into stored bodies and read back, so a disagreement
    /// between serde and `as_str` silently mis-labels a park.
    #[test]
    fn every_reason_round_trips_and_agrees_with_as_str() {
        for r in [
            StoredReason::BelowMinR,
            StoredReason::BelowMinRForecast,
            StoredReason::BelowMinSize,
        ] {
            let json = serde_json::to_string(&r).expect("serialises");
            assert_eq!(
                json,
                format!("\"{}\"", r.as_str()),
                "serde and as_str must agree for {r:?}",
            );
            let back: StoredReason = serde_json::from_str(&json).expect("round-trips");
            assert_eq!(back, r);
        }
    }

    /// Bucketing is by bar boundary, not elapsed time: 13:59 → 14:00 is one
    /// minute later but a genuinely new bar, while 13:00 → 13:59 is 59 minutes
    /// later and the same one.
    #[test]
    fn a_later_bar_is_measured_in_buckets_not_elapsed_time() {
        let o = size_park("2026-07-22T13:59:00Z", "2026-07-24T00:00:00Z");
        assert!(
            o.is_a_later_bar(Some(at("2026-07-22T14:00:00Z"))),
            "one minute later, but a new bar",
        );
        let o = size_park("2026-07-22T13:00:00Z", "2026-07-24T00:00:00Z");
        assert!(
            !o.is_a_later_bar(Some(at("2026-07-22T13:59:00Z"))),
            "59 minutes later, but the same bar",
        );
    }

    /// A non-positive bar clock is unjudgeable, not "every instant is a new
    /// bar" — which a naive `div_euclid` would panic on anyway.
    #[test]
    fn a_degenerate_bar_clock_is_unjudgeable() {
        let mut o = size_park("2026-07-22T13:00:00Z", "2026-07-24T00:00:00Z");
        for secs in [0, -3600] {
            o.bar_seconds = Some(secs);
            assert!(!o.is_a_later_bar(Some(at("2026-07-23T13:00:00Z"))));
        }
    }
}
