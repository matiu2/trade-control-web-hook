//! Contract specifications and close-out deadline derivation.
//!
//! # The trap this crate exists to avoid
//!
//! IBKR **force-liquidates** an expiring futures position, without additional
//! notice, during a "close-out period" preceding expiration. The deadline is
//! **not** the contract's expiry date:
//!
//! - **Long:** end of the 2nd business day before **First Notice Day**.
//! - **Short:** end of the 2nd business day before **last trade day**.
//!
//! For a physically delivered contract, First Notice Day is the last business
//! day of the month *preceding* delivery. So a **December** gold contract's
//! long close-out deadline falls in **November** — roughly a month before the
//! expiry date anyone would read off the contract chain. Verified live on
//! 2026-09-06: GCU6 (last trade 2026-09-28) was already past its long
//! close-out deadline while the chain still listed it as healthy front month.
//!
//! Cash-settled contracts have no delivery obligation and therefore no First
//! Notice Day, so both deadlines derive from last trade day and coincide.

use chrono::{NaiveDate, Weekday};

use crate::holiday::{
    business_days_before, is_covered, last_business_day_of_month, nth_last_business_day_of_month,
    nth_weekday_of_month,
};

/// Business days between the close-out deadline and its reference day.
///
/// IBKR's stated policy: *"The standard Close-Out Deadline for holders of long
/// positions is the end of the second (2nd) business day prior to the exchange
/// specified First Notice Day"*, and for shorts *"the end of trading on the
/// second (2nd) business day prior to the exchange-specified last trade day"*.
///
/// ⚠️ **This is the STANDARD deadline, and IBKR documents per-product
/// overrides**: the same page adds *"Certain contracts use a different time
/// ahead of the Close-Out deadline as specified in the following table."* That
/// table is behind an anti-scraping block and has **not** been read, so it is
/// unconfirmed whether GC/MGC/ES/MES carry an override. Anything shorter than
/// the standard would make these deadlines **too late** — the unsafe
/// direction. Verify against IBKR's live close-out table (client portal, or
/// ask a rep) before the first real-money futures position; until then the
/// Stage 3 safety margin is what absorbs the uncertainty.
pub const CLOSE_OUT_BUSINESS_DAYS: u32 = 2;

/// Business days of head-room between the last moment a plan may still be
/// *entering* and the close-out deadline itself.
///
/// The deadline is when IBKR may liquidate. Arming right up to it would be
/// wrong, because an armed plan is not a position — it is a *licence to open
/// one*, and everything between arming and the deadline has to fit:
///
/// - the trade window itself (`trade_expiry`, default 48h from arming),
/// - multi-shot re-entry, which may place again after a stop-out,
/// - a weekend, during which nothing can be closed,
/// - and the unread per-product override table above, which could move the
///   real deadline earlier than [`CLOSE_OUT_BUSINESS_DAYS`] assumes.
///
/// Ten business days clears all four with room to spare. It costs coverage —
/// roughly a fortnight at the end of each contract month during which new
/// plans on the front month are refused and the operator arms the next month
/// instead — which is the intended trade, since rolling forward is free and a
/// forced liquidation is not.
pub const ARM_SAFETY_BUSINESS_DAYS: u32 = 10;

/// How a contract settles at expiry.
///
/// This is the single fact that decides whether a contract has a First Notice
/// Day, and therefore whether its long deadline lands a month early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    /// Physically delivered — has a First Notice Day; long positions face a
    /// delivery obligation and the long deadline is ~a month before expiry.
    Physical,
    /// Cash settled — no delivery, no First Notice Day.
    Cash,
}

impl Settlement {
    pub fn as_str(self) -> &'static str {
        match self {
            Settlement::Physical => "physical",
            Settlement::Cash => "cash",
        }
    }
}

/// How a contract's last trade day is computed from its contract month.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastTradeRule {
    /// The `n`th-last business day of the delivery month. COMEX metals use
    /// the third-last business day.
    NthLastBusinessDay(u32),
    /// The `n`th occurrence of a weekday in the contract month. CME equity
    /// index futures use the third Friday.
    NthWeekday(Weekday, u32),
}

