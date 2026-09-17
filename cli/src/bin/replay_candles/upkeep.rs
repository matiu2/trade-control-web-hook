//! The **upkeep ticks**: the replay's copy of the live scheduler's 900 s
//! order-control cadence (job 2).
//!
//! # Why
//!
//! Live runs `order_control_tick::run_both` (promote, then re-price) every
//! `upkeep_secs` = 900 s (`worker/src/scheduler.rs`) off a live `get_quote`,
//! and every `Adjust` cancels-and-replaces through `run_enter` at a fresh 1 %
//! size. The replay ran the SAME two shared passes once per plan bar, off that
//! bar's CLOSE spread. On a daily plan the close is the 17:00-NY rollover print
//! — the widest print of the day — so offline a widened stop was never given
//! back when the spread calmed and a Stored (below-min-R) order was never
//! promoted. Live shrinks within one or two ticks of the reopen hour ending.
//! Every D1 fixture's R was therefore unreliable in the pessimistic direction.
//!
//! # What this is
//!
//! [`UpkeepTicks`] is a finer bid/ask series (M15 ideally — one bar per live
//! tick — or H1 as a first cut on D1) over the live window. Between two plan
//! bar closes the replay loop asks [`UpkeepTicks::samples_in`] for the finer
//! bars that CLOSED strictly inside that span and, for each, points the
//! broker's quote at that bar ([`ReplayBroker::set_upkeep_sample`]) and calls
//! the shared `promote_due_orders` + `reprice_due_orders` with `now` = the
//! sample's close. Only the clock and the quote source differ from the per-bar
//! pass: no replay-local sizing, no replay-local widen/shrink decision.
//!
//! Both span ends are excluded on purpose: the per-bar pass at the previous
//! close already sampled `open`, and the per-bar pass at this close will sample
//! `close`. A sample landing on either would double-run a tick.
//!
//! `None` (no series) is the default and is byte-identical to before.
//!
//! [`ReplayBroker::set_upkeep_sample`]: super::replay_broker::ReplayBroker::set_upkeep_sample

use chrono::{DateTime, Duration, Utc};
use trade_control_core::broker::BidAskCandle;

/// A finer bid/ask series whose bar CLOSES are the instants the replay runs
/// its upkeep (order-control) ticks at.
#[derive(Debug, Clone)]
pub struct UpkeepTicks {
    /// Ascending finer bars. Bar times are OPEN times, as everywhere.
    bars: Vec<BidAskCandle>,
    /// One finer bar's length — a bar's close is `time + bar_len`.
    bar_len: Duration,
}

impl UpkeepTicks {
    /// Sort `bars` ascending and drop exact-duplicate open times (candle-cache
    /// range reads can hand back a bar twice across a chunk seam).
    pub fn new(mut bars: Vec<BidAskCandle>, bar_len: Duration) -> Self {
        bars.sort_by_key(|c| c.time);
        bars.dedup_by_key(|c| c.time);
        Self { bars, bar_len }
    }

    /// How many finer bars the series holds.
    pub fn len(&self) -> usize {
        self.bars.len()
    }

    /// True when the series holds no bars at all.
    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }

    /// The finer bars whose CLOSE (`time + bar_len`) lies strictly inside
    /// `(open, close)`, ascending, each paired with that close: one upkeep
    /// tick per item, the close being the tick's `now` and the bar's
    /// `bid_c`/`ask_c` its quote.
    pub fn samples_in(
        &self,
        open: DateTime<Utc>,
        close: DateTime<Utc>,
    ) -> impl Iterator<Item = (DateTime<Utc>, &BidAskCandle)> {
        let bar_len = self.bar_len;
        self.bars
            .iter()
            .map(move |c| (c.time + bar_len, c))
            .filter(move |(at, _)| *at > open && *at < close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn bar(epoch: i64) -> BidAskCandle {
        let p = 1.0;
        BidAskCandle {
            time: Utc.timestamp_opt(epoch, 0).unwrap(),
            o: p,
            h: p,
            l: p,
            c: p,
            bid_o: p,
            bid_h: p,
            bid_l: p,
            bid_c: p,
            ask_o: p,
            ask_h: p,
            ask_l: p,
            ask_c: p,
        }
    }

    /// The sampled bars' OPEN epochs, plus a check that each paired instant is
    /// that bar's close.
    fn closes(it: impl Iterator<Item = (DateTime<Utc>, &'static BidAskCandle)>) -> Vec<i64> {
        it.map(|(at, c)| {
            assert_eq!(
                at,
                c.time + Duration::hours(1),
                "tick instant is the bar close"
            );
            c.time.timestamp()
        })
        .collect()
    }

    #[test]
    fn samples_are_the_bars_closing_strictly_inside_the_span() {
        // H1 ticks under a D1 bar opening at 0 and closing at 86400.
        let bars: Vec<_> = (0..30).map(|h| bar(h * 3600)).collect();
        let t = Box::leak(Box::new(UpkeepTicks::new(bars, Duration::hours(1))));
        let got = closes(t.samples_in(
            Utc.timestamp_opt(0, 0).unwrap(),
            Utc.timestamp_opt(86400, 0).unwrap(),
        ));
        // Bar opening at 0 closes at 1h → the first tick inside the day.
        // Bar opening at 82800 (23h) closes AT `close` → excluded (that instant
        // is this bar's per-bar pass). Bars from 24h on close outside.
        let want: Vec<i64> = (0..23).map(|h| h * 3600).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn samples_come_back_ascending_and_deduplicated_from_unsorted_input() {
        let bars = vec![bar(7200), bar(3600), bar(3600), bar(10800)];
        let t = Box::leak(Box::new(UpkeepTicks::new(bars, Duration::hours(1))));
        assert_eq!(t.len(), 3);
        let got = closes(t.samples_in(
            Utc.timestamp_opt(0, 0).unwrap(),
            Utc.timestamp_opt(86400, 0).unwrap(),
        ));
        assert_eq!(got, vec![3600, 7200, 10800]);
    }

    #[test]
    fn empty_series_yields_no_ticks() {
        let t = UpkeepTicks::new(Vec::new(), Duration::minutes(15));
        assert!(t.is_empty());
        assert_eq!(
            t.samples_in(
                Utc.timestamp_opt(0, 0).unwrap(),
                Utc.timestamp_opt(86400, 0).unwrap()
            )
            .count(),
            0
        );
    }
}
