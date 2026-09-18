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

use super::replay_broker::QuoteSample;

/// The on-disk form of an [`UpkeepTicks`] series (`upkeep_bars.json` in a
/// fixture): the finer bars plus their length, so a fixture that was saved
/// under `--upkeep` replays the SAME sub-bar ticks offline. `bar_seconds`
/// rather than a `Granularity` so the file is self-describing without the
/// CLI's parser.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrozenUpkeep {
    pub bar_seconds: i64,
    pub bars: Vec<BidAskCandle>,
}

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

    /// The series as saved into / loaded from a fixture.
    pub fn to_frozen(&self) -> FrozenUpkeep {
        FrozenUpkeep {
            bar_seconds: self.bar_len.num_seconds(),
            bars: self.bars.clone(),
        }
    }

    /// Rebuild from the fixture form (sorted + deduplicated like [`Self::new`]).
    pub fn from_frozen(frozen: FrozenUpkeep) -> Self {
        Self::new(frozen.bars, Duration::seconds(frozen.bar_seconds))
    }

    /// The quote at the INSTANT `at` — the open book of the finer bar that
    /// opens exactly then, or `None` when the series has no such bar.
    ///
    /// This is the entry-instant sample. Live, a plan bar closes and the cron
    /// dispatches the enter seconds later, so the spread `run_enter` sees is
    /// the first print AFTER the close — on the 17:00 New York grid that is the
    /// rollover spike, not the last print before it. A plan bar's own close
    /// book (`bid_c`/`ask_c`) is that last print: for TradeNation's aggregated
    /// D1 it is the calm 20:59 spread, so the spread gate could never trip
    /// offline and a park was unreachable. The finer bar OPENING at the close
    /// carries the first post-close print in its open book.
    pub fn opening_at(&self, at: DateTime<Utc>) -> Option<QuoteSample> {
        let i = self.bars.binary_search_by_key(&at, |c| c.time).ok()?;
        Some(QuoteSample::at_open(&self.bars[i]))
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

    /// The entry-instant sample is the OPEN book of the bar opening at `at`
    /// — not the close book of the bar closing there (which is the previous
    /// bar). A bar opening at 3600 with a wide open and calm close must report
    /// the wide open.
    #[test]
    fn opening_at_reads_the_open_book_of_the_bar_opening_then() {
        let mut wide_open = bar(3600);
        wide_open.bid_o = 0.9990;
        wide_open.ask_o = 1.0010;
        // Its close is calm; the PREVIOUS bar's close is calm too.
        let t = UpkeepTicks::new(vec![bar(0), wide_open, bar(7200)], Duration::hours(1));
        let q = t
            .opening_at(Utc.timestamp_opt(3600, 0).unwrap())
            .expect("a bar opens at 3600");
        assert_eq!(q.at, Utc.timestamp_opt(3600, 0).unwrap());
        assert!(
            (q.ask - q.bid - 0.0020).abs() < 1e-12,
            "the OPEN book, got {q:?}"
        );
        assert!(
            t.opening_at(Utc.timestamp_opt(1800, 0).unwrap()).is_none(),
            "no bar opens at 1800"
        );
    }

    /// `upkeep_bars.json` round-trips the series exactly, and rebuilding sorts
    /// + dedups like `new` so a hand-edited file can't smuggle a duplicate in.
    #[test]
    fn frozen_form_round_trips() {
        let t = UpkeepTicks::new(vec![bar(7200), bar(3600), bar(3600)], Duration::minutes(15));
        let json = serde_json::to_string(&t.to_frozen()).unwrap();
        let back: FrozenUpkeep = serde_json::from_str(&json).unwrap();
        assert_eq!(back.bar_seconds, 900);
        let rebuilt = UpkeepTicks::from_frozen(back);
        assert_eq!(rebuilt.len(), 2);
        assert_eq!(rebuilt.to_frozen(), t.to_frozen());
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
