//! Is an instrument **covered** by the baked spread table — and if not, why not?
//!
//! # Why this exists
//!
//! Every reader of the baked table degrades to a zero on a miss:
//! [`spread_forecast_frac`](super::spread_forecast_frac) returns `(0.0, 0.0)`,
//! the forecast term vanishes from the SL `max`, and the stop is sized off the
//! last bar alone. That is a *silent* degrade, and it hid a real defect: all 35
//! TradeNation rows carried an all-zero forecast for weeks while a green guard
//! test watched an OANDA symbol.
//!
//! The trap that made it invisible is that **"no row" and "a row full of zeros"
//! were indistinguishable at the call site** — both produced `(0.0, 0.0)`. This
//! module separates them, the same way the generator's `ReviewStatus` separates
//! "analysed, genuinely flat" from "never looked".
//!
//! # What it deliberately does NOT do
//!
//! It does not reject at compile time. The instrument is a `String` on
//! [`Intent`](crate::intent::Intent) and arrives from a signed alert body at
//! runtime, so the compiler never sees which instrument a plan is for. Nor can
//! `core` consult `instrument-lookup` — that dependency is deliberately absent
//! (the same decision that bakes `Intent.pip_size` instead of looking it up).
//!
//! The closest thing to a compile-time gate is therefore **arm time**: `tv-arm`
//! links `instrument-lookup`, knows the instrument, and can refuse to mint a
//! plan whose instrument this module reports as uncovered. That turns a silent
//! runtime mis-size into a loud failure in front of the operator, before any
//! plan exists. The cross-check that the table and the catalog agree lives in
//! `cli` (which links both) for the same reason.

/// Why an instrument is not fully covered by the baked spread table.
///
/// Ordered from "we know nothing" to "we know, and it is fine" so a caller can
/// treat the variants differently: [`Missing`](Self::Missing) and
/// [`StaleForecast`](Self::StaleForecast) are defects, while a `Covered`
/// instrument with an empty mask is a legitimate verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// The instrument has a row, a resolvable schedule timezone, and a
    /// populated forecast. Everything downstream can trust the table.
    Covered,
    /// No row at all. Every table reader will degrade to zero for it, so the
    /// forward-looking SL floor is inoperative and the spread-hour mask is
    /// absent. Arming against this instrument should be refused.
    Missing,
    /// A row exists but its `schedule` FK names no resolvable timezone
    /// (`"none"` or a name `schedule_tz` doesn't know). The mask cannot be
    /// indexed, so the row is inert even though it is present — a distinct
    /// failure from having no row, and it usually means the schedule table and
    /// this crate's `schedule_tz` map have drifted apart.
    NoSchedule,
    /// A row exists and resolves, but its forecast column is entirely zero
    /// while its widen column is populated — the exact self-contradiction the
    /// stale TradeNation rows exhibited. A row cannot have computed a widen
    /// without computing the per-hour p90 the forecast is rendered from, so
    /// this means the row predates the forecast column or the bake dropped it.
    StaleForecast,
}

impl Coverage {
    /// Whether the table can be trusted for this instrument.
    pub fn is_covered(self) -> bool {
        matches!(self, Coverage::Covered)
    }

    /// A short operator-facing reason, for the message a refusing caller prints.
    pub fn reason(self) -> &'static str {
        match self {
            Coverage::Covered => "covered by the baked spread table",
            Coverage::Missing => "not in the baked spread table (no row)",
            Coverage::NoSchedule => "baked row has no resolvable spread schedule timezone",
            Coverage::StaleForecast => {
                "baked row has an all-zero spread forecast (stale — needs re-baking)"
            }
        }
    }
}

/// Classify an instrument against the baked spread table.
///
/// This is the single predicate the arm-time gate, the catalog cross-check and
/// any runtime refusal all share, so "covered" means one thing everywhere.
pub fn coverage(instrument: &str) -> Coverage {
    let Some(row) = super::baseline_candle::SPREAD_BASELINE_CANDLE
        .iter()
        .find(|(_broker, symbol, ..)| *symbol == instrument)
    else {
        return Coverage::Missing;
    };
    let (_broker, _symbol, schedule, _reviewed, _mask, widen, _m, _l, _h, forecast) = row;
    if super::schedule_tz(schedule).is_none() {
        return Coverage::NoSchedule;
    }
    // A populated widen with an empty forecast is the stale-row signature. A row
    // that is flat on BOTH is a legitimate "genuinely no spread hour" verdict and
    // must not be flagged — that is what `reviewed` asserts.
    if forecast.iter().all(|f| *f <= 0.0) && widen.iter().any(|w| *w > 0.0) {
        return Coverage::StaleForecast;
    }
    Coverage::Covered
}

/// Every instrument symbol present in the baked table, in table order.
///
/// Exposed so consumers that DO link a catalog (`cli`, `tv-arm`) can cross-check
/// the two directions — a catalog asset with no row, and a row naming an asset
/// the catalog no longer lists.
pub fn baked_symbols() -> impl Iterator<Item = &'static str> {
    super::baseline_candle::SPREAD_BASELINE_CANDLE
        .iter()
        .map(|(_broker, symbol, ..)| *symbol)
}