/// Which calendar months a contract root lists as actively traded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonthCycle {
    /// Feb, Apr, Jun, Aug, Oct, Dec — COMEX gold's listed cycle. CME's spec
    /// reads *"for delivery in any February, April, June, August, October, and
    /// December falling within a 24-month period"*. Some odd months are also
    /// listed with far lower liquidity; they are deliberately not emitted,
    /// since a thin book is where a stop-loss slips worst.
    EvenMonths,
    /// Mar, Jun, Sep, Dec — the CME equity index quarterly cycle.
    Quarterly,
}

impl MonthCycle {
    /// The contract months in this cycle, ascending.
    pub fn months(self) -> &'static [u32] {
        match self {
            MonthCycle::EvenMonths => &[2, 4, 6, 8, 10, 12],
            MonthCycle::Quarterly => &[3, 6, 9, 12],
        }
    }
}

/// A tradeable futures root and the rules its contracts follow.
#[derive(Debug, Clone, Copy)]
pub struct ContractSpec {
    /// The root symbol as IBKR reports it (`"GC"`, `"MGC"`, `"ES"`, `"MES"`).
    pub root: &'static str,
    /// Exchange, for the generated table's provenance.
    pub exchange: &'static str,
    pub settlement: Settlement,
    pub cycle: MonthCycle,
    pub last_trade: LastTradeRule,
    /// Contract multiplier, verified live against the paper Gateway on
    /// 2026-09-06. Recorded here for cross-checking Stage 5's baked value; it
    /// is **not** the source of truth for sizing.
    pub multiplier: f64,
}

/// The contract roots this system trades.
///
/// Both standard and micro variants are listed (operator decision): the micros
/// are required for the demo month, since GC's ~$409k notional is unusable
/// below a ~$400k account.
///
/// ⚠️ **MGC is recorded as `Physical`.** Secondary retail/broker sources widely
/// claim COMEX Micro Gold is cash-settled, but the primary-adjacent sources say
/// otherwise and are what this encoding follows:
///
/// - CME's own micro-metals product literature gives the settlement method as
///   **deliverable** — delivery takes the form of an **Accumulated Certificate
///   of Exchange (ACE)**, a 10% interest in a 100 oz COMEX bar, ten of which
///   aggregate into one full warrant. That an ACE isn't a physical bar is
///   likely what the "cash-settled" claims are garbling.
/// - **IBKR's own COMEX precious metals documentation groups MGC together with
///   GC** under physical delivery, and describes an Intent to Receive / Intent
///   to Deliver process for both. IBKR would have no reason to document a
///   delivery process for a contract it treats as cash-settled.
///
/// The error is asymmetric regardless: `Physical` costs a slightly earlier
/// deadline, `Cash` risks a delivery obligation and a forced liquidation. So
/// the conservative reading is also the better-sourced one. Still worth a final
/// confirmation against a CME terminal or an IBKR rep before real money.
pub const CONTRACT_SPECS: &[ContractSpec] = &[
    ContractSpec {
        root: "GC",
        exchange: "COMEX",
        settlement: Settlement::Physical,
        cycle: MonthCycle::EvenMonths,
        last_trade: LastTradeRule::NthLastBusinessDay(3),
        multiplier: 100.0,
    },
    ContractSpec {
        root: "MGC",
        exchange: "COMEX",
        settlement: Settlement::Physical,
        cycle: MonthCycle::EvenMonths,
        last_trade: LastTradeRule::NthLastBusinessDay(3),
        multiplier: 10.0,
    },
    ContractSpec {
        root: "ES",
        exchange: "CME",
        settlement: Settlement::Cash,
        cycle: MonthCycle::Quarterly,
        last_trade: LastTradeRule::NthWeekday(Weekday::Fri, 3),
        multiplier: 50.0,
    },
    ContractSpec {
        root: "MES",
        exchange: "CME",
        settlement: Settlement::Cash,
        cycle: MonthCycle::Quarterly,
        last_trade: LastTradeRule::NthWeekday(Weekday::Fri, 3),
        multiplier: 5.0,
    },
];

/// Look up a spec by root symbol (case-sensitive, as IBKR reports it).
pub fn spec_for(root: &str) -> Option<&'static ContractSpec> {
    CONTRACT_SPECS.iter().find(|s| s.root == root)
}

