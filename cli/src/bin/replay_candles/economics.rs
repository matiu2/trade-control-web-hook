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

/// `|R|` above which a booked leg is warned about as implausible.
///
/// A take-profit is baked at arm time, so a fill can gap past it but not by
/// orders of magnitude; the realistic ceiling on a runner is tens of R, not
/// hundreds. `100` is well clear of any genuine trade while still catching the
/// shape that actually occurs — a scaled or corrupt candle turning a 1R bracket
/// into 990R, which would swamp a 291-trade aggregate on its own.
///
/// This is a **reporting** threshold only. Nothing is clamped or excluded; see
/// the warning in [`ReplayEconomics::book`] for why.
pub const IMPLAUSIBLE_R: f64 = 100.0;

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
    /// Flattened by the structure-invalidation `close-positions` veto (`too-low`
    /// for a long / `too-high` for a short) — the thesis died, the clock didn't
    /// run out. Kept separate from [`Self::Expiry`] so "the setup broke" and "we
    /// ran out of time" read as the different lessons they are.
    Invalidation,
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
            FillKind::ClosedOnInvalidation => Some(Self::Invalidation),
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
    /// Positions flattened by the structure-invalidation `close-positions` veto.
    /// `#[serde(default)]` so fixtures blessed before the counter existed still
    /// deserialize (they recorded these under `expiry_closes`).
    #[serde(default)]
    pub invalidation_closes: usize,
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
            invalidation_closes: 0,
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
    /// (`acct += 0.01 · acct · R`), and `ReplayOutcome` equality is *exact* float
    /// comparison, so storing it made the golden fixture gate **flaky**: the test
    /// binary and the release binary disagreed in the last two digits
    /// (`100337.04422788607` vs `...609`), most likely from FMA contraction or
    /// other codegen differences between profiles.
    ///
    /// **What this does NOT claim:** that `net_r` is bit-stable and `account`
    /// isn't. An earlier version of this comment said exactly that, and it was
    /// wrong. Float addition is not associative, so a plain sum is the canonical
    /// order-dependent reduction — reordering the *real* uk-100 fixture legs
    /// (`+0.549 / −1.000 / +0.797`) through a left fold yields **two** distinct
    /// bit patterns for `net_r`. (Beware checking this in Python: its builtin
    /// `sum` is smarter than a naive fold and shows one pattern; Rust's
    /// `Sum for f64` is a plain left fold.)
    ///
    /// So `net_r` and `legs[].r` — both still stored — carry the *same* residual
    /// exposure. Removing `account` narrowed the surface (it was the longest
    /// dependent chain, and the only value observed to actually differ across
    /// profiles) but did not eliminate the class.
    ///
    /// **It flaked again, and the prescribed fix is now built.** On 2026-07-30 the
    /// EUR/USD 2026-07-22 corpus failed `--check` on a leg's `stop_loss` differing
    /// by **2 ULP** between the capture and check paths (`net_r` identical). The
    /// comparison is no longer `==`: both the `--check` gate and the
    /// `all_fixtures_match_expected` test go through
    /// [`super::golden_eq::outcome_matches`], which compares stored floats with a
    /// relative tolerance and everything structural exactly. Bit-exact float
    /// equality on this snapshot is a bug — don't reinstate it, and don't reach for
    /// rounding-on-write instead (`golden_eq`'s module doc says why not).
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
            ExitReason::Invalidation => self.invalidation_closes += 1,
            ExitReason::OpenAtWindowEnd => self.open_at_end += 1,
        }

        // A still-open position has no exit and books 0R; so does a closed one
        // whose ledger somehow carries no exit price. Resolve the exit ONCE here
        // — a `?` further down would bail out after the counter above had already
        // moved, leaving a counted outcome with no leg.
        //
        // A NON-FINITE exit is treated as no exit at all, not merely as 0R.
        // `realized_r` already refuses to return NaN, but the price itself lands
        // on the leg — and `serde_json` writes `NaN`/`±inf` as `null`, so storing
        // it verbatim produces an `expected.json` that can never be read back
        // (`invalid type: null, expected f64`). Dropping it keeps the golden
        // loadable and, because `exit_time` is derived from the same `Option`,
        // keeps the leg self-consistent: no exit price, no exit time.
        let exit = result
            .exit_price
            .filter(|p| p.is_finite())
            .filter(|_| reason.is_realized());
        let r = exit
            .map(|e| realized_r(result.entry_price, result.stop_loss, e))
            .unwrap_or(0.0);
        // An implausible R is almost always a data glitch (a 10×-scaled candle,
        // a bad tick), and one such leg can swamp a whole batch's net R — at 291
        // trades nobody will spot it by eye.
        //
        // Deliberately NOT clamped: the number stays exactly what was measured.
        // Clamping would silently rewrite a real measurement and hide the glitch
        // that produced it, which is the failure mode this corpus exists to avoid
        // (see `[[no_silent_degrade_prefer_loud_failure]]`). A warning surfaces it
        // while keeping the arithmetic honest — a genuine 60R runner is possible,
        // if rare, and must not be quietly truncated to fit a guess about limits.
        if r.abs() > IMPLAUSIBLE_R {
            tracing::warn!(
                r,
                entry = result.entry_price,
                stop_loss = result.stop_loss,
                exit = ?exit,
                reason = ?reason,
                "implausible R booked (|R| > {IMPLAUSIBLE_R}) — suspect a scaled or \
                 corrupt candle; the value is recorded as measured, NOT clamped"
            );
        }
        self.net_r += r;

        self.legs.push(Leg {
            entry_time: result.fill_at,
            // Same reasoning for the bracket prices: these are non-optional
            // fields, so a non-finite one has no `null`-free representation at
            // all. Substituting 0.0 keeps the golden loadable and is visibly
            // wrong to a reader, rather than silently unloadable. `r` is already
            // 0.0 in that case (`realized_r` guards every input), so the
            // economics don't move.
            entry_price: finite_or_zero(result.entry_price),
            stop_loss: finite_or_zero(result.stop_loss),
            take_profit: finite_or_zero(result.take_profit),
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
/// direction branch.
///
/// Returns `0.0` for a degenerate/zero-risk bracket (stop at or adjacent to
/// entry) so it can't divide by zero or report an absurd R.
///
/// The guard is **relative to the price**, not absolute. It used to be
/// `risk.abs() < f64::EPSILON` — 2.2e-16 flat — which is meaningless at index
/// scale: on UK100 at ~10500 the gap between adjacent representable doubles is
/// already ~1.8e-12, so a stop one float-step from entry passed the guard and
/// produced R ≈ 10¹⁰, which would then swamp a whole batch's net R. Upstream
/// floors (`min_r >= 1.0`, the 10×-spread SL floor) mean such a bracket shouldn't
/// reach here at all, but this guard's entire job is to be the last line of
/// defence, so it should hold at every price scale.
///
/// **The result is always finite.** Every input is checked, not just `risk`:
/// a non-finite `exit` used to sail through and yield `NaN`, which poisons
/// `net_r` and every downstream `.sum()`. Worse, it doesn't fail loudly —
/// `serde_json` serializes `NaN`/`±inf` as `null` rather than erroring, so
/// `--save`/`--rebless` would write an `expected.json` that can **never be
/// loaded again** (`invalid type: null, expected f64`), and the resulting
/// failure classifies as exit 4 "bad input" — blaming the operator for bytes
/// this tool wrote. A garbage price is worth 0R, not an unloadable corpus entry.
/// A price that is safe to serialize into a golden: `0.0` if it isn't finite.
///
/// `serde_json` writes `NaN`/`±inf` as `null` instead of erroring, and a `null`
/// in a non-`Option` field makes the whole `expected.json` unloadable forever
/// (`invalid type: null, expected f64`) — a corpus entry that can only be
/// deleted, not read. `0.0` is obviously-wrong-on-sight, which is what you want
/// from a value that should never have existed.
fn finite_or_zero(price: f64) -> f64 {
    if price.is_finite() { price } else { 0.0 }
}

pub fn realized_r(entry: f64, stop_loss: f64, exit: f64) -> f64 {
    let risk = entry - stop_loss;
    // ~1e-9 of the price: far below any real tick (the finest FX pip is 1e-5, and
    // a fractional-pip tick 1e-6) yet far above the ULP at index scale.
    let floor = (entry.abs() * 1e-9).max(f64::MIN_POSITIVE);
    // The explicit `exit` check is redundant with the `r.is_finite()` catch-all
    // below (a non-finite exit can only produce a non-finite quotient), so
    // deleting either one alone keeps every test green. Both are kept
    // deliberately: this one states the precondition at the top where a reader
    // looks for it, the other guarantees the postcondition whatever arithmetic
    // lands between them.
    if !risk.is_finite() || risk.abs() < floor || !exit.is_finite() {
        return 0.0;
    }
    let r = (exit - entry) / risk;
    // Belt-and-braces: with all three inputs finite and `risk` above the floor,
    // the quotient is finite too — but this function's output goes straight into
    // a serialized golden, and `null` in a golden is unrecoverable. Never let a
    // non-finite escape, whatever future arithmetic lands above.
    if r.is_finite() { r } else { 0.0 }
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

    /// An invalidation close books its OWN counter, not the expiry one. Both come
    /// from a `ClosePositions` veto, and the replay loop used to hardcode `Expiry`
    /// for the whole arm — so a `too-low` flatten reported "CLOSED AT EXPIRY" and
    /// tallied `EXP` with the real trade-expiry days out (GBP/NZD 2026-07-22).
    #[test]
    fn invalidation_close_books_its_own_counter_not_expiry() {
        let mut e = ReplayEconomics::new();
        e.book(&long_fire(FillKind::ClosedOnInvalidation, Some(1.095)));
        assert_eq!(e.invalidation_closes, 1);
        assert_eq!(
            e.expiry_closes, 0,
            "an invalidation close must not be tallied as a trade-expiry close"
        );
        // Same −0.5R economics as any other flatten at that price: only the
        // *reason* differs, so a relabel must not move the money.
        assert!((e.net_r + 0.5).abs() < 1e-9, "net_r was {}", e.net_r);
        assert_eq!(e.legs.len(), 1);
        assert_eq!(e.legs[0].exit_reason, ExitReason::Invalidation);
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

    /// An open position books 0R **even when an exit price is present** — the
    /// `is_realized()` guard, not the absent price, is what makes it flat.
    ///
    /// Without this case `is_realized()` is unfalsifiable: the sibling test above
    /// passes `exit: None`, so `.filter(|_| reason.is_realized())` never receives
    /// a value and the closure is never called. Replacing the whole function body
    /// with `true` left all 499 tests green (verified 2026-07-27) — the ledger
    /// happens to hardcode `exit_price: None` beside `FillKind::Open`
    /// (`replay_broker.rs`), so the invariant was encoded in two places with only
    /// one enforced. If that ledger detail ever changes, an open position would
    /// silently score as a **win**, inflating every grid cell that holds one.
    #[test]
    fn an_open_position_books_zero_r_even_with_an_exit_price() {
        let mut e = ReplayEconomics::new();
        // 1.12 against entry 1.10 / stop 1.09 would be +2R if it were realized.
        e.book(&long_fire(FillKind::Open, Some(1.12)));

        assert_eq!(e.open_at_end, 1);
        assert_eq!(e.net_r, 0.0, "an open position must not be scored");
        assert_eq!(e.account(), START_ACCOUNT);
        let leg = &e.legs[0];
        assert_eq!(leg.r, 0.0);
        // The exit is dropped, not merely unscored: a leg that carried an exit
        // price with r == 0.0 would read as a scratch, which it is not.
        assert!(leg.exit_price.is_none());
        assert!(leg.exit_time.is_none());
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

    /// The zero-risk guard is RELATIVE to the price, so it holds at index scale
    /// too. With the old absolute `f64::EPSILON` threshold, a stop one
    /// representable step from entry on UK100 (~10500, ULP ~1.8e-12) sailed
    /// through and produced R ~ 1e10 — one leg would have swamped a batch's net R.
    #[test]
    fn zero_risk_guard_holds_at_index_scale() {
        let entry = 10_500.0_f64;
        // One float step away: absolutely tiny, but ~1.8e-12 at this magnitude.
        let stop = f64::from_bits(entry.to_bits() + 1);
        assert_ne!(stop, entry, "must actually be a different double");
        assert_eq!(
            realized_r(entry, stop, 10_600.0),
            0.0,
            "a one-ULP stop at index scale must be treated as zero-risk"
        );
        // A REAL index stop (30 points) still scores normally.
        assert!((realized_r(entry, 10_470.0, 10_530.0) - 1.0).abs() < 1e-9);
        // And the finest real FX tick (a fractional pip, 1e-6) is still scored.
        assert!((realized_r(1.10000, 1.099999, 1.100001) - 1.0).abs() < 1e-6);
    }

    /// **No input can produce a non-finite R.** `risk` was guarded but `exit`
    /// wasn't, so a garbage exit price yielded `NaN` — which then poisons `net_r`
    /// and every downstream sum.
    #[test]
    fn no_input_yields_a_non_finite_r() {
        let bad = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
        for &exit in &bad {
            assert_eq!(realized_r(1.10, 1.09, exit), 0.0, "exit {exit} → 0R");
        }
        for &entry in &bad {
            assert_eq!(realized_r(entry, 1.09, 1.11), 0.0, "entry {entry} → 0R");
        }
        for &stop in &bad {
            assert_eq!(realized_r(1.10, stop, 1.11), 0.0, "stop {stop} → 0R");
        }
        // Exhaustively: no combination of the three escapes finite.
        let all = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, 1.10, -1.10];
        for &e in &all {
            for &s in &all {
                for &x in &all {
                    assert!(
                        realized_r(e, s, x).is_finite(),
                        "realized_r({e}, {s}, {x}) was not finite"
                    );
                }
            }
        }
    }

    /// The reason the guard above matters: `serde_json` writes `NaN`/`inf` as
    /// `null` rather than failing, so a non-finite R would be *silently* baked
    /// into a golden — and that golden can then never be loaded again, with the
    /// resulting error classified as the operator's bad input rather than ours.
    ///
    /// Documents the serde behaviour AND proves the booked economics survive a
    /// round-trip when a garbage exit price is present.
    #[test]
    fn a_garbage_exit_price_still_leaves_a_loadable_golden() {
        // The trap, stated explicitly: this is what would have been written.
        assert_eq!(
            serde_json::to_string(&f64::NAN).unwrap_or_default(),
            "null",
            "serde_json serializes NaN as null instead of erroring"
        );
        assert!(
            serde_json::from_str::<f64>("null").is_err(),
            "…and null can't be read back as f64, so the golden is unloadable"
        );

        // Every price on the leg, not just `r`. Guarding `realized_r` alone was
        // not enough: the raw price is *stored* on the leg, so a non-finite one
        // still serialized as `null`. Found by this test, which is why it checks
        // all four fields rather than only the exit.
        let bad = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
        for &garbage in &bad {
            let cases = [
                (
                    "exit",
                    FireResult {
                        exit_price: Some(garbage),
                        ..long_fire(FillKind::TookProfit, None)
                    },
                ),
                (
                    "entry",
                    FireResult {
                        entry_price: garbage,
                        ..long_fire(FillKind::TookProfit, Some(1.12))
                    },
                ),
                (
                    "stop",
                    FireResult {
                        stop_loss: garbage,
                        ..long_fire(FillKind::TookProfit, Some(1.12))
                    },
                ),
                (
                    "take_profit",
                    FireResult {
                        take_profit: garbage,
                        ..long_fire(FillKind::TookProfit, Some(1.12))
                    },
                ),
            ];
            for (which, fire) in cases {
                let mut econ = ReplayEconomics::new();
                econ.book(&fire);
                let json = serde_json::to_string(&econ).expect("serialize");
                assert!(
                    !json.contains("null"),
                    "a non-finite {which} ({garbage}) leaked a null into the golden: {json}"
                );
                // The whole point: it can be read back.
                let back: ReplayEconomics =
                    serde_json::from_str(&json).expect("golden must reload");
                assert_eq!(back, econ, "round-trip must be exact for {which}");
                assert!(econ.net_r.is_finite(), "net_r stayed finite for {which}");
            }
        }

        // And a garbage exit books 0R rather than poisoning the sum.
        let mut econ = ReplayEconomics::new();
        econ.book(&long_fire(FillKind::TookProfit, Some(f64::INFINITY)));
        assert_eq!(econ.net_r, 0.0, "a garbage exit books 0R, not NaN");
        // Dropped, not stored — and `exit_time` stays consistent with it.
        let leg = &econ.legs[0];
        assert!(leg.exit_price.is_none());
        assert!(
            leg.exit_time.is_none(),
            "no exit price means no exit time — the leg must stay self-consistent"
        );
    }

    /// An implausible R is **recorded as measured, not clamped**.
    ///
    /// The reviewer's case: a 10×-scaled exit on a 1R bracket books 990R, enough
    /// for one leg to swamp a 291-trade aggregate. The response is a `warn!`, not
    /// a clamp — clamping would silently rewrite a real measurement and hide the
    /// data glitch that caused it. This test exists so nobody "fixes" that into a
    /// clamp without deciding to.
    #[test]
    fn an_implausible_r_is_recorded_not_clamped() {
        let mut econ = ReplayEconomics::new();
        // entry 1.10, stop 1.09 (1R = 0.01), exit 11.0 → 990R.
        econ.book(&long_fire(FillKind::TookProfit, Some(11.0)));

        let r = econ.legs[0].r;
        assert!(r > IMPLAUSIBLE_R, "must exceed the warn threshold: {r}");
        assert!(
            (r - 990.0).abs() < 1e-6,
            "the measured value must survive verbatim, not be clamped to \
             {IMPLAUSIBLE_R}: got {r}"
        );
        assert!((econ.net_r - r).abs() < 1e-9, "and it must reach net_r");
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
