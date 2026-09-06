//! What a reader of a futures replay must be told before believing its Net R.
//!
//! # The accepted gap
//!
//! Offline replay does not size. `ReplayBroker` reports `size: None` **by
//! design** (see `replay_broker::replay_placement`): sizing needs live account
//! equity and an FX rate, which an offline replay has by definition not got,
//! and replay economics are pure R-multiples off a synthetic account. Inventing
//! an equity model to produce contract counts would make the numbers *less*
//! honest, not more — so the IBKR plan accepts the gap rather than closing it.
//!
//! Accepting a gap silently is what this module exists to prevent. Three
//! mitigations, of which this module carries two and a half:
//!
//! - **A — close the closeable half.** The close-out deadline is pure calendar
//!   arithmetic and fully deterministic offline, so replay can and does check
//!   it ([`CloseOutVerdict`], via the shared `close_out_check::verdict`). The
//!   scoping doc bundled this with the impossible sizing half, which is the only
//!   reason it looked blocked.
//! - **B — name the bias.** A one-line caveat, in the same spirit as the
//!   `IMPLAUSIBLE_R` warning: the replay is **optimistic** about entries,
//!   because live may reject one that floors to zero contracts.
//! - **C — measure it.** The granularity probe ([`probe`]) counts how many
//!   fills would have floored to 0 contracts at an account size the operator
//!   *states*. A coverage statistic, not a simulation: it consumes a supplied
//!   number rather than inventing equity, which is exactly what keeps it
//!   honest.
//!
//! # Why the caveat is not printed for CFD replays
//!
//! Every existing replay is a CFD/spot replay, and none of this applies to
//! them. A caveat printed unconditionally would be noise on hundreds of
//! fixtures and would train the reader to skip it — so the block renders only
//! when the plan's instrument actually parses as a futures contract.

use chrono::{Datelike, NaiveDate};
use trade_control_cli::close_out_check::{self, CloseOutVerdict};
use trade_control_core::intent::Direction;

use super::economics::{Leg, RISK_FRACTION};

/// The line printed for every futures replay, whatever else is true.
///
/// Deliberately names the **direction** of the error. "Sizing not simulated"
/// alone would leave a reader to guess whether the replay is optimistic or
/// pessimistic; it is optimistic, because every entry fills here regardless of
/// whether a live account could afford a single contract.
pub const SIZING_CAVEAT: &str =
    "sizing not simulated — live may reject entries that floor to 0 contracts";

/// What the report needs to know about a futures replay, before it has legs.
///
/// Assembled by the caller (which knows the plan, the window and the flags) and
/// handed to the report, which runs the probe against the legs it books itself.
/// That ordering matters: the legs are produced *during* rendering, so a probe
/// run beforehand would have to book them a second time and could disagree with
/// the printed Net R.
pub struct FuturesContext {
    /// The close-out verdict for this plan over this replay window.
    pub verdict: CloseOutVerdict,
    /// The account size `--probe-account` stated, if any.
    pub probe_account: Option<f64>,
    /// The instrument's contract multiplier, if the catalog knows one.
    pub multiplier: Option<f64>,
}

impl FuturesContext {
    /// Resolve against the legs the report booked, or `None` when there is
    /// nothing to say.
    ///
    /// `None` for every CFD and spot replay — see the module docs.
    pub fn resolve(self, legs: &[Leg]) -> Option<Caveats> {
        let probe = self
            .probe_account
            .map(|account| probe(legs, account, self.multiplier));
        Caveats::new(self.verdict, probe)
    }
}