/// One fully derived contract month.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractDates {
    pub root: &'static str,
    pub year: i32,
    pub month: u32,
    pub last_trade_day: NaiveDate,
    /// `None` for cash-settled contracts — they have no delivery obligation.
    pub first_notice_day: Option<NaiveDate>,
    /// Deadline for a **long** position: 2 business days before First Notice
    /// Day (physical) or before last trade day (cash).
    pub long_close_out: NaiveDate,
    /// Deadline for a **short** position: 2 business days before last trade
    /// day, regardless of settlement type.
    pub short_close_out: NaiveDate,
    /// Last day a **long** plan may still be armed:
    /// [`ARM_SAFETY_BUSINESS_DAYS`] before [`Self::long_close_out`]. The guard
    /// reads this, not the deadline — see the constant's docs for why the
    /// head-room is needed.
    pub long_arm_by: NaiveDate,
    /// Last day a **short** plan may still be armed.
    pub short_arm_by: NaiveDate,
}

/// Compute the last trade day for a contract month.
pub fn last_trade_day(spec: &ContractSpec, year: i32, month: u32) -> Option<NaiveDate> {
    match spec.last_trade {
        LastTradeRule::NthLastBusinessDay(n) => nth_last_business_day_of_month(year, month, n),
        LastTradeRule::NthWeekday(weekday, n) => nth_weekday_of_month(year, month, weekday, n),
    }
}

/// Compute First Notice Day: the last business day of the month **preceding**
/// the delivery month. `None` for cash-settled contracts, which have none.
///
/// The "preceding month" is the whole reason a December gold contract's long
/// deadline lands in November.
pub fn first_notice_day(spec: &ContractSpec, year: i32, month: u32) -> Option<NaiveDate> {
    if spec.settlement != Settlement::Physical {
        return None;
    }
    let (prior_year, prior_month) = if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    };
    last_business_day_of_month(prior_year, prior_month)
}

/// Derive every date for one contract month.
///
/// Returns `None` when any component falls outside the holiday table's covered
/// span — a refusal, never an extrapolated guess.
pub fn contract_dates(spec: &ContractSpec, year: i32, month: u32) -> Option<ContractDates> {
    let last_trade = last_trade_day(spec, year, month)?;
    if !is_covered(last_trade) {
        return None;
    }
    let fnd = first_notice_day(spec, year, month);
    if spec.settlement == Settlement::Physical && fnd.is_none() {
        return None;
    }
    // A long faces the delivery obligation, so it counts back from First
    // Notice Day when there is one; a short only has to be out before trading
    // stops.
    let long_reference = fnd.unwrap_or(last_trade);
    let long_close_out = business_days_before(long_reference, CLOSE_OUT_BUSINESS_DAYS)?;
    let short_close_out = business_days_before(last_trade, CLOSE_OUT_BUSINESS_DAYS)?;
    // The arm-by dates carry the head-room the arm-time guard needs; deriving
    // them here keeps one holiday table rather than duplicating it into `core`.
    let long_arm_by = business_days_before(long_close_out, ARM_SAFETY_BUSINESS_DAYS)?;
    let short_arm_by = business_days_before(short_close_out, ARM_SAFETY_BUSINESS_DAYS)?;
    Some(ContractDates {
        root: spec.root,
        year,
        month,
        last_trade_day: last_trade,
        first_notice_day: fnd,
        long_close_out,
        short_close_out,
        long_arm_by,
        short_arm_by,
    })
}

