//! Futures contract close-out calendar — the lookup side of the baked table.
//!
//! # What this answers
//!
//! *"If I hold this futures contract in this direction, by when must I be
//! out?"* — and it answers **fail-closed**: an unknown contract, an
//! unparseable row or an ambiguous key all yield [`None`], which the caller
//! must treat as **refusal to arm**, never as "no constraint".
//!
//! # Why the deadline is not the expiry date
//!
//! IBKR force-liquidates an expiring position during a close-out period
//! preceding expiry, *without additional prior notice*, and does not roll
//! positions ("Automatic Futures Rollover" is a charting feature that rolls
//! nothing). The deadlines:
//!
//! - **Long:** 2 business days before **First Notice Day** — and for a
//!   physically delivered contract First Notice Day is the last business day
//!   of the month *preceding* delivery. A **December** gold contract's long
//!   deadline therefore lands in **November**, roughly a month before the
//!   expiry date shown on the contract chain.
//! - **Short:** 2 business days before **last trade day**.
//!
//! Verified live on 2026-09-06: GCU6 (last trade 2026-09-28) was already past
//! its long close-out deadline while the chain still listed it as the healthy
//! front month.
//!
//! # Direction is not optional
//!
//! [`close_out_deadline`] takes a [`Direction`] because for a physical
//! contract the two answers differ by about a month. Collapsing them into one
//! number would either forbid perfectly good shorts or — far worse — permit
//! longs a month past their deadline. There is deliberately no
//! direction-agnostic accessor.
//!
//! # Keying
//!
//! Rows are keyed on `(root, contract_month)` and **both** columns are
//! matched. `core/src/spread_blackout/coverage.rs` and
//! `core/src/intent/blackout/baked.rs` each bind their first column to `_` and
//! take the first symbol match, so a colliding key silently resolves to
//! whichever row sorted first; that shape is not copied here. A duplicate key
//! is rejected at render time by the generator, and [`lookup`] additionally
//! returns `None` if it ever sees one, rather than picking a winner.

use chrono::NaiveDate;

// The close-out deadline differs by direction on physically delivered
// contracts, so every accessor here takes one. Deliberately the *same*
// `Direction` the rest of the crate uses rather than a private copy — two
// identically-shaped enums for "which side of the market" is exactly the drift
// hazard the four parallel broker enums already demonstrate.
pub use crate::intent::Direction;

/// The generated close-out calendar, produced offline by
/// `contract-calendar-gen` and committed as a source file. Row shape
/// `(root, contract_month, settlement, last_trade_day, first_notice_day,
/// long_close_out, short_close_out, long_arm_by, short_arm_by)`, sorted by
/// `(root, contract_month)`.
#[allow(clippy::type_complexity)]
mod baked_table {
    include!("contract_calendar_baked.rs");
}
use baked_table::CONTRACT_CALENDAR_BAKED;

/// One row of the baked table: `(root, contract_month, settlement,
/// last_trade_day, first_notice_day, long_close_out, short_close_out,
/// long_arm_by, short_arm_by)`.
type BakedRow = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);

/// How a contract settles at expiry.
///
/// Named `SettlementType` rather than `Settlement` to stay distinct from
/// [`crate::settlement::Settlement`], which is an unrelated concept (what the
/// broker reported about a finished trade).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementType {
    /// Physically delivered — has a First Notice Day, so a long position's
    /// deadline lands about a month before expiry.
    Physical,
    /// Cash settled — no delivery obligation and no First Notice Day.
    Cash,
}

/// One contract month's dates, as read back from the baked table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractCalendarEntry {
    pub root: &'static str,
    /// `"YYYYMM"`, the form IBKR reports on `ContractDetails`.
    pub contract_month: &'static str,
    pub settlement: SettlementType,
    pub last_trade_day: NaiveDate,
    /// `None` for cash-settled contracts, which have no First Notice Day.
    pub first_notice_day: Option<NaiveDate>,
    pub long_close_out: NaiveDate,
    pub short_close_out: NaiveDate,
    /// Last day a **long** plan may still be armed — the deadline minus the
    /// arm-time safety margin. See [`Self::arm_by_for`].
    pub long_arm_by: NaiveDate,
    /// Last day a **short** plan may still be armed.
    pub short_arm_by: NaiveDate,
}

