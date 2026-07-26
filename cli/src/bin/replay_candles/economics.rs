//! The economic result of a simulated replay: what each taken position earned,
//! and the net R across the run.
//!
//! This is the **single** economic computation. Both consumers read it:
//!
//! - `report.rs` formats it into the human summary line (`Net R: +0.52 | …`).
//! - `fixture.rs` serializes it into `expected.json`, so a saved fixture records
//!   its own economic result instead of only which rules fired.
//!
//! ## Why it lives here and not in `report.rs`
//!
//! It used to be a private `Tally` inside `report.rs`, built during rendering
//! and thrown away — the Net R existed only as printed text. That left the
//! fixture snapshot to compute fills a *second*, independent way (`fill_for` →
//! `fill_sim::simulate_fill`), and the two paths diverged: `simulate_fill` has
//! no reversal- or expiry-close awareness, so `expected.json` could not
//! represent those outcomes at all. A regression that turned a reversal-close
//! into a 0R no-op fired the same rules and passed the golden gate.
//!
//! Both consumers now read the `ReplayBroker` held ledger (`fire.realized`) via
//! this module, so the printed report and the saved golden cannot disagree.
//!
//! ## Ordering matters
//!
//! Legs are booked in **fire order**, not exit-time order, because the account
//! compounds: each trade risks 1% of what the previous ones left. Reordering
//! changes the dollar figures (though not `net_r`). `report.rs` renders events
//! sorted by time, but books through here in fire order — keep that split.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::report::{FillKind, FireResult};

/// The account size a `--simulate` P&L projection compounds from: 1% risk per
/// taken trade against a fresh $100k. Every fill's R multiple grows or shrinks
/// this balance so the report shows what the sequence would have made on a
/// standard account, not just the raw R sum.
pub const START_ACCOUNT: f64 = 100_000.0;

/// Fraction of the *remaining* account risked on each taken trade (1%).
pub const RISK_FRACTION: f64 = 0.01;

/// How one taken position ended. The serialized form of [`FillKind`]'s *taken*
/// variants — the not-taken kinds (never-filled / declined / gate-blocked) book
/// no leg at all, so they have no representation here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// Hit the protected stop. A stop that break-even armed and then scratched
    /// exits at the entry price for 0R — still `StoppedOut`, R tells them apart.
    StoppedOut,
    TookProfit,
    /// Flattened by a `06-`/`07-close-on-…-reversal` fire before SL/TP.
    Reversal,
    /// Flattened by the trade-expiry `close-positions` veto at wall-clock expiry.
    Expiry,
    /// Still open when the replay window ended. Books 0R — the position has no
    /// realized result yet, so it must not be scored as a win or a loss.
    OpenAtWindowEnd,
}

impl ExitReason {
    /// The taken [`FillKind`]s map 1:1; the not-taken ones yield `None` and book
    /// no leg.
    pub fn from_fill_kind(kind: FillKind) -> Option<Self> {
        match kind {
            FillKind::StoppedOut => Some(Self::StoppedOut),
            FillKind::TookProfit => Some(Self::TookProfit),
            FillKind::ClosedOnReversal => Some(Self::Reversal),
            FillKind::ClosedAtExpiry => Some(Self::Expiry),
            FillKind::Open => Some(Self::OpenAtWindowEnd),
            FillKind::NeverFilled | FillKind::Declined | FillKind::GateBlocked => None,
        }
    }

    /// Does this outcome realize a P&L? `OpenAtWindowEnd` does not — it books a
    /// leg (so the corpus can see the position existed) but contributes 0R.
    pub fn is_realized(self) -> bool {
        !matches!(self, Self::OpenAtWindowEnd)
    }
}

/// One taken position's economics: where it got in, where it got out, and what
/// that was worth in R.
///
/// A leg is what makes an aggregate explainable — a trade's `+0.52R` net is
/// often several re-entries (`+0.35 / −1.00 / +1.18`), and that sequence is the
/// actual lesson a journal page teaches.
///
/// `exit_*` are `Option` because a position still open at the window end has no
/// exit yet. The struct holds **one** exit deliberately: partial/scaled exits
/// (take 50% at 80%-to-TP, the rest at TP) would book several legs against one
/// entry rather than growing this type — see `SCOPING-fixture-corpus.md` §6.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Leg {
    /// Open-time of the bar the entry filled on.
    pub entry_time: DateTime<Utc>,
    pub entry_price: f64,
    /// The **floored** stop the position actually rested on, from the ledger —
    /// this is the risk denominator, so it must not be the un-floored intent
    /// level (see `[[breakeven_armed_at_floor_divergence]]`).
    pub stop_loss: f64,
    pub take_profit: f64,
    /// Exit bar / price. `None` while still open at the window end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_time: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_price: Option<f64>,
    pub exit_reason: ExitReason,
    /// Realized R: `(exit − entry) / (entry − stop)`. `0.0` for a still-open
    /// position and for a degenerate zero-risk bracket.
    pub r: f64,
}