/// Assemble the futures context for a replay, or `None` when the plan is not a
/// futures plan.
///
/// `acts_until` is the end of the replay window — the last moment this replay
/// lets the plan open a position, which is the date the close-out deadline must
/// be compared against. (Arm time asks the same question about the trade
/// expiry; see `close_out_check::verdict`.)
///
/// The multiplier is read from `instrument-lookup`'s `futures` spec rather than
/// from `Asset::contract_multiplier()`, which answers `1.0` for spot. Here the
/// difference matters: `1.0` would let the probe judge a contract it has no
/// multiplier for, under-reporting floor-outs by up to 100× on gold. A futures
/// row with no multiplier stays `None` and the probe says so.
pub fn context_for(
    instrument: &str,
    direction: Direction,
    acts_until: NaiveDate,
    probe_account: Option<f64>,
) -> Option<FuturesContext> {
    let verdict = close_out_check::verdict(instrument, direction, acts_until, acts_until.year());
    if matches!(verdict, CloseOutVerdict::NotFutures) {
        return None;
    }
    let multiplier = verdict
        .contract()
        .and_then(|c| multiplier_for_root(&c.root));
    Some(FuturesContext {
        verdict,
        probe_account,
        multiplier,
    })
}

/// The contract multiplier for a futures **root**, if the catalog carries one.
///
/// Keyed on the root (`GC`), never on the instrument string the plan carries
/// (`GC 202612` / `GCZ6`): `instrument-lookup` has no contract-month dimension,
/// so one row per series holds the multiplier and every month shares it. That
/// is correct — the multiplier is a property of the series, not of a month.
///
/// Fail-soft: a catalog miss or an overlay error yields `None`, which the probe
/// reports as *cannot judge*. A replay report is read-only — there is nothing
/// here worth failing a replay over, and `None` is already the honest answer.
fn multiplier_for_root(root: &str) -> Option<f64> {
    let asset = instrument_lookup::resolve(root).ok().flatten()?;
    asset.futures.map(|f| f.multiplier)
}

/// One futures replay's caveats, ready to render.
pub struct Caveats {
    verdict: CloseOutVerdict,
    probe: Option<ProbeResult>,
}

impl Caveats {
    /// Build the caveats for a plan, or `None` when there is nothing to say.
    ///
    /// `None` for every CFD and spot replay — see the module docs.
    pub fn new(verdict: CloseOutVerdict, probe: Option<ProbeResult>) -> Option<Self> {
        match verdict {
            CloseOutVerdict::NotFutures => None,
            _ => Some(Self { verdict, probe }),
        }
    }

    /// Render as report lines, each already newline-terminated.
    ///
    /// Kept off the `Done:` / `Net R:` summary line: batch drivers scrape that
    /// line, so a caveat appended to it would change what they parse.
    pub fn render(&self) -> String {
        let mut out = String::from("\nFUTURES CAVEATS\n");
        out.push_str(&format!("  {SIZING_CAVEAT}\n"));
        out.push_str(&format!("  {}\n", self.close_out_line()));
        if let Some(probe) = &self.probe {
            out.push_str(&format!("  {}\n", probe.line()));
        }
        out
    }

    fn close_out_line(&self) -> String {
        match &self.verdict {
            // Unreachable via `new`, which refuses to build a `Caveats` for a
            // non-futures plan. Rendered rather than panicking: a report is a
            // read-only description, and no line in it is worth aborting a
            // replay over.
            CloseOutVerdict::NotFutures => "not a futures contract".to_string(),
            CloseOutVerdict::Armable { contract, arm_by } => format!(
                "close-out: OK — {} {} is armable through {arm_by}",
                contract.root, contract.contract_month,
            ),
            CloseOutVerdict::PastArmingWindow {
                contract,
                arm_by,
                close_out,
                acts_until,
            } => format!(
                "close-out: PAST WINDOW — {} {} could only be armed through {arm_by}, \
                 but this replay runs to {acts_until} and IBKR may liquidate without \
                 notice from {close_out}. Live, this plan would have been refused.",
                contract.root, contract.contract_month,
            ),
            CloseOutVerdict::UnknownContract { contract } => format!(
                "close-out: UNKNOWN — {} {} is not in the contract calendar, so its \
                 deadline cannot be checked. Live, this plan would have been refused.",
                contract.root, contract.contract_month,
            ),
        }
    }
}