/// Every `(broker, symbol)` pair in the baked table — the table's true key.
///
/// `baked_symbols` alone is ambiguous when two brokers list the same string;
/// the table is sorted and deduplicated on this pair, not on the symbol.
pub fn baked_rows() -> impl Iterator<Item = (&'static str, &'static str)> {
    super::baseline_candle::SPREAD_BASELINE_CANDLE
        .iter()
        .map(|(broker, symbol, ..)| (*broker, *symbol))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-known OANDA row is covered — the positive control, so a test
    /// suite that goes green proves the predicate can say "yes".
    #[test]
    fn a_baked_oanda_instrument_is_covered() {
        assert_eq!(coverage("EUR_USD"), Coverage::Covered);
        assert!(coverage("EUR_USD").is_covered());
    }

    /// An instrument that was never baked is `Missing`, NOT silently fine.
    /// `UK 100` and `Coffee` are real corpus instruments in exactly this state.
    #[test]
    fn an_unbaked_instrument_is_missing() {
        assert_eq!(coverage("NOT_A_REAL_PAIR"), Coverage::Missing);
        assert!(!coverage("NOT_A_REAL_PAIR").is_covered());
    }

    /// The classifier applied to a hand-built row, so the four verdicts are
    /// exercised regardless of what the committed table currently contains.
    ///
    /// `coverage` reads a `&'static` table, so it cannot be pointed at a
    /// fixture. This mirrors its decision over an owned row instead; the
    /// `classify_agrees_with_coverage_on_the_real_table` test below pins the two
    /// together so this mirror cannot drift into testing only itself.
    fn classify(schedule: &str, widen: &[f64; 24], forecast: &[f64; 24]) -> Coverage {
        if super::super::schedule_tz(schedule).is_none() {
            return Coverage::NoSchedule;
        }
        if forecast.iter().all(|f| *f <= 0.0) && widen.iter().any(|w| *w > 0.0) {
            return Coverage::StaleForecast;
        }
        Coverage::Covered
    }

    /// The distinction this module exists for: a MISSING row and a STALE row
    /// must not look alike. Both make `spread_forecast_frac` return zeros, so
    /// without this they are indistinguishable downstream — which is precisely
    /// how 35 stale rows hid behind a green test.
    ///
    /// Built from an explicit stale-shaped row rather than by filtering the live
    /// table: once the table is fully re-baked no row is stale, and a filter
    /// would yield an empty list that passes vacuously — proving nothing exactly
    /// when the regression it guards would return.
    #[test]
    fn missing_and_stale_are_distinguishable_though_both_forecast_zero() {
        let mut widen = [0.0_f64; 24];
        widen[17] = 0.0015; // a populated widen …
        let forecast = [0.0_f64; 24]; // … with an empty forecast: the stale signature.

        assert_eq!(
            classify("ny", &widen, &forecast),
            Coverage::StaleForecast,
            "a populated widen with a zero forecast must read as STALE, not as \
             missing and not as covered",
        );
        assert_eq!(
            coverage("NOT_A_REAL_PAIR"),
            Coverage::Missing,
            "…while a row that does not exist reads as MISSING",
        );
        assert_ne!(
            classify("ny", &widen, &forecast),
            Coverage::Missing,
            "the two defects must stay distinguishable — collapsing them is what \
             let 35 stale rows hide behind a green test",
        );
    }

    /// Ties the mirror above to the real classifier: for every row in the
    /// committed table, `classify` and `coverage` must agree. Without this the
    /// mirror could drift and the distinction test would be checking only
    /// itself.
    #[test]
    fn classify_agrees_with_coverage_on_the_real_table() {
        for row in super::super::baseline_candle::SPREAD_BASELINE_CANDLE.iter() {
            let (_broker, symbol, schedule, _reviewed, _mask, widen, _m, _l, _h, forecast) = row;
            assert_eq!(
                classify(schedule, widen, forecast),
                coverage(symbol),
                "mirror disagrees with the classifier for {symbol}",
            );
        }
    }

    /// A stale row forecasts zero exactly like a missing one — the precondition
    /// that makes the distinction necessary rather than academic.
    #[test]
    fn a_stale_row_forecasts_zero_just_like_a_missing_one() {
        let at = chrono::DateTime::from_timestamp(1_781_500_000, 0)
            .unwrap_or(chrono::DateTime::UNIX_EPOCH);
        for s in baked_symbols().filter(|s| coverage(s) == Coverage::StaleForecast) {
            assert_eq!(
                super::super::spread_forecast_frac(s, at),
                (0.0, 0.0),
                "{s} is stale, so its forecast must read zero",
            );
        }
        assert_eq!(
            super::super::spread_forecast_frac("NOT_A_REAL_PAIR", at),
            (0.0, 0.0),
            "a missing instrument also reads zero — hence the ambiguity",
        );
    }

    /// A row that is flat on BOTH columns is a legitimate verdict ("reviewed,
    /// genuinely no spread hour"), not staleness. Flagging it would make the
    /// gate cry wolf on every calm instrument.
    #[test]
    fn a_genuinely_flat_row_is_covered_not_stale() {
        let flat_but_covered = baked_symbols().find(|s| {
            super::super::baseline_candle::SPREAD_BASELINE_CANDLE
                .iter()
                .find(|(_b, sym, ..)| sym == s)
                .is_some_and(|(_b, _s, _sch, _r, mask, _w, ..)| *mask == 0)
                && coverage(s) == Coverage::Covered
        });
        assert!(
            flat_but_covered.is_some(),
            "at least one empty-mask row must classify as Covered, else the \
             predicate conflates 'calm' with 'stale'",
        );
    }

    /// Every baked row's schedule must resolve, or its mask is inert. This
    /// catches drift between the catalog's schedule table and `schedule_tz`.
    #[test]
    fn no_baked_row_has_an_unresolvable_schedule() {
        let broken: Vec<&str> = baked_symbols()
            .filter(|s| coverage(s) == Coverage::NoSchedule)
            .collect();
        assert!(
            broken.is_empty(),
            "baked rows whose schedule FK resolves to no timezone (their mask \
             can never fire): {broken:?}",
        );
    }
}
