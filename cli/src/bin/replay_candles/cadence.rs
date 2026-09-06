//! How many freshly-closed bars one replay tick hands to `evaluate_plan`.
//!
//! # Why this exists
//!
//! Live and replay drive the **same** engine with **different bar cadence**, and
//! that difference hides a whole class of bug.
//!
//! - **Live.** The cron's engine job takes ONE `Utc::now()` per tick
//!   (`worker/src/scheduler.rs:216`), fetches *every* bar closed since the
//!   plan's watermark (`trade-control-cron/src/engine.rs:156`,
//!   `filter_new_candles`), and hands the whole slice to a **single**
//!   `evaluate_plan` call (`engine.rs:181`) which loops it internally
//!   (`engine/src/evaluate.rs:284`). Every fired intent from that batch is then
//!   dispatched under that same `now` (`engine.rs:253`).
//! - **Replay.** One bar per `evaluate_plan` call, each with its own
//!   `now = candle.time + bar`.
//!
//! So any live tick that catches up over more than one bar — a gap, a worker
//! restart, a slow tick, the first tick after seeding — processes N bars under a
//! **single wall-clock instant**. Replay could never construct that state, so no
//! fixture was evidence about any timing-sensitive gate.
//!
//! That is not hypothetical. Preps used to be stamped with wall-clock `now`
//! (`core/src/dispatch/control.rs`) while the prep gate requires *strictly
//! increasing* prep timestamps (`core/src/intent/prep_req.rs`, `resolve_slot`).
//! A two-bar catch-up stamped `break-and-close` and `retest` identically and a
//! geometrically-correct entry was rejected `prep-order-violated`, forfeiting a
//! live trade (plan `hs-eur-cad-08ca0693`; both stored prep rows byte-identical
//! at `2026-08-08 00:07:05.759557+10`).
//!
//! # Measured: the corpus was blind to it
//!
//! Replaying the full 910-fixture corpus four ways, counting
//! `prep-order-violated`:
//!
//! | prep stamping | default cadence | `--cron-gap 6` |
//! |---|---|---|
//! | wall-clock `now` (**unfixed**) | **0** | **1002** |
//! | bar time (**fixed**) | 0 | 0 |
//!
//! The top-left cell is the whole point: with the bug *present*, the old
//! one-bar-per-tick replay reported zero occurrences over every fixture we
//! have. That was accidental immunity, not fidelity — no fixture was ever
//! evidence about a timing-sensitive gate. Batching makes the failure mode
//! reachable (1002 hits), and the fix drives it back to zero.
//!
//! # The model
//!
//! [`CronCadence`] is the number of bars one replay tick batches. The default,
//! [`CronCadence::PER_BAR`], is `1` — exactly today's behaviour, bar for bar, so
//! every existing fixture is byte-identical. A larger value reproduces a live
//! catch-up of that width.
//!
//! `now` for a batch is the **last** bar's close, matching live: the cron's
//! `Utc::now()` is taken at or after the newest closed bar it is about to
//! process, and every earlier bar in the batch shares it. That is also what
//! makes the batch clock deterministic — it is derived from the candle series,
//! never from `Utc::now()`, so a replay stays reproducible
//! (`[[wallclock_now_leaks_into_replayable_plans]]`).

/// The number of freshly-closed bars one replay tick hands to a single
/// `evaluate_plan` call, mirroring a live cron catch-up. See the module docs.
///
/// A newtype rather than a bare `usize` so it cannot be transposed with the
/// other numeric knobs threaded through the replay driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CronCadence(usize);

impl CronCadence {
    /// One bar per `evaluate_plan` call — the historical replay behaviour and
    /// the default. Every existing fixture must reproduce byte-identically
    /// under this.
    pub const PER_BAR: Self = Self(1);

    /// A cadence of `bars` bars per tick. Zero is meaningless (it would advance
    /// no bars and spin), so it is clamped up to `PER_BAR`.
    pub fn new(bars: usize) -> Self {
        Self(bars.max(1))
    }

    /// The batch width in bars. Always `>= 1`.
    pub fn bars(self) -> usize {
        self.0
    }

    /// True when this is the historical one-bar-per-tick cadence, i.e. the
    /// batching path is a pure no-op.
    pub fn is_per_bar(self) -> bool {
        self.0 == 1
    }

    /// The half-open bar range `[lo, hi)` this tick processes, given the live
    /// window's exclusive `end`. The final batch is short whenever the window
    /// doesn't divide evenly — exactly like a live catch-up that runs out of
    /// closed bars.
    pub fn batch_end(self, lo: usize, end: usize) -> usize {
        (lo + self.0).min(end)
    }
}

impl Default for CronCadence {
    fn default() -> Self {
        Self::PER_BAR
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_one_bar_per_tick() {
        // The whole no-op guarantee rests on this: absent the flag, every tick
        // is one bar wide and the replay walks the window exactly as before.
        assert_eq!(CronCadence::default(), CronCadence::PER_BAR);
        assert_eq!(CronCadence::default().bars(), 1);
        assert!(CronCadence::default().is_per_bar());
    }

    #[test]
    fn per_bar_batches_walk_one_index_at_a_time() {
        let c = CronCadence::PER_BAR;
        assert_eq!(c.batch_end(0, 5), 1);
        assert_eq!(c.batch_end(4, 5), 5);
    }

    #[test]
    fn a_wider_cadence_batches_that_many_bars() {
        let c = CronCadence::new(3);
        assert!(!c.is_per_bar());
        assert_eq!(c.batch_end(0, 10), 3);
        assert_eq!(c.batch_end(3, 10), 6);
    }

    #[test]
    fn the_final_batch_is_truncated_at_the_window_end() {
        // A live catch-up processes whatever bars actually closed; it does not
        // invent bars to round the batch out.
        let c = CronCadence::new(4);
        assert_eq!(c.batch_end(6, 8), 8);
    }

    #[test]
    fn zero_is_clamped_up_so_the_loop_cannot_stall() {
        // `--cron-gap 0` would otherwise advance no bars and spin forever.
        assert_eq!(CronCadence::new(0), CronCadence::PER_BAR);
        assert_eq!(CronCadence::new(0).batch_end(0, 5), 1);
    }

    #[test]
    fn walking_a_window_covers_every_bar_exactly_once() {
        for width in 1..=5 {
            let c = CronCadence::new(width);
            let end = 11;
            let mut lo = 0;
            let mut seen = Vec::new();
            while lo < end {
                let hi = c.batch_end(lo, end);
                assert!(hi > lo, "batch must advance");
                seen.extend(lo..hi);
                lo = hi;
            }
            assert_eq!(seen, (0..end).collect::<Vec<_>>(), "width {width}");
        }
    }
}
