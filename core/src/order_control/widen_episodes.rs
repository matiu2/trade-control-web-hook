//! Which widened stop is in force on a given bar — across **every** spread-hour
//! episode a position lives through, not just the first.
//!
//! # The bug this exists to make unrepresentable
//!
//! System 2 widens an open position's stop away from price for the duration of a
//! learned spread hour, then restores it once the spread recovers. A position
//! open for days crosses **many** such hours, so that is a *sequence* of
//! widen→restore episodes.
//!
//! The live cron models the sequence correctly, but by accident of shape rather
//! than by design: it re-evaluates every tick, guards on an `applied` record so
//! it can't double-widen inside one episode, and **clears that record on
//! restore** (`blackout_watch`), which frees the next hour to widen again.
//!
//! The replay's fill simulator did not. It scanned for a widen and `return`ed the
//! first one it found, so a trade got **one** shielded episode and every later
//! spread hour ran with the un-widened stop. That is a replay↔live divergence in
//! the direction that silently *under-protects* the replay — fixtures book losses
//! on trades the live worker would have carried through.
//!
//! Measured on the AUD/NZD 2026-06-11 strategy-v2 fixture: widen at 06-12T20:00Z,
//! restore at 06-14T22:00Z, then the 06-14T21:00Z NY-close bar (18-pip spread)
//! got no widen at all. The stop stayed at 1.20733 and `bid_l = 1.20645` took it
//! out for **−1.00R**. The widened level, 1.20552, is not reached by *any* bar in
//! the window — with it the trade runs to TP for **+1.18R**. A 2.18R swing on one
//! position, invisible because the fixture looked like an ordinary stop-out.
//!
//! # Why the sequence lives here
//!
//! Because "one episode" was expressible. An `Option<SpreadWiden>` cannot say
//! "widened, restored, widened again", so the simulator could not have been right
//! no matter how carefully it was written — the type had already lost the
//! information. [`WidenEpisodes`] is a sequence, so the second episode is
//! representable, and [`WidenEpisodes::stop_on_bar`] is the single place a bar is
//! matched to the stop in force on it.
//!
//! Both halves call this: the replay reconstructs episodes from the candle path,
//! and the live side has the same question to answer whenever it reasons about a
//! historical position. Keeping it in `core` is the standing rule for anything
//! whose disagreement would show up as a fixture that quietly contradicts
//! production (`[[strategy_changes_in_both_replayer_and_worker]]`).

use chrono::{DateTime, Utc};

/// One widen→restore episode: the stop was moved away from price at
/// [`effective_from`](WidenEpisode::effective_from) and put back at
/// [`restored_at`](WidenEpisode::restored_at).
///
/// The half-open interval `[effective_from, restored_at)` is deliberate: the
/// restore bar itself is **not** shielded. The live cron restores the broker stop
/// at the top of that bar, so the position trades that bar on the narrow stop —
/// modelling it as shielded would over-protect the replay, which is the same
/// class of lie as the under-protection this module fixes, just pointing the
/// other way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WidenEpisode {
    /// Open-time of the bar from which the widened stop governs the exit.
    pub effective_from: DateTime<Utc>,
    /// Open-time of the bar at which the original stop is restored, or `None`
    /// when the widen is still in force at the end of the window.
    pub restored_at: Option<DateTime<Utc>>,
    /// The stop after widening away from price.
    pub widened_stop: f64,
}

impl WidenEpisode {
    /// Is this episode in force on the bar opening at `bar`?
    ///
    /// Half-open: `effective_from` is shielded, `restored_at` is not.
    pub fn covers(&self, bar: DateTime<Utc>) -> bool {
        bar >= self.effective_from && self.restored_at.is_none_or(|r| bar < r)
    }
}

/// Every widen episode a position lives through, in chronological order.
///
/// Empty when the position never crossed a spread hour — the common case, and
/// the reason [`stop_on_bar`](Self::stop_on_bar) takes the unshielded stop as an
/// argument rather than this type carrying one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WidenEpisodes {
    episodes: Vec<WidenEpisode>,
}

impl WidenEpisodes {
    /// Build from episodes already in chronological order.
    pub fn new(episodes: Vec<WidenEpisode>) -> Self {
        Self { episodes }
    }

    /// No spread hour was ever crossed.
    pub fn none() -> Self {
        Self::default()
    }