impl ContractCalendarEntry {
    /// The close-out deadline for a position in `direction`.
    pub fn close_out_for(&self, direction: Direction) -> NaiveDate {
        match direction {
            Direction::Long => self.long_close_out,
            Direction::Short => self.short_close_out,
        }
    }

    /// The last day a plan in `direction` may still be **armed**.
    ///
    /// Earlier than [`Self::close_out_for`], because arming is a licence to
    /// open a position later: the trade window, multi-shot re-entry and a
    /// weekend all have to fit between arming and the deadline. The margin is
    /// baked by `contract-calendar-gen` (`ARM_SAFETY_BUSINESS_DAYS`) so the
    /// business-day arithmetic — and its holiday table — lives in exactly one
    /// place.
    pub fn arm_by_for(&self, direction: Direction) -> NaiveDate {
        match direction {
            Direction::Long => self.long_arm_by,
            Direction::Short => self.short_arm_by,
        }
    }
}

/// Look up one contract month.
///
/// Returns `None` when the contract is not in the table, when a row is
/// malformed, or when the key is ambiguous — all of which the caller must
/// treat as a refusal, not as an absent constraint.
pub fn lookup(root: &str, contract_month: &str) -> Option<ContractCalendarEntry> {
    lookup_in(CONTRACT_CALENDAR_BAKED, root, contract_month)
}

/// [`lookup`] against an explicit table.
///
/// Split out so the ambiguity guard below is reachable from tests: the
/// generator rejects duplicate keys, so the baked table can never contain one
/// and a test driving `lookup` alone could never exercise the guard. An
/// untested guard is one a later refactor deletes without a red test — which is
/// precisely how `coverage.rs` came to take the first match silently.
fn lookup_in(
    table: &'static [BakedRow],
    root: &str,
    contract_month: &str,
) -> Option<ContractCalendarEntry> {
    let mut matches = table
        .iter()
        .filter(|(r, m, ..)| *r == root && *m == contract_month);
    let row = matches.next()?;
    // A duplicate key means the table is ambiguous. Picking the first match is
    // exactly the silent-wrong-row bug this module's docs call out; refuse.
    if matches.next().is_some() {
        tracing::error!(
            root,
            contract_month,
            "duplicate contract calendar rows — refusing to guess"
        );
        return None;
    }
    parse_row(row)
}

/// The close-out deadline for `(root, contract_month)` in `direction`.
///
/// `None` is a **refusal**: unknown contract, malformed row, or ambiguous key.
/// Callers must never read it as "no deadline applies".
pub fn close_out_deadline(
    root: &str,
    contract_month: &str,
    direction: Direction,
) -> Option<NaiveDate> {
    lookup(root, contract_month).map(|e| e.close_out_for(direction))
}

/// Is `date` on or after the close-out deadline for this contract and
/// direction — i.e. inside the window where IBKR may liquidate without notice?
///
/// Returns `None` when the contract is unknown, so a caller cannot accidentally
/// read "unknown" as "safe".
pub fn is_past_close_out(
    root: &str,
    contract_month: &str,
    direction: Direction,
    date: NaiveDate,
) -> Option<bool> {
    close_out_deadline(root, contract_month, direction).map(|deadline| date >= deadline)
}

/// The last day a plan on `(root, contract_month)` may still be armed in
/// `direction`.
///
/// `None` is a **refusal**, exactly as for [`close_out_deadline`].
pub fn arm_by_deadline(
    root: &str,
    contract_month: &str,
    direction: Direction,
) -> Option<NaiveDate> {
    lookup(root, contract_month).map(|e| e.arm_by_for(direction))
}

/// May a plan on this contract still be armed on `date`, in `direction`?
///
/// **`None` means refuse, not "unconstrained".** An unknown root, an unlisted
/// contract month, a malformed row and an ambiguous key all land here, and in
/// every one of those cases we do not know when IBKR would liquidate. The
/// caller must not `unwrap_or(true)`.
///
/// `true` on the arm-by date itself — the margin already carries the head-room,
/// so there is no reason to also lose its last day.
pub fn is_armable_on(
    root: &str,
    contract_month: &str,
    direction: Direction,
    date: NaiveDate,
) -> Option<bool> {
    arm_by_deadline(root, contract_month, direction).map(|arm_by| date <= arm_by)
}