/// The economic result of one replay: every taken position, the net R, and the
/// compounding account.
///
/// Counts only move on *taken* fills; not-taken outcomes (never-filled,
/// declined, gate-blocked) contribute nothing and leave the balance untouched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayEconomics {
    /// Sum of every leg's R. The headline number.
    pub net_r: f64,
    pub tp_hits: usize,
    pub sl_hits: usize,
    pub reversal_closes: usize,
    pub expiry_closes: usize,
    /// Positions still open when the window ended (0R, not scored either way).
    #[serde(default)]
    pub open_at_end: usize,
    /// Per-position breakdown, in **fire order** (the order they were booked,
    /// which is the order the account compounds in).
    #[serde(default)]
    pub legs: Vec<Leg>,
}

impl Default for ReplayEconomics {
    fn default() -> Self {
        Self {
            net_r: 0.0,
            tp_hits: 0,
            sl_hits: 0,
            reversal_closes: 0,
            expiry_closes: 0,
            open_at_end: 0,
            legs: Vec::new(),
        }
    }
}

impl ReplayEconomics {
    pub fn new() -> Self {
        Self::default()
    }

    /// The $100k account compounded at 1% risk per taken trade, in leg order.
    ///
    /// **Derived, deliberately not stored.** It's a chain of multiply-accumulates
    /// (`acct += 0.01 · acct · R`) whose low bits depend on floating-point
    /// operation order, so two builds of the same code can land on
    /// `100337.04422788607` vs `...609`. `ReplayOutcome` equality is exact float
    /// comparison, so storing it made the golden fixture gate **flaky** — the
    /// test binary and the release binary disagreed in the last two digits.
    ///
    /// `net_r` is a plain sum and is bit-stable, so the legs plus `net_r` are the
    /// durable record; this is a presentation of them, recomputed on demand.
    pub fn account(&self) -> f64 {
        self.legs.iter().fold(START_ACCOUNT, |acct, leg| {
            acct + RISK_FRACTION * acct * leg.r
        })
    }

    /// Book one resolved fire. Not-taken outcomes are ignored (they have no
    /// position); taken ones append a leg, bump the matching counter, and
    /// compound the account.
    ///
    /// Returns the booked [`Leg`] so the caller can render it — `None` when the
    /// fire booked nothing.
    pub fn book(&mut self, result: &FireResult) -> Option<&Leg> {
        let reason = ExitReason::from_fill_kind(result.kind)?;
        match reason {
            ExitReason::TookProfit => self.tp_hits += 1,
            ExitReason::StoppedOut => self.sl_hits += 1,
            ExitReason::Reversal => self.reversal_closes += 1,
            ExitReason::Expiry => self.expiry_closes += 1,
            ExitReason::OpenAtWindowEnd => self.open_at_end += 1,
        }

        // A still-open position has no exit and books 0R; so does a closed one
        // whose ledger somehow carries no exit price. Resolve the exit ONCE here
        // — a `?` further down would bail out after the counter above had already
        // moved, leaving a counted outcome with no leg.
        let exit = result.exit_price.filter(|_| reason.is_realized());
        let r = exit
            .map(|e| realized_r(result.entry_price, result.stop_loss, e))
            .unwrap_or(0.0);
        self.net_r += r;

        self.legs.push(Leg {
            entry_time: result.fill_at,
            entry_price: result.entry_price,
            stop_loss: result.stop_loss,
            take_profit: result.take_profit,
            exit_time: exit.map(|_| result.until),
            exit_price: exit,
            exit_reason: reason,
            r,
        });
        self.legs.last()
    }

    /// Dollar P&L against the starting balance.
    pub fn profit(&self) -> f64 {
        self.account() - START_ACCOUNT
    }

    /// The trailing summary segment: net R and the compounded $100k-account P&L.
    pub fn summary_line(&self) -> String {
        format!(
            "  |  Net R: {:+.2}  |  $100k acct (1%/trade): ${:.0} ({:+.0})",
            self.net_r,
            self.account(),
            self.profit()
        )
    }
}