    /// The episodes, in order.
    pub fn as_slice(&self) -> &[WidenEpisode] {
        &self.episodes
    }

    /// How many episodes — the count that was structurally stuck at ≤1 before.
    pub fn len(&self) -> usize {
        self.episodes.len()
    }

    /// Did the position never cross a spread hour?
    pub fn is_empty(&self) -> bool {
        self.episodes.is_empty()
    }

    /// The first episode, for callers that report the widen in a journal line.
    ///
    /// Deliberately **not** how the exit is scored — that is
    /// [`stop_on_bar`](Self::stop_on_bar). A caller that scores the exit off this
    /// is reintroducing the one-shot bug.
    pub fn first(&self) -> Option<&WidenEpisode> {
        self.episodes.first()
    }

    /// The stop the broker actually holds on the bar opening at `bar`:
    /// the widened stop of whichever episode covers it, else `unshielded`
    /// (the break-even-managed or original stop).
    ///
    /// When episodes overlap — which they shouldn't, but a reconstruction from a
    /// noisy candle path could produce it — the **first** match wins, so a
    /// duplicate can never tighten the stop below an active shield.
    pub fn stop_on_bar(&self, bar: DateTime<Utc>, unshielded: f64) -> f64 {
        self.episodes
            .iter()
            .find(|e| e.covers(bar))
            .map_or(unshielded, |e| e.widened_stop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .unwrap_or_else(|e| panic!("bad test timestamp {s}: {e}"))
            .with_timezone(&Utc)
    }

    /// An episode shields from its first bar up to, but excluding, the restore
    /// bar. The restore bar trades on the narrow stop, because that is when the
    /// live cron has already amended it back.
    #[test]
    fn an_episode_is_half_open_shielded_at_the_start_bare_at_the_restore() {
        let e = WidenEpisode {
            effective_from: t("2026-06-12T20:00:00Z"),
            restored_at: Some(t("2026-06-12T23:00:00Z")),
            widened_stop: 1.20552,
        };
        assert!(!e.covers(t("2026-06-12T19:00:00Z")), "before the widen");
        assert!(e.covers(t("2026-06-12T20:00:00Z")), "the widen bar itself");
        assert!(e.covers(t("2026-06-12T22:00:00Z")), "mid-episode");
        assert!(
            !e.covers(t("2026-06-12T23:00:00Z")),
            "the restore bar is NOT shielded"
        );
    }

    /// An unrestored episode shields everything after it.
    #[test]
    fn an_unrestored_episode_shields_to_the_end_of_the_window() {
        let e = WidenEpisode {
            effective_from: t("2026-06-12T20:00:00Z"),
            restored_at: None,
            widened_stop: 1.20552,
        };
        assert!(e.covers(t("2026-06-12T20:00:00Z")));
        assert!(e.covers(t("2026-06-30T00:00:00Z")));
    }

    /// The regression this module exists for, in miniature: a bar sitting in a
    /// SECOND spread hour, after the first has restored, must be shielded.
    ///
    /// Under the old `Option<SpreadWiden>` the second episode could not be
    /// represented, so this bar fell through to the narrow stop.
    #[test]
    fn a_second_spread_hour_is_shielded_not_left_bare() {
        let eps = WidenEpisodes::new(vec![
            WidenEpisode {
                effective_from: t("2026-06-12T20:00:00Z"),
                restored_at: Some(t("2026-06-12T23:00:00Z")),
                widened_stop: 1.20500,
            },
            WidenEpisode {
                effective_from: t("2026-06-14T21:00:00Z"),
                restored_at: Some(t("2026-06-14T22:00:00Z")),
                widened_stop: 1.20552,
            },
        ]);
        let narrow = 1.20733;
        assert_eq!(
            eps.stop_on_bar(t("2026-06-14T21:00:00Z"), narrow),
            1.20552,
            "the second spread hour must widen too — this is the -1.00R bug"
        );
        // Between the episodes the narrow stop is correct.
        assert_eq!(eps.stop_on_bar(t("2026-06-13T12:00:00Z"), narrow), narrow);
        // And after the second restores.
        assert_eq!(eps.stop_on_bar(t("2026-06-14T22:00:00Z"), narrow), narrow);
    }

    /// The exact AUD/NZD 2026-06-11 regression, expressed as the thing that
    /// actually went wrong: **where the episode's restore lands**.
    ///
    /// The bug was the 12h safety backstop force-restoring onto the 21:00Z
    /// NY-close bar itself (a weekend gap aged the wall-clock timer out with no
    /// market in between). Because the interval is half-open, the restore bar runs
    /// on the *narrow* stop — and `bid_l = 1.20645` took out 1.20733 for −1.00R.
    /// Held one bar longer the widened 1.20552 is never touched and the trade runs
    /// to TP.
    ///
    /// So the assertion is about the two candidate restore points, not about
    /// comparing literals: restoring ON the spike exposes the position, restoring
    /// after it does not. `order_control::backstop_restore_allowed` is what pushes
    /// it to the later bar.
    #[test]
    fn a_restore_landing_on_the_spike_bar_exposes_it_one_bar_later_does_not() {
        const NARROW: f64 = 1.20733;
        const WIDENED: f64 = 1.20552;
        let spike_bar = t("2026-06-14T21:00:00Z");
        let after_spike = t("2026-06-14T22:00:00Z");
        let widened_at = t("2026-06-12T20:00:00Z");

        let episode = |restored_at| {
            WidenEpisodes::new(vec![WidenEpisode {
                effective_from: widened_at,
                restored_at: Some(restored_at),
                widened_stop: WIDENED,
            }])
        };

        // The buggy restore: the spike bar is the restore bar, so it is NOT
        // shielded and the position sits on the narrow stop through the spike.
        assert_eq!(
            episode(spike_bar).stop_on_bar(spike_bar, NARROW),
            NARROW,
            "restoring ON the spike bar leaves it bare — the -1.00R"
        );
        // The fixed restore: pushed past the spread hour, so the spike bar keeps
        // its shield.
        assert_eq!(
            episode(after_spike).stop_on_bar(spike_bar, NARROW),
            WIDENED,
            "restoring after the spike keeps the shield on — the +R"
        );
    }

    /// With no episodes the unshielded stop always applies — the common path.
    #[test]
    fn no_episodes_means_the_unshielded_stop_governs_every_bar() {
        let eps = WidenEpisodes::none();
        assert!(eps.is_empty());
        assert_eq!(eps.len(), 0);
        assert_eq!(eps.stop_on_bar(t("2026-06-14T21:00:00Z"), 1.20733), 1.20733);
    }

    /// Overlapping episodes resolve to the first match, so a duplicated
    /// reconstruction can never tighten the stop under an active shield.
    #[test]
    fn overlapping_episodes_resolve_to_the_first_never_to_the_tighter_stop() {
        let eps = WidenEpisodes::new(vec![
            WidenEpisode {
                effective_from: t("2026-06-12T20:00:00Z"),
                restored_at: Some(t("2026-06-12T23:00:00Z")),
                widened_stop: 1.20500,
            },
            WidenEpisode {
                effective_from: t("2026-06-12T21:00:00Z"),
                restored_at: Some(t("2026-06-12T22:00:00Z")),
                widened_stop: 1.20600,
            },
        ]);
        assert_eq!(
            eps.stop_on_bar(t("2026-06-12T21:00:00Z"), 1.20733),
            1.20500,
            "first match wins; an overlap must not narrow an active shield"
        );
    }

    /// `first()` is for journal display only; scoring an exit off it is the
    /// one-shot bug. Pin that it reports the FIRST episode, not the one in force.
    #[test]
    fn first_reports_the_earliest_episode_not_the_one_in_force() {
        let eps = WidenEpisodes::new(vec![
            WidenEpisode {
                effective_from: t("2026-06-12T20:00:00Z"),
                restored_at: Some(t("2026-06-12T23:00:00Z")),
                widened_stop: 1.20500,
            },
            WidenEpisode {
                effective_from: t("2026-06-14T21:00:00Z"),
                restored_at: None,
                widened_stop: 1.20552,
            },
        ]);
        assert_eq!(eps.len(), 2);
        assert_eq!(
            eps.first().map(|e| e.widened_stop),
            Some(1.20500),
            "first() is the earliest episode"
        );
        assert_eq!(
            eps.stop_on_bar(t("2026-06-14T21:00:00Z"), 1.20733),
            1.20552,
            "but the stop in force on a later bar is the LATER episode's"
        );
    }
}