/// Every contract month listed for a root, ascending by month.
pub fn months_for_root(root: &str) -> Vec<ContractCalendarEntry> {
    CONTRACT_CALENDAR_BAKED
        .iter()
        .filter(|(r, ..)| *r == root)
        .filter_map(parse_row)
        .collect()
}

/// Decode one baked row. A malformed row yields `None` (and logs) rather than
/// a partially-defaulted entry.
fn parse_row(row: &'static BakedRow) -> Option<ContractCalendarEntry> {
    let (
        root,
        contract_month,
        settlement,
        last_trade,
        fnd,
        long_out,
        short_out,
        long_arm,
        short_arm,
    ) = *row;
    let settlement = match settlement {
        "physical" => SettlementType::Physical,
        "cash" => SettlementType::Cash,
        other => {
            tracing::error!(root, contract_month, other, "unknown settlement type");
            return None;
        }
    };
    let last_trade_day = parse_date(last_trade, root, contract_month)?;
    // An empty First Notice Day is the documented encoding for cash-settled;
    // a non-empty one that fails to parse is a malformed row, not "no FND".
    let first_notice_day = if fnd.is_empty() {
        None
    } else {
        Some(parse_date(fnd, root, contract_month)?)
    };
    if settlement == SettlementType::Physical && first_notice_day.is_none() {
        tracing::error!(
            root,
            contract_month,
            "physical contract with no first notice day"
        );
        return None;
    }
    Some(ContractCalendarEntry {
        root,
        contract_month,
        settlement,
        last_trade_day,
        first_notice_day,
        long_close_out: parse_date(long_out, root, contract_month)?,
        short_close_out: parse_date(short_out, root, contract_month)?,
        long_arm_by: parse_date(long_arm, root, contract_month)?,
        short_arm_by: parse_date(short_arm, root, contract_month)?,
    })
}