/// What the granularity probe found.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    /// The account size the operator stated. Not measured, not inferred.
    pub account: f64,
    /// Legs that would have sized to at least one contract.
    pub placeable: usize,
    /// Legs whose budget floors to zero contracts at this account size.
    pub floored_to_zero: usize,
    /// Legs the probe could not judge, because the multiplier is unknown.
    ///
    /// **Never counted as placeable.** Substituting `1.0` for an unknown
    /// multiplier would under-count the floor-outs by the multiplier's own
    /// factor — 100× on gold — which is the silent-wrong-number failure this
    /// whole stage exists to avoid.
    pub unknown_multiplier: usize,
}

impl ProbeResult {
    fn line(&self) -> String {
        let total = self.placeable + self.floored_to_zero + self.unknown_multiplier;
        if self.unknown_multiplier > 0 {
            return format!(
                "granularity probe: CANNOT JUDGE {} of {total} fill(s) at ${:.0} — no \
                 contract multiplier for this instrument, so the probe refuses rather \
                 than assuming 1.0",
                self.unknown_multiplier, self.account,
            );
        }
        format!(
            "granularity probe: {} of {total} fill(s) would floor to 0 contracts at \
             a stated ${:.0} account ({} placeable)",
            self.floored_to_zero, self.account, self.placeable,
        )
    }
}

/// Count how many of `legs` could not have been placed at `account`.
///
/// `multiplier` is the instrument's contract multiplier; `None` when the
/// catalog has none, which the probe reports as *cannot judge* rather than
/// guessing.
///
/// The budget per leg is the same `RISK_FRACTION` the replay's own economics
/// compound on, so the probe measures the account the report already describes
/// rather than a second, differently-risked one.
///
/// FX is deliberately **not** modelled: it is the other half of the accepted
/// gap. A probe at a stated account size in the contract's own currency is a
/// coverage statistic; folding in a guessed FX rate would make it a simulation
/// with an invented input.
pub fn probe(legs: &[Leg], account: f64, multiplier: Option<f64>) -> ProbeResult {
    let budget = account * RISK_FRACTION;
    let mut result = ProbeResult {
        account,
        placeable: 0,
        floored_to_zero: 0,
        unknown_multiplier: 0,
    };
    for leg in legs {
        let Some(m) = multiplier.filter(|m| m.is_finite() && *m > 0.0) else {
            result.unknown_multiplier += 1;
            continue;
        };
        let stop_distance = (leg.entry_price - leg.stop_loss).abs();
        if contracts_for(budget, stop_distance, m) == 0 {
            result.floored_to_zero += 1;
        } else {
            result.placeable += 1;
        }
    }
    result
}