/// Derive every listed contract month for every known root across `years`.
///
/// Months whose dates fall outside the holiday table are skipped with a
/// warning rather than emitted with guessed arithmetic.
pub fn all_contract_dates(years: &[i32]) -> Vec<ContractDates> {
    let mut out: Vec<ContractDates> = CONTRACT_SPECS
        .iter()
        .flat_map(|spec| {
            years.iter().flat_map(move |&year| {
                spec.cycle.months().iter().filter_map(move |&month| {
                    let dates = contract_dates(spec, year, month);
                    if dates.is_none() {
                        tracing::warn!(
                            root = spec.root,
                            year,
                            month,
                            "skipped: dates fall outside the holiday table"
                        );
                    }
                    dates
                })
            })
        })
        .collect();
    out.sort_by(|a, b| (a.root, a.year, a.month).cmp(&(b.root, b.year, b.month)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).expect("valid test date")
    }

    fn spec(root: &str) -> &'static ContractSpec {
        spec_for(root).expect("known root in test")
    }

    /// THE anchor test for this stage: a December gold contract's long
    /// close-out deadline must land in NOVEMBER, ~a month before expiry.
    #[test]
    fn arm_by_leaves_ten_business_days_of_head_room() {
        // The guard reads arm_by, not the deadline. Pin the gap in the unit
        // that matters — business days — rather than re-asserting a date the
        // implementation just produced.
        let gc = contract_dates(spec_for("GC").expect("GC known"), 2026, 12).expect("derivable");
        assert_eq!(
            business_days_before(gc.long_close_out, ARM_SAFETY_BUSINESS_DAYS),
            Some(gc.long_arm_by),
        );
        assert_eq!(
            business_days_before(gc.short_close_out, ARM_SAFETY_BUSINESS_DAYS),
            Some(gc.short_arm_by),
        );
        // And it really is head-room: strictly earlier than the deadline.
        assert!(gc.long_arm_by < gc.long_close_out);
        assert!(gc.short_arm_by < gc.short_close_out);
    }

    #[test]
    fn arm_by_inherits_the_month_early_trap() {
        // The whole point of Stage 3: a December gold LONG must stop being
        // armable in NOVEMBER, well before anyone reading the chain's expiry
        // date would expect. Verified by hand: 10 business days back from
        // 2026-11-25, skipping Thanksgiving week's weekends, is 2026-11-11.
        let gc = contract_dates(spec_for("GC").expect("GC known"), 2026, 12).expect("derivable");
        assert_eq!(
            gc.long_arm_by.month(),
            11,
            "long arm-by must land in November"
        );
        assert_eq!(
            gc.long_arm_by,
            NaiveDate::from_ymd_opt(2026, 11, 11).expect("valid"),
        );
        // The short is a month later — the two must not collapse.
        assert_eq!(gc.short_arm_by.month(), 12);
        assert!(gc.long_arm_by < gc.short_arm_by);
    }

    #[test]
    fn gc_december_long_deadline_is_in_november() {
        let dates = contract_dates(spec("GC"), 2026, 12).expect("GC Dec 2026 derivable");
        // Last trade is the third-last business day of December.
        assert_eq!(dates.last_trade_day, d(2026, 12, 29));
        // First Notice Day is the last business day of NOVEMBER.
        assert_eq!(dates.first_notice_day, Some(d(2026, 11, 30)));
        // So the long deadline is two business days before that — still November.
        assert_eq!(dates.long_close_out, d(2026, 11, 25));
        assert_eq!(
            dates.long_close_out.month(),
            11,
            "the whole point: a December contract dies in November"
        );
        // And it is more than three weeks before the contract's own expiry.
        let gap = (dates.last_trade_day - dates.long_close_out).num_days();
        assert!(
            gap > 30,
            "long deadline should precede expiry by ~a month, got {gap} days"
        );
    }

    #[test]
    fn gc_long_and_short_deadlines_differ() {
        let dates = contract_dates(spec("GC"), 2026, 12).expect("derivable");
        // Short only has to be out before trading stops: 2 business days
        // before Dec 29 ⇒ Dec 24 (Christmas Day Dec 25 is a holiday).
        assert_eq!(dates.short_close_out, d(2026, 12, 24));
        assert!(
            dates.long_close_out < dates.short_close_out,
            "a physical long must exit well before the short"
        );
    }

    #[test]
    fn es_is_cash_settled_with_no_first_notice_day() {
        let dates = contract_dates(spec("ES"), 2026, 12).expect("ES Dec 2026 derivable");
        assert_eq!(dates.first_notice_day, None, "cash settled ⇒ no FND");
        assert_eq!(
            dates.long_close_out, dates.short_close_out,
            "with no delivery obligation both deadlines coincide"
        );
    }

    #[test]
    fn es_last_trade_is_the_third_friday() {
        // Verified live against the paper Gateway 2026-09-06: ES/MES 202609
        // last trade 2026-09-18.
        let dates = contract_dates(spec("ES"), 2026, 9).expect("derivable");
        assert_eq!(dates.last_trade_day, d(2026, 9, 18));
        assert_eq!(dates.last_trade_day.weekday(), Weekday::Fri);
    }

    #[test]
    fn live_gateway_fixtures_match_the_derived_dates() {
        // Captured from the live IBKR paper Gateway on 2026-09-06. These are
        // the ground truth the whole crate is calibrated against.
        let cases = [
            ("GC", 2026, 9, d(2026, 9, 28)),
            ("MGC", 2026, 10, d(2026, 10, 28)),
            ("ES", 2026, 9, d(2026, 9, 18)),
            ("MES", 2026, 9, d(2026, 9, 18)),
        ];
        for (root, year, month, expected) in cases {
            // GC/MGC list all twelve months; the odd September and October
            // contracts are outside the traded even-month cycle but their
            // date arithmetic must still be right.
            let dates = contract_dates(spec(root), year, month)
                .unwrap_or_else(|| panic!("{root} {year}-{month} derivable"));
            assert_eq!(
                dates.last_trade_day, expected,
                "{root} {year}-{month:02} last trade day"
            );
        }
    }

    #[test]
    fn gcu6_was_already_past_its_long_deadline_on_2026_09_06() {
        // The live trap that motivated this crate: GCU6 was listed as healthy
        // front month while its long close-out deadline had already passed.
        let dates = contract_dates(spec("GC"), 2026, 9).expect("derivable");
        let observed = d(2026, 9, 6);
        assert!(
            dates.long_close_out < observed,
            "GCU6 long deadline {} should already be past on {observed}",
            dates.long_close_out
        );
        assert!(
            dates.last_trade_day > observed,
            "yet the contract still had weeks to expiry"
        );
    }

    #[test]
    fn micros_inherit_the_standard_contract_rules() {
        // MGC follows GC's calendar; MES follows ES's. Only size differs.
        let gc = contract_dates(spec("GC"), 2026, 12).expect("derivable");
        let mgc = contract_dates(spec("MGC"), 2026, 12).expect("derivable");
        assert_eq!(gc.last_trade_day, mgc.last_trade_day);
        assert_eq!(gc.long_close_out, mgc.long_close_out);

        let es = contract_dates(spec("ES"), 2026, 12).expect("derivable");
        let mes = contract_dates(spec("MES"), 2026, 12).expect("derivable");
        assert_eq!(es.last_trade_day, mes.last_trade_day);
    }

    #[test]
    fn unknown_root_is_refused() {
        assert!(spec_for("CL").is_none(), "crude oil is not configured");
        assert!(spec_for("gc").is_none(), "lookup is case-sensitive");
    }

    #[test]
    fn january_physical_contract_looks_back_to_the_prior_year() {
        // A January delivery month's First Notice Day is in the PRIOR
        // December — the year rollover must not be dropped.
        let fnd = first_notice_day(spec("GC"), 2027, 1).expect("derivable");
        assert_eq!(fnd.year(), 2026);
        assert_eq!(fnd.month(), 12);
        assert_eq!(fnd, d(2026, 12, 31));
    }

    #[test]
    fn cycles_list_the_expected_months() {
        assert_eq!(MonthCycle::EvenMonths.months(), &[2, 4, 6, 8, 10, 12]);
        assert_eq!(MonthCycle::Quarterly.months(), &[3, 6, 9, 12]);
    }

    #[test]
    fn all_contract_dates_covers_every_root() {
        let all = all_contract_dates(&[2026, 2027]);
        for spec in CONTRACT_SPECS {
            assert!(
                all.iter().any(|d| d.root == spec.root),
                "{} missing from the generated set",
                spec.root
            );
        }
        // Sorted by (root, year, month) for a deterministic table.
        let mut sorted = all.clone();
        sorted.sort_by(|a, b| (a.root, a.year, a.month).cmp(&(b.root, b.year, b.month)));
        assert_eq!(all, sorted);
    }

    #[test]
    fn out_of_range_years_are_refused() {
        // 2030 is outside the holiday table ⇒ no guessed arithmetic.
        assert!(contract_dates(spec("GC"), 2030, 12).is_none());
        assert!(all_contract_dates(&[2030]).is_empty());
    }
}
