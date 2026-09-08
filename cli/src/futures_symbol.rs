//! Recognise a futures instrument and split it into `(root, contract_month)`.
//!
//! # Why parse the instrument at all
//!
//! The close-out guard has to know two things: *is this a futures contract?*
//! and *which contract month?* Neither is available anywhere else at arm time.
//! `BrokerKind` cannot answer the first (a futures broker could in principle
//! list CFDs, and `BrokerKind::Ibkr` does not exist yet), and
//! `instrument-lookup` has no contract-month dimension at all — its canonical
//! ids are flat symbols, one per broker, which is exactly the gap the scoping
//! doc names ("IBKR instrument identity is `conId + exchange + expiry`, not a
//! flat symbol").
//!
//! So the contract month travels in the instrument string, and this module is
//! the single place that reads it.
//!
//! # Accepted spellings
//!
//! Two, deliberately:
//!
//! - **`GCZ6`** — IBKR's own `local_symbol`: root, a single month letter, and
//!   the last digit of the year. This is what the Gateway reports back, so it
//!   is what an operator copying off a chain will type.
//! - **`GC 202612`** — root and explicit `YYYYMM`, the form IBKR reports as
//!   `contract_month` and the form the baked calendar is keyed on. Unambiguous
//!   about the decade, so it is the spelling to prefer in written plans.
//!
//! Both normalise to the same `(root, "YYYYMM")` key.
//!
//! # Why the single-digit year needs a pivot, and why that is safe
//!
//! `GCZ6` does not say *which* 6. The convention is resolved against a
//! reference year: pick the nearest year ending in that digit, searching
//! forward first. A futures plan is armed weeks-to-months ahead, never a
//! decade, so forward-first is right; and a wrong decade cannot slip through
//! as a *permissive* answer, because the resolved month either exists in the
//! baked calendar (which spans a few years) or it does not and the guard
//! refuses. The failure mode is a refusal, not a bad deadline.
//!
//! # What must NOT parse
//!
//! Every CFD/spot instrument the system trades today: `AUD/CAD`, `EUR_USD`,
//! `Spot Gold`, `US 500`. If one of those ever parsed as futures, the guard
//! would demand a contract calendar entry for it and refuse to arm a
//! perfectly good CFD plan. The tests pin the whole existing vocabulary.

/// A futures instrument split into the two columns the calendar is keyed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuturesContract {
    /// Root symbol, upper-case — `"GC"`, `"ES"`, `"MGC"`, `"MES"`.
    pub root: String,
    /// `"YYYYMM"`, matching the baked calendar's key and IBKR's
    /// `contract_month`.
    pub contract_month: String,
}

/// CME month codes. Position in the array is the month, 1-based.
///
/// Note `I`, `L`, `O`, `W`, `Y` and `A`–`E` are not month codes; only these
/// twelve letters are, which is part of what keeps false positives rare.
const MONTH_CODES: [char; 12] = ['F', 'G', 'H', 'J', 'K', 'M', 'N', 'Q', 'U', 'V', 'X', 'Z'];

/// Month number for a CME month code, or `None` if it isn't one.
fn month_for_code(c: char) -> Option<u32> {
    MONTH_CODES
        .iter()
        .position(|&m| m == c)
        .map(|i| i as u32 + 1)
}

/// Parse `instrument` as a futures contract, or `None` if it isn't one.
///
/// `reference_year` resolves the single-digit year in the `GCZ6` form — pass
/// the arming date's year. It is unused by the explicit `GC 202612` form.
pub fn parse(instrument: &str, reference_year: i32) -> Option<FuturesContract> {
    let s = instrument.trim();
    if s.is_empty() {
        return None;
    }
    parse_explicit_month(s).or_else(|| parse_local_symbol(s, reference_year))
}

/// `"GC 202612"` / `"GC-202612"` — root and an explicit `YYYYMM`.
fn parse_explicit_month(s: &str) -> Option<FuturesContract> {
    let (root, month) = s
        .split_once([' ', '-'])
        .map(|(r, m)| (r.trim(), m.trim()))?;
    if !is_root(root) || month.len() != 6 || !month.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // Reject an impossible month outright rather than handing the calendar a
    // key it can only answer "unknown" to — the operator mistyped, and saying
    // so beats "contract not in the calendar".
    let mm: u32 = month.get(4..6)?.parse().ok()?;
    if !(1..=12).contains(&mm) {
        return None;
    }
    Some(FuturesContract {
        root: root.to_ascii_uppercase(),
        contract_month: month.to_string(),
    })
}