/// Contracts affordable at this budget and stop distance.
///
/// A deliberate re-statement of `broker_ibkr::risk::contracts_for_budget`'s
/// floor, not a call into it: `cli` does not depend on `broker-ibkr`, and the
/// probe needs only the offline half (no FX, no account fetch, no size grid).
/// The **floor** is the part that must match, and it does — rounding up here
/// would report a leg as placeable that the live broker would reject, which is
/// the exact optimism this probe exists to measure.
fn contracts_for(budget: f64, stop_distance: f64, multiplier: f64) -> u32 {
    if !budget.is_finite() || budget <= 0.0 {
        return 0;
    }
    if !stop_distance.is_finite() || stop_distance <= 0.0 {
        return 0;
    }
    let risk_per_contract = stop_distance * multiplier;
    if !risk_per_contract.is_finite() || risk_per_contract <= 0.0 {
        return 0;
    }
    let contracts = budget / risk_per_contract;
    if !contracts.is_finite() || contracts <= 0.0 {
        return 0;
    }
    contracts.floor().min(f64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_candles::economics::ExitReason;
    use chrono::{TimeZone, Utc};
    use trade_control_cli::futures_symbol::FuturesContract;

    fn day(s: &str) -> NaiveDate {
        s.parse().expect("test date")
    }

    fn gc() -> FuturesContract {
        FuturesContract {
            root: "GC".to_string(),
            contract_month: "202612".to_string(),
        }
    }

    /// A leg with the given stop distance, at a gold-like price.
    fn leg(stop_distance: f64) -> Leg {
        Leg {
            entry_time: Utc.timestamp_opt(0, 0).single().expect("epoch"),
            entry_price: 4000.0,
            stop_loss: 4000.0 - stop_distance,
            take_profit: 4100.0,
            exit_time: None,
            exit_price: None,
            exit_reason: ExitReason::OpenAtWindowEnd,
            r: 0.0,
        }
    }

    #[test]
    fn a_cfd_replay_gets_no_caveats_at_all() {
        assert!(
            Caveats::new(CloseOutVerdict::NotFutures, None).is_none(),
            "a CFD replay must render nothing — the whole existing corpus is CFD"
        );
    }

    /// The production entry point, not just the layer below it. `Caveats::new`
    /// also refuses a non-futures verdict, but that is a second guard — this
    /// pins the one the replay binary actually calls, so a refactor that trusts
    /// `context_for` alone cannot start printing a futures block on all 910 CFD
    /// fixtures.
    #[test]
    fn context_for_refuses_a_cfd_instrument_at_the_entry_point() {
        for name in ["EUR/USD", "EUR_USD", "Spot Gold", "US 500", "XAU_XAG"] {
            assert!(
                context_for(name, Direction::Short, day("2026-07-24"), Some(100_000.0)).is_none(),
                "{name} must produce no futures context at all"
            );
        }
    }

    /// The mirror: a real contract DOES produce a context, so "always return
    /// None" cannot satisfy the test above.
    #[test]
    fn context_for_builds_a_context_for_a_real_contract() {
        let ctx = context_for("GC 202612", Direction::Short, day("2026-07-24"), None)
            .expect("GC must produce a context");
        assert!(
            matches!(ctx.verdict, CloseOutVerdict::Armable { .. }),
            "expected armable in July for the December short"
        );
        assert_eq!(
            ctx.multiplier,
            Some(100.0),
            "the multiplier must come from the catalog's GC row, keyed on the \
             ROOT — the plan's instrument string carries a contract month the \
             catalog has no dimension for"
        );
    }

    #[test]
    fn a_futures_replay_always_names_the_sizing_bias() {
        let c = Caveats::new(
            CloseOutVerdict::Armable {
                contract: gc(),
                arm_by: day("2026-11-11"),
            },
            None,
        )
        .expect("futures must produce caveats");
        assert!(
            c.render().contains(SIZING_CAVEAT),
            "even a clean futures replay must name the sizing gap: {}",
            c.render()
        );
    }

    #[test]
    fn a_past_window_replay_says_live_would_have_refused() {
        let c = Caveats::new(
            CloseOutVerdict::PastArmingWindow {
                contract: gc(),
                arm_by: day("2026-11-11"),
                close_out: day("2026-11-25"),
                acts_until: day("2026-12-01"),
            },
            None,
        )
        .expect("futures must produce caveats");
        let text = c.render();
        assert!(text.contains("PAST WINDOW"), "{text}");
        assert!(
            text.contains("would have been refused"),
            "the reader must be told the live answer differs: {text}"
        );
        assert!(text.contains("2026-11-11"), "names the arm-by: {text}");
        assert!(text.contains("2026-11-25"), "names the close-out: {text}");
    }

    #[test]
    fn an_unknown_contract_says_it_cannot_be_checked() {
        let c = Caveats::new(CloseOutVerdict::UnknownContract { contract: gc() }, None)
            .expect("futures must produce caveats");
        let text = c.render();
        assert!(text.contains("UNKNOWN"), "{text}");
        assert!(text.contains("would have been refused"), "{text}");
    }

    /// The probe's whole point: the same legs are placeable at one account size
    /// and not at another. GC's multiplier is 100, so a $20 stop distance risks
    /// $2,000 per contract — affordable on a $1m account at 1%, not on $100k.
    #[test]
    fn the_same_legs_floor_out_at_a_small_account_and_not_a_large_one() {
        let legs = vec![leg(20.0), leg(20.0)];

        let small = probe(&legs, 100_000.0, Some(100.0));
        assert_eq!(
            small.floored_to_zero, 2,
            "1% of $100k is $1,000 — under one
             $2,000 contract: {small:?}"
        );
        assert_eq!(small.placeable, 0);

        let large = probe(&legs, 1_000_000.0, Some(100.0));
        assert_eq!(
            large.floored_to_zero, 0,
            "1% of $1m is $10,000 — five contracts: {large:?}"
        );
        assert_eq!(large.placeable, 2);
    }

    /// The micro contract is the reason both sizes are in the calendar: MGC's
    /// multiplier is 10, so the identical setup that floors out on GC is
    /// placeable on MGC at the same account.
    #[test]
    fn a_micro_contract_is_placeable_where_the_full_size_is_not() {
        let legs = vec![leg(20.0)];
        assert_eq!(probe(&legs, 100_000.0, Some(100.0)).floored_to_zero, 1);
        assert_eq!(probe(&legs, 100_000.0, Some(10.0)).placeable, 1);
    }

    #[test]
    fn an_unknown_multiplier_is_refused_not_assumed_to_be_one() {
        let legs = vec![leg(20.0)];
        let r = probe(&legs, 100_000.0, None);
        assert_eq!(r.unknown_multiplier, 1);
        assert_eq!(
            r.placeable, 0,
            "an unjudgeable leg must never be counted placeable"
        );
        assert_eq!(r.floored_to_zero, 0);
        assert!(
            r.line().contains("CANNOT JUDGE"),
            "the operator must be told the probe could not answer: {}",
            r.line()
        );
    }

    /// A multiplier of 0, NaN or a negative number is as unusable as a missing
    /// one, and must land in the same refusal rather than dividing by zero.
    #[test]
    fn an_unusable_multiplier_is_treated_as_unknown() {
        let legs = vec![leg(20.0)];
        for m in [0.0, -100.0, f64::NAN, f64::INFINITY] {
            let r = probe(&legs, 100_000.0, Some(m));
            assert_eq!(r.unknown_multiplier, 1, "multiplier {m} must not be used");
            assert_eq!(r.placeable, 0, "multiplier {m} must not be placeable");
        }
    }

    /// Flooring, not rounding: 1.9 contracts is one contract, and 0.9 is none.
    /// Rounding up would report a leg as placeable that the live broker rejects
    /// — the exact optimism the probe measures.
    #[test]
    fn a_fractional_contract_count_floors_and_never_rounds_up() {
        // Budget $1,000; multiplier 100. A stop of 5.3 costs $530/contract →
        // 1.88 contracts → 1.
        assert_eq!(contracts_for(1_000.0, 5.3, 100.0), 1);
        // A stop of 11 costs $1,100/contract → 0.9 contracts → 0, not 1.
        assert_eq!(contracts_for(1_000.0, 11.0, 100.0), 0);
    }

    #[test]
    fn a_zero_stop_distance_sizes_to_zero_rather_than_dividing_by_zero() {
        assert_eq!(contracts_for(1_000.0, 0.0, 100.0), 0);
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            assert_eq!(contracts_for(1_000.0, bad, 100.0), 0, "stop {bad}");
            assert_eq!(contracts_for(bad, 5.0, 100.0), 0, "budget {bad}");
        }
    }

    #[test]
    fn the_probe_reports_the_account_it_was_given_not_one_it_inferred() {
        let r = probe(&[leg(20.0)], 250_000.0, Some(10.0));
        assert!(
            r.line().contains("250000"),
            "the stated account must appear verbatim: {}",
            r.line()
        );
    }

    #[test]
    fn no_legs_means_nothing_to_judge() {
        let r = probe(&[], 100_000.0, Some(100.0));
        assert_eq!(r.placeable, 0);
        assert_eq!(r.floored_to_zero, 0);
        assert_eq!(r.unknown_multiplier, 0);
    }
}