/// The realized R multiple of a taken fill: signed reward over risk. `entry −
/// stop_loss` is the risk (positive for a long, negative for a short), and
/// `exit − entry` is the reward with the trade's own sign, so the quotient is
/// `+1` on a clean TP and `−1` on a clean SL for *both* directions without a
/// direction branch. Returns `0.0` when the stop sits at the entry (a
/// degenerate/zero-risk bracket) so it can't divide by zero.
pub fn realized_r(entry: f64, stop_loss: f64, exit: f64) -> f64 {
    let risk = entry - stop_loss;
    if risk.abs() < f64::EPSILON {
        return 0.0;
    }
    (exit - entry) / risk
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use trade_control_core::intent::Direction;

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 18, hour, 0, 0).unwrap()
    }

    /// A long: entry 1.10, stop 1.09 (risk 0.01), exit at the given price.
    fn long_fire(kind: FillKind, exit: Option<f64>) -> FireResult {
        FireResult {
            direction: Direction::Long,
            fill_at: at(12),
            until: at(18),
            entry_price: 1.10,
            stop_loss: 1.09,
            take_profit: 1.12,
            exit_price: exit,
            kind,
        }
    }

    #[test]
    fn clean_tp_books_plus_one_r() {
        let mut e = ReplayEconomics::new();
        e.book(&long_fire(FillKind::TookProfit, Some(1.12)));
        assert_eq!(e.tp_hits, 1);
        assert!((e.net_r - 2.0).abs() < 1e-9, "net_r was {}", e.net_r);
        assert_eq!(e.legs.len(), 1);
        assert_eq!(e.legs[0].exit_reason, ExitReason::TookProfit);
    }

    #[test]
    fn clean_sl_books_minus_one_r() {
        let mut e = ReplayEconomics::new();
        e.book(&long_fire(FillKind::StoppedOut, Some(1.09)));
        assert_eq!(e.sl_hits, 1);
        assert!((e.net_r + 1.0).abs() < 1e-9, "net_r was {}", e.net_r);
        assert!(e.account() < START_ACCOUNT);
    }

    /// A short mirrors without a direction branch: entry 1.10, stop 1.11
    /// (risk −0.01), exit 1.09 → +1R.
    #[test]
    fn short_scores_without_a_direction_branch() {
        let mut e = ReplayEconomics::new();
        e.book(&FireResult {
            direction: Direction::Short,
            stop_loss: 1.11,
            exit_price: Some(1.09),
            ..long_fire(FillKind::TookProfit, None)
        });
        assert!((e.net_r - 1.0).abs() < 1e-9, "net_r was {}", e.net_r);
    }

    /// The divergence this module exists to fix: reversal- and expiry-closes are
    /// real booked outcomes, not 0R no-ops. The old `simulate_fill` path could
    /// not represent either.
    #[test]
    fn reversal_and_expiry_closes_book_r() {
        let mut e = ReplayEconomics::new();
        e.book(&long_fire(FillKind::ClosedOnReversal, Some(1.105)));
        e.book(&long_fire(FillKind::ClosedAtExpiry, Some(1.095)));
        assert_eq!(e.reversal_closes, 1);
        assert_eq!(e.expiry_closes, 1);
        // +0.5R then −0.5R.
        assert!(e.net_r.abs() < 1e-9, "net_r was {}", e.net_r);
        assert_eq!(e.legs.len(), 2);
    }

    /// A position still open at the window end is recorded but scores 0R — it
    /// must not be counted as a win or a loss.
    #[test]
    fn open_at_window_end_books_zero_r() {
        let mut e = ReplayEconomics::new();
        e.book(&long_fire(FillKind::Open, None));
        assert_eq!(e.open_at_end, 1);
        assert_eq!(e.tp_hits, 0);
        assert_eq!(e.sl_hits, 0);
        assert_eq!(e.net_r, 0.0);
        assert_eq!(e.account(), START_ACCOUNT);
        let leg = &e.legs[0];
        assert_eq!(leg.exit_reason, ExitReason::OpenAtWindowEnd);
        assert!(leg.exit_time.is_none());
        assert!(leg.exit_price.is_none());
    }

    /// Not-taken outcomes book nothing at all — no leg, no count, no balance move.
    #[test]
    fn not_taken_outcomes_book_nothing() {
        let mut e = ReplayEconomics::new();
        for kind in [
            FillKind::NeverFilled,
            FillKind::Declined,
            FillKind::GateBlocked,
        ] {
            assert!(e.book(&long_fire(kind, None)).is_none());
        }
        assert!(e.legs.is_empty());
        assert_eq!(e.net_r, 0.0);
        assert_eq!(e.account(), START_ACCOUNT);
    }

    /// A break-even scratch exits at the entry price: 0R, but it IS a stop-out.
    #[test]
    fn breakeven_scratch_is_a_stop_at_zero_r() {
        let mut e = ReplayEconomics::new();
        e.book(&long_fire(FillKind::StoppedOut, Some(1.10)));
        assert_eq!(e.sl_hits, 1);
        assert_eq!(e.net_r, 0.0);
        assert_eq!(e.account(), START_ACCOUNT);
    }

    /// A zero-risk bracket (stop at entry) can't divide by zero.
    #[test]
    fn degenerate_zero_risk_bracket_books_zero() {
        let mut e = ReplayEconomics::new();
        e.book(&FireResult {
            stop_loss: 1.10,
            exit_price: Some(1.15),
            ..long_fire(FillKind::TookProfit, None)
        });
        assert_eq!(e.net_r, 0.0);
    }

    /// The account compounds off the *running* balance: each trade risks 1% of
    /// what the previous ones left. Verified against hand arithmetic — this is
    /// the sequencing `report.rs` must preserve by booking in fire order.
    ///
    /// Note two trades of equal magnitude *commute* ((1+x)(1−y) = (1−y)(1+x)),
    /// so order-dependence needs unequal R — hence the +2R / +1R pair below.
    #[test]
    fn account_compounds_off_the_running_balance() {
        let mut e = ReplayEconomics::new();
        // +2R risking 1% of 100,000 → +$2,000 → 102,000.
        e.book(&long_fire(FillKind::TookProfit, Some(1.12)));
        assert!(
            (e.account() - 102_000.0).abs() < 1e-6,
            "got {}",
            e.account()
        );
        // −1R now risks 1% of 102,000 → −$1,020 → 100,980. A flat account would
        // have booked −$1,000; compounding off the running balance is the point.
        e.book(&long_fire(FillKind::StoppedOut, Some(1.09)));
        assert!(
            (e.account() - 100_980.0).abs() < 1e-6,
            "got {}",
            e.account()
        );
        assert!((e.net_r - 1.0).abs() < 1e-9);
        assert!((e.profit() - 980.0).abs() < 1e-6);
    }

    /// Reordering the same set of trades leaves BOTH `net_r` and the balance
    /// unchanged: net R is a plain sum, and the balance is a product of `(1 +
    /// 0.01·R)` factors, which commutes.
    ///
    /// Recorded deliberately, because it's easy to assume otherwise and write a
    /// test asserting the balance *must* differ (it doesn't). What booking order
    /// actually determines is the **per-leg dollar figures** the report prints
    /// mid-sequence — each leg's P&L is 1% of the balance at that moment — which
    /// is why `report.rs` books in fire order and only sorts for display.
    #[test]
    fn reordering_leaves_net_r_and_the_balance_unchanged() {
        let win = long_fire(FillKind::TookProfit, Some(1.12)); // +2R
        let loss = long_fire(FillKind::StoppedOut, Some(1.09)); // −1R

        let mut win_first = ReplayEconomics::new();
        win_first.book(&win); // → 102,000
        win_first.book(&loss); // −1% of 102,000 → 100,980

        let mut loss_first = ReplayEconomics::new();
        loss_first.book(&loss); // → 99,000
        loss_first.book(&win); // +2% of 99,000 → 100,980

        assert!((win_first.net_r - loss_first.net_r).abs() < 1e-9);
        assert!((win_first.account() - loss_first.account()).abs() < 1e-6);
        // The intermediate legs differ, though: −$1,020 booked after the win vs
        // −$1,000 booked first.
        assert!((win_first.legs[1].r + 1.0).abs() < 1e-9);
        assert!((loss_first.legs[0].r + 1.0).abs() < 1e-9);
    }

    /// The whole point of the extraction: the struct round-trips through JSON so
    /// `expected.json` can carry it (commit 2).
    #[test]
    fn economics_json_round_trips() {
        let mut e = ReplayEconomics::new();
        e.book(&long_fire(FillKind::TookProfit, Some(1.12)));
        e.book(&long_fire(FillKind::Open, None));
        let json = serde_json::to_string_pretty(&e).unwrap();
        let back: ReplayEconomics = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }
}