/// `"GCZ6"` — IBKR's `local_symbol`: root, month code, last digit of the year.
fn parse_local_symbol(s: &str, reference_year: i32) -> Option<FuturesContract> {
    if s.len() < 3 || !s.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let year_digit = s.chars().last()?.to_digit(10)?;
    let without_year = s.get(..s.len() - 1)?;
    let code = without_year.chars().last()?.to_ascii_uppercase();
    let month = month_for_code(code)?;
    let root = without_year.get(..without_year.len() - 1)?;
    if !is_root(root) {
        return None;
    }
    let year = nearest_year_ending_in(year_digit, reference_year);
    Some(FuturesContract {
        root: root.to_ascii_uppercase(),
        contract_month: format!("{year:04}{month:02}"),
    })
}

/// Is this a plausible futures root — 1-4 ASCII letters?
///
/// Length-bounded so a long alphabetic name can't be sliced into a root plus a
/// coincidental month letter.
fn is_root(s: &str) -> bool {
    !s.is_empty() && s.len() <= 4 && s.chars().all(|c| c.is_ascii_alphabetic())
}

/// The year nearest `reference` whose last digit is `digit`, searching forward
/// before back. See the module docs for why forward-first is the right bias.
fn nearest_year_ending_in(digit: u32, reference: i32) -> i32 {
    let digit = digit as i32;
    let base = reference - reference.rem_euclid(10) + digit;
    if base >= reference { base } else { base + 10 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(root: &str, month: &str) -> Option<FuturesContract> {
        Some(FuturesContract {
            root: root.to_string(),
            contract_month: month.to_string(),
        })
    }

    #[test]
    fn parses_ibkr_local_symbols() {
        // Z = December, and 6 resolves forward to 2026 from a 2026 reference.
        assert_eq!(parse("GCZ6", 2026), c("GC", "202612"));
        assert_eq!(parse("ESU6", 2026), c("ES", "202609"));
        assert_eq!(parse("MGCV6", 2026), c("MGC", "202610"));
        assert_eq!(parse("MESH7", 2026), c("MES", "202703"));
    }

    #[test]
    fn parses_explicit_contract_months() {
        assert_eq!(parse("GC 202612", 2026), c("GC", "202612"));
        assert_eq!(parse("MES-202703", 2026), c("MES", "202703"));
        assert_eq!(parse("  es 202609  ", 2026), c("ES", "202609"));
    }

    #[test]
    fn both_spellings_agree() {
        assert_eq!(parse("GCZ6", 2026), parse("GC 202612", 2026));
    }

    /// The guard must be invisible to every instrument traded today. If one of
    /// these ever parsed, arming a normal CFD plan would start demanding a
    /// contract calendar entry and refusing when it found none.
    #[test]
    fn existing_cfd_and_spot_instruments_are_not_futures() {
        for name in [
            // FX, both broker spellings.
            "AUD/CAD",
            "EUR/USD",
            "GBP/NZD",
            "USD_CAD",
            "EUR_USD",
            "CHF/JPY",
            // TradeNation display names.
            "Spot Gold",
            "Spot Silver",
            "US 500",
            "Australia 200",
            // Indices, across the naming styles brokers use. These are the
            // interesting ones: an index CFD is a bare alphanumeric string
            // like a local_symbol, so it is the only family that could
            // plausibly collide.
            "NAS100",
            "US30",
            "GER40",
            "UK100",
            "JPN225",
            "EU50",
            "HK50",
            "SPX500",
            "AUS200",
            "FRA40",
            "ESP35",
            "SWI20",
            "CH20",
            "SMI",
            "US500",
            "US2000",
            // Metals, energy, crypto.
            "XAUUSD",
            "XAGUSD",
            "WTI",
            "BRENT",
            "NATGAS",
            "COPPER",
            "GOLD",
            "SILVER",
            "USOIL",
            "UKOIL",
            "BTCUSD",
            "ETHUSD",
            "DASHCUSD",
            "SOLUSD",
        ] {
            assert_eq!(parse(name, 2026), None, "{name} must not parse as futures");
        }
    }

    #[test]
    fn rejects_things_that_only_look_like_contracts() {
        // Not a month code (I, L, O are deliberately not CME codes).
        assert_eq!(parse("GCI6", 2026), None);
        // No year digit.
        assert_eq!(parse("GCZ", 2026), None);
        // No root left after stripping code + year.
        assert_eq!(parse("Z6", 2026), None);
        // Root too long to be a root.
        assert_eq!(parse("LONGROOTZ6", 2026), None);
        // Explicit month out of range.
        assert_eq!(parse("GC 202613", 2026), None);
        assert_eq!(parse("GC 202600", 2026), None);
        // Wrong width.
        assert_eq!(parse("GC 20261", 2026), None);
        assert_eq!(parse("", 2026), None);
        assert_eq!(parse("   ", 2026), None);
    }

    #[test]
    fn the_year_digit_resolves_forward_first() {
        // From 2026, "6" is this year, not 2016 or 2036.
        assert_eq!(nearest_year_ending_in(6, 2026), 2026);
        // "1" is ahead: 2031, not 2021 — a plan is armed forward in time.
        assert_eq!(nearest_year_ending_in(1, 2026), 2031);
        // Across a decade boundary.
        assert_eq!(nearest_year_ending_in(0, 2029), 2030);
        assert_eq!(nearest_year_ending_in(9, 2029), 2029);
    }

    /// A wrong decade cannot become a permissive answer: it resolves to a
    /// month the calendar does not list, and an unlisted month is a refusal.
    #[test]
    fn an_out_of_range_year_lands_outside_the_calendar() {
        let far = parse("GCZ1", 2026).expect("parses");
        assert_eq!(far.contract_month, "203112");
        assert_eq!(
            trade_control_core::contract_calendar::lookup(&far.root, &far.contract_month),
            None,
            "outside the baked span, so the guard refuses rather than guessing"
        );
    }

    /// The forward bias means a contract that has already rolled past cannot be
    /// re-armed by accident: arming in 2027 on last year's `GCZ6` resolves to
    /// 2036, which is outside the calendar, so the guard refuses. Refusing an
    /// expired contract is the right answer; this pins that it is reached
    /// deliberately rather than by luck.
    #[test]
    fn a_contract_that_already_rolled_resolves_out_of_range() {
        let stale = parse("GCZ6", 2027).expect("parses");
        assert_eq!(stale.contract_month, "203612");
        assert_eq!(
            trade_control_core::contract_calendar::lookup(&stale.root, &stale.contract_month),
            None,
        );
        // Whereas the contract genuinely ahead of a Dec-2026 arming resolves
        // across the year boundary as you would want.
        assert_eq!(parse("GCG7", 2026), c("GC", "202702"));
    }

    /// Non-ASCII instrument names refuse cleanly rather than panicking.
    ///
    /// Worth stating plainly what this does and does not prove. The byte
    /// slicing below cannot be reached with a multibyte *final* character:
    /// the last char must pass `to_digit(10)` first, and no multibyte char is
    /// a digit. Both the `is_ascii_alphanumeric` guard and `str::get`
    /// (vs `&s[..n]`) are therefore belt-and-braces — removing either one
    /// leaves this test passing, which was verified rather than assumed.
    ///
    /// So this is a behaviour test, not a panic-safety tripwire: an operator
    /// pasting a non-ASCII name gets a refusal, and refusal is the correct
    /// answer for a name no futures contract uses.
    #[test]
    fn non_ascii_names_refuse() {
        // The last char must itself be multibyte to reach the byte-boundary
        // slice — an ASCII-terminated string like "GC€6" exits earlier and
        // proves nothing.
        for name in ["GCZ€", "GC6é", "GCZ日", "日経225", "GCZ６", "café", "GC€6"] {
            assert_eq!(parse(name, 2026), None, "{name} must refuse");
        }
    }

    #[test]
    fn roots_normalise_to_upper_case() {
        assert_eq!(parse("gcz6", 2026), c("GC", "202612"));
        assert_eq!(parse("gc 202612", 2026), c("GC", "202612"));
    }
}