fn parse_date(raw: &str, root: &str, contract_month: &str) -> Option<NaiveDate> {
    match NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        Ok(d) => Some(d),
        Err(_) => {
            tracing::error!(root, contract_month, raw, "unparseable calendar date");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).expect("valid test date")
    }

    /// The anchor: a December gold contract's LONG deadline is in November.
    #[test]
    fn gc_december_long_deadline_is_a_month_before_expiry() {
        let e = lookup("GC", "202612").expect("GC Dec 2026 in the table");
        assert_eq!(e.last_trade_day, d(2026, 12, 29));
        assert_eq!(e.first_notice_day, Some(d(2026, 11, 30)));
        assert_eq!(e.long_close_out, d(2026, 11, 25));
        assert_eq!(
            e.close_out_for(Direction::Long).month(),
            11,
            "a December contract's long deadline lives in November"
        );
        // The short may stay in much longer.
        assert!(e.close_out_for(Direction::Short) > e.close_out_for(Direction::Long));
    }

    #[test]
    fn direction_changes_the_answer_on_a_physical_contract() {
        let long = close_out_deadline("GC", "202612", Direction::Long).expect("long");
        let short = close_out_deadline("GC", "202612", Direction::Short).expect("short");
        assert_ne!(
            long, short,
            "collapsing direction would permit longs a month past their deadline"
        );
    }

    #[test]
    fn cash_settled_contracts_have_one_deadline_for_both_directions() {
        let e = lookup("ES", "202612").expect("ES Dec 2026 in the table");
        assert_eq!(e.settlement, SettlementType::Cash);
        assert_eq!(e.first_notice_day, None);
        assert_eq!(e.long_close_out, e.short_close_out);
    }

    #[test]
    fn unknown_contract_is_a_refusal_not_a_pass() {
        assert!(lookup("CL", "202612").is_none(), "unknown root");
        assert!(lookup("GC", "209912").is_none(), "unknown month");
        assert!(close_out_deadline("CL", "202612", Direction::Long).is_none());
        // The is_past helper must not answer `false` (= safe) for an unknown
        // contract — that would read as permission.
        assert_eq!(
            is_past_close_out("CL", "202612", Direction::Long, d(2026, 1, 1)),
            None
        );
    }

    #[test]
    fn is_past_close_out_brackets_the_deadline() {
        // GC Dec 2026 long deadline is 2026-11-25.
        let before = is_past_close_out("GC", "202612", Direction::Long, d(2026, 11, 24));
        let on = is_past_close_out("GC", "202612", Direction::Long, d(2026, 11, 25));
        let after = is_past_close_out("GC", "202612", Direction::Long, d(2026, 11, 26));
        assert_eq!(before, Some(false));
        assert_eq!(
            on,
            Some(true),
            "the deadline day itself is inside the window"
        );
        assert_eq!(after, Some(true));
    }

    #[test]
    fn gcu6_reproduces_the_live_trap() {
        // Observed 2026-09-06: still front month, long deadline already gone.
        let observed = d(2026, 9, 6);
        let e = lookup("GC", "202610").expect("GC Oct 2026 in the table");
        assert!(
            e.last_trade_day > observed,
            "the contract still has weeks to run"
        );
        // The October contract is still safe on that date...
        assert_eq!(
            is_past_close_out("GC", "202610", Direction::Long, observed),
            Some(false)
        );
        // ...but every August contract long deadline is long past.
        assert_eq!(
            is_past_close_out("GC", "202608", Direction::Long, observed),
            Some(true)
        );
    }

    #[test]
    fn every_baked_row_parses() {
        // A malformed row yields None; if the generator ever emits one this
        // test catches it before the guard silently stops constraining.
        for row in CONTRACT_CALENDAR_BAKED {
            assert!(parse_row(row).is_some(), "row failed to parse: {:?}", row.0);
        }
        assert!(
            !CONTRACT_CALENDAR_BAKED.is_empty(),
            "the table must not be empty"
        );
    }

    #[test]
    fn every_physical_long_deadline_precedes_its_short_deadline() {
        // The invariant that makes the direction split meaningful.
        for row in CONTRACT_CALENDAR_BAKED {
            let Some(e) = parse_row(row) else { continue };
            match e.settlement {
                SettlementType::Physical => assert!(
                    e.long_close_out < e.short_close_out,
                    "{} {}: physical long must precede short",
                    e.root,
                    e.contract_month
                ),
                SettlementType::Cash => assert_eq!(
                    e.long_close_out, e.short_close_out,
                    "{} {}: cash deadlines coincide",
                    e.root, e.contract_month
                ),
            }
            assert!(
                e.short_close_out < e.last_trade_day,
                "{} {}: must be out before trading stops",
                e.root,
                e.contract_month
            );
        }
    }

    #[test]
    fn months_for_root_lists_the_traded_cycle() {
        let gc = months_for_root("GC");
        assert!(!gc.is_empty());
        assert!(gc.iter().all(|e| e.root == "GC"));
        // Gold trades the even-month cycle.
        assert!(gc.iter().all(|e| {
            let month: u32 = e.contract_month[4..].parse().expect("MM parses");
            month.is_multiple_of(2)
        }));
        assert!(months_for_root("CL").is_empty(), "unknown root ⇒ empty");
    }

    /// A hand-edited or mis-generated table with a duplicate key must resolve
    /// to a refusal, not to whichever row happens to sort first. Driven
    /// against a synthetic table because the generator makes duplicates
    /// impossible in the baked one — see `lookup_in`'s docs.
    #[test]
    fn a_duplicate_key_refuses_instead_of_taking_the_first_row() {
        static AMBIGUOUS: &[BakedRow] = &[
            (
                "GC",
                "202612",
                "physical",
                "2026-12-29",
                "2026-11-30",
                "2026-11-25",
                "2026-12-24",
                "2026-11-11",
                "2026-12-10",
            ),
            (
                "GC",
                "202612",
                "physical",
                "2026-12-29",
                "2026-11-30",
                "2027-01-01",
                "2027-01-01",
                "2026-12-17",
                "2026-12-17",
            ),
            (
                "GC",
                "202610",
                "physical",
                "2026-10-28",
                "2026-09-30",
                "2026-09-28",
                "2026-10-26",
                "2026-09-14",
                "2026-10-12",
            ),
        ];
        assert!(
            lookup_in(AMBIGUOUS, "GC", "202612").is_none(),
            "an ambiguous key must refuse, never pick a winner"
        );
        // The unambiguous row in the same table still resolves.
        assert!(lookup_in(AMBIGUOUS, "GC", "202610").is_some());
    }

    /// A row whose settlement column is unrecognised must not decode as a
    /// silently-defaulted entry.
    #[test]
    fn a_malformed_row_refuses_rather_than_defaulting() {
        static BAD_SETTLEMENT: &[BakedRow] = &[(
            "GC",
            "202612",
            "notional",
            "2026-12-29",
            "2026-11-30",
            "2026-11-25",
            "2026-12-24",
            "2026-11-11",
            "2026-12-10",
        )];
        assert!(lookup_in(BAD_SETTLEMENT, "GC", "202612").is_none());

        static BAD_DATE: &[BakedRow] = &[(
            "GC",
            "202612",
            "physical",
            "not-a-date",
            "2026-11-30",
            "2026-11-25",
            "2026-12-24",
            "2026-11-11",
            "2026-12-10",
        )];
        assert!(lookup_in(BAD_DATE, "GC", "202612").is_none());

        // A physical contract with no first notice day is incoherent: the
        // long deadline derives from it, so accepting the row would hand back
        // a deadline computed from the wrong reference.
        static PHYSICAL_NO_FND: &[BakedRow] = &[(
            "GC",
            "202612",
            "physical",
            "2026-12-29",
            "",
            "2026-11-25",
            "2026-12-24",
            "2026-11-11",
            "2026-12-10",
        )];
        assert!(lookup_in(PHYSICAL_NO_FND, "GC", "202612").is_none());
    }

    /// The arm-by date is strictly earlier than the deadline it protects, and
    /// inherits the month-early trap: a December gold LONG stops being armable
    /// in November.
    #[test]
    fn arm_by_is_head_room_before_the_deadline() {
        let e = lookup("GC", "202612").expect("GC Dec 2026");
        assert!(e.arm_by_for(Direction::Long) < e.close_out_for(Direction::Long));
        assert!(e.arm_by_for(Direction::Short) < e.close_out_for(Direction::Short));
        assert_eq!(e.arm_by_for(Direction::Long).month(), 11);
    }

    /// The direction split is the whole point: in the month between the two
    /// arm-by dates a short is still armable and a long is not. Collapsing the
    /// two would permit longs a month past their deadline.
    #[test]
    fn a_long_and_a_short_diverge_between_the_two_arm_by_dates() {
        let between = d(2026, 11, 20);
        assert_eq!(
            is_armable_on("GC", "202612", Direction::Long, between),
            Some(false),
            "a long is already unarmable"
        );
        assert_eq!(
            is_armable_on("GC", "202612", Direction::Short, between),
            Some(true),
            "the short still has a month"
        );
    }

    #[test]
    fn the_arm_by_date_itself_is_still_armable() {
        let e = lookup("GC", "202612").expect("GC Dec 2026");
        let arm_by = e.arm_by_for(Direction::Long);
        assert_eq!(
            is_armable_on("GC", "202612", Direction::Long, arm_by),
            Some(true),
            "the margin carries the head-room; don't also lose its last day"
        );
        assert_eq!(
            is_armable_on(
                "GC",
                "202612",
                Direction::Long,
                arm_by.succ_opt().expect("next day")
            ),
            Some(false)
        );
    }

    /// Unknown contracts must refuse, not answer "safe". A caller that reads
    /// `None` as "no constraint" is the failure this whole module guards.
    #[test]
    fn an_unknown_contract_refuses_rather_than_permitting() {
        assert_eq!(
            is_armable_on("ZZ", "202612", Direction::Long, d(2026, 1, 1)),
            None
        );
        assert_eq!(
            is_armable_on("GC", "209912", Direction::Long, d(2026, 1, 1)),
            None,
            "an unlisted month is as unknown as an unlisted root"
        );
        assert_eq!(arm_by_deadline("ZZ", "202612", Direction::Long), None);
    }

    #[test]
    fn micros_share_their_standard_contracts_calendar() {
        let gc = lookup("GC", "202612").expect("GC");
        let mgc = lookup("MGC", "202612").expect("MGC");
        assert_eq!(gc.last_trade_day, mgc.last_trade_day);
        assert_eq!(gc.long_close_out, mgc.long_close_out);
    }
}
