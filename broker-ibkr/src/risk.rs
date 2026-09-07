//! Pure position-sizing math for exchange-traded futures.
//!
//! The futures analogue of `broker-oanda`'s `risk.rs`, and deliberately its
//! near-twin: same defensive shape, same floor-don't-round rule, same
//! "return 0 rather than guess" discipline. What differs is the **quantum**.
//!
//! # Contracts are not units
//!
//! OANDA sizes in units, which are effectively continuous — a 33,333-unit
//! position is one unit away from a 33,334-unit one, so flooring costs
//! ~0.003% of the intended risk. A futures position is sized in **whole
//! contracts**, and one ES contract carries `50 × stop_distance` of risk. At a
//! small account the floor is not a rounding detail: it is the difference
//! between the intended trade and no trade at all.
//!
//! That is why [`contracts_for_budget`] returning `0` is a **normal, expected**
//! answer rather than an error state — the account is simply too small for this
//! setup at this stop distance. The caller turns it into
//! `EntryError::UnitsBelowMinimum`, which since v137 parks the setup and
//! re-checks it once per bar instead of failing forever.
//!
//! # The multiplier is the whole point
//!
//! `contracts = budget / (stop_distance × multiplier × fx)`
//!
//! The multiplier converts a price move into money. Omitting it from that
//! division does not produce a slightly-wrong size — it produces a size wrong
//! by exactly the multiplier: 50× for ES, 100× for GC. There is no plausible
//! default, which is why the multiplier arrives as a required argument here and
//! as a checked `Option` at the call site rather than defaulting to `1.0`.

/// Number of whole contracts a risk budget buys, floored.
///
/// - `budget` — money to risk, in **account** currency.
/// - `stop_distance` — absolute entry-to-stop distance, in **price** terms.
/// - `multiplier` — the contract multiplier: money per 1.0 of price movement,
///   per contract. ES `50`, GC `100`, MES `5`, MGC `10`.
/// - `fx_quote_to_account` — value of one unit of the contract's quote currency
///   in account currency. Pass `1.0` when they match.
///
/// Returns `0` for any input that cannot produce an honest size — non-finite,
/// zero, or negative — mirroring `units_for_budget`. `0` means "this budget
/// does not buy one contract", which the caller must treat as a refusal to
/// place, never as an unbounded or default size.
///
/// # Why floor, never round
///
/// Rounding up would place a position risking **more** than the operator
/// authorised — for ES, up to half a contract's worth, which at a 20-point stop
/// is $500 of unrequested risk. Flooring can only ever risk less than intended.
pub fn contracts_for_budget(
    budget: f64,
    stop_distance: f64,
    multiplier: f64,
    fx_quote_to_account: f64,
) -> u32 {
    if !budget.is_finite() || budget <= 0.0 {
        return 0;
    }
    if !stop_distance.is_finite() || stop_distance <= 0.0 {
        return 0;
    }
    if !multiplier.is_finite() || multiplier <= 0.0 {
        return 0;
    }
    if !fx_quote_to_account.is_finite() || fx_quote_to_account <= 0.0 {
        return 0;
    }

    let risk_per_contract = stop_distance * multiplier * fx_quote_to_account;
    if !risk_per_contract.is_finite() || risk_per_contract <= 0.0 {
        return 0;
    }

    let contracts = budget / risk_per_contract;
    if !contracts.is_finite() || contracts <= 0.0 {
        return 0;
    }
    // A budget vastly larger than one contract's risk can exceed u32 — saturate
    // rather than wrap into a small number, which is what `as u32` would do for
    // some values and is the worst possible failure here.
    contracts.floor().min(f64::from(u32::MAX)) as u32
}

/// Round a contract count down onto the exchange's order-size grid, and
/// reject anything below its minimum.
///
/// IBKR reports both numbers per contract on `ContractDetails` (`min_size`,
/// `size_increment`); for the four roots we trade both are `1`, but reading
/// them is what keeps this correct for a root where they are not — spreads and
/// some energy products quote fractional or stepped sizes.
///
/// Returns `None` when the size does not clear `min_size`, which the caller
/// maps to `UnitsBelowMinimum`. Checking the exchange's real minimum rather
/// than `== 0` is the point: a `min_size` of 2 makes a 1-contract order a
/// broker rejection, and finding that out from the exchange is better than
/// finding it out from a rejected live order.
pub fn fit_to_size_grid(contracts: u32, min_size: f64, size_increment: f64) -> Option<u32> {
    if contracts == 0 {
        return None;
    }
    let requested = f64::from(contracts);

    // A non-positive or non-finite increment means the broker told us nothing
    // usable; fall back to whole contracts rather than dividing by it.
    let increment = if size_increment.is_finite() && size_increment > 0.0 {
        size_increment
    } else {
        1.0
    };

    let steps = (requested / increment).floor();
    let fitted = steps * increment;

    // Likewise a non-positive/non-finite minimum: treat as "one contract",
    // which is the universal floor for the roots we trade.
    let minimum = if min_size.is_finite() && min_size > 0.0 {
        min_size
    } else {
        1.0
    };

    if fitted < minimum || fitted <= 0.0 {
        return None;
    }
    Some(fitted.floor().min(f64::from(u32::MAX)) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The headline case, with real numbers: ES has a multiplier of 50.
    ///
    /// $1,000 of risk, a 20-point stop. One contract risks 20 × 50 = $1,000,
    /// so the answer is exactly 1. Drop the multiplier from the division and
    /// this returns 50 — a position risking $50,000 against an authorised
    /// $1,000. That is the bug this whole argument exists to prevent.
    #[test]
    fn es_sizes_on_its_multiplier_not_on_raw_price_distance() {
        assert_eq!(contracts_for_budget(1_000.0, 20.0, 50.0, 1.0), 1);
        assert_eq!(
            contracts_for_budget(1_000.0, 20.0, 1.0, 1.0),
            50,
            "sanity: with multiplier 1.0 the same budget buys 50 — \
             this is the oversize a dropped multiplier would place"
        );
    }

    /// Live values read off the paper Gateway 2026-09-06. Each root sizes
    /// differently for the *same* budget and stop, purely via its multiplier —
    /// the micros buy 10× more contracts than their standard sibling, which is
    /// exactly why they are the tradeable size on a small account.
    #[test]
    fn every_traded_root_sizes_by_its_own_multiplier() {
        // $5,000 budget, 10-point stop.
        assert_eq!(contracts_for_budget(5_000.0, 10.0, 100.0, 1.0), 5, "GC");
        assert_eq!(contracts_for_budget(5_000.0, 10.0, 10.0, 1.0), 50, "MGC");
        assert_eq!(contracts_for_budget(5_000.0, 10.0, 50.0, 1.0), 10, "ES");
        assert_eq!(contracts_for_budget(5_000.0, 10.0, 5.0, 1.0), 100, "MES");
    }

    /// Flooring must never round up: a position risking more than authorised
    /// is the one direction that is genuinely unsafe.
    #[test]
    fn a_fractional_contract_count_floors_and_never_rounds_up() {
        // 2.9 contracts' worth of budget: 2900 / (10 * 100) = 2.9.
        assert_eq!(contracts_for_budget(2_900.0, 10.0, 100.0, 1.0), 2);
        // 2.999 — still 2. `.round()` would give 3, over-risking by ~33%.
        assert_eq!(contracts_for_budget(2_999.0, 10.0, 100.0, 1.0), 2);
    }

    /// The small-account case, and the reason Stage 7's park matters: a budget
    /// that cannot afford one contract returns 0, not a fraction and not 1.
    #[test]
    fn a_budget_too_small_for_one_contract_is_zero_not_one() {
        // $900 of risk against a GC contract risking $1,000 (10pt × 100).
        assert_eq!(contracts_for_budget(900.0, 10.0, 100.0, 1.0), 0);
        // The same account CAN afford the micro — 10× smaller multiplier.
        assert_eq!(contracts_for_budget(900.0, 10.0, 10.0, 1.0), 9);
    }

    /// Cross-currency: an AUD account trading a USD-denominated contract.
    /// The FX rate scales risk-per-contract, so it must reduce the size.
    #[test]
    fn fx_converts_contract_risk_into_account_currency() {
        // AUD account, ES (USD). 1 USD = 1.55 AUD. Budget 10,000 AUD,
        // 20-point stop ⇒ risk/contract = 20 × 50 × 1.55 = 1,550 AUD.
        // 10,000 / 1,550 = 6.45 ⇒ 6.
        assert_eq!(contracts_for_budget(10_000.0, 20.0, 50.0, 1.55), 6);
        // Same trade ignoring FX would size 10 — a 55% oversize.
        assert_eq!(contracts_for_budget(10_000.0, 20.0, 50.0, 1.0), 10);
    }

    /// Every input that cannot produce an honest number must yield 0 rather
    /// than a default, an infinity, or a panic. A `0.0` multiplier is the
    /// dangerous one: it makes risk-per-contract zero, so an unguarded
    /// division returns infinity and `as u32` would saturate to a colossal
    /// position.
    #[test]
    fn any_unusable_input_sizes_to_zero() {
        let bad = [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
        for v in bad {
            assert_eq!(contracts_for_budget(v, 10.0, 50.0, 1.0), 0, "budget {v}");
            assert_eq!(contracts_for_budget(1e6, v, 50.0, 1.0), 0, "stop {v}");
            assert_eq!(contracts_for_budget(1e6, 10.0, v, 1.0), 0, "multiplier {v}");
            assert_eq!(contracts_for_budget(1e6, 10.0, 50.0, v), 0, "fx {v}");
        }
    }

    /// An absurd budget must saturate, never wrap. `as u32` on an f64 above
    /// u32::MAX is a saturating cast in Rust, but pinning it here means a
    /// refactor to an integer path can't silently reintroduce wrapping.
    #[test]
    fn an_enormous_budget_saturates_rather_than_wrapping() {
        let n = contracts_for_budget(f64::MAX / 2.0, 0.25, 5.0, 1.0);
        assert_eq!(n, u32::MAX);
    }

    #[test]
    fn the_size_grid_passes_through_whole_contracts() {
        // The real case for all four roots: min 1, increment 1.
        assert_eq!(fit_to_size_grid(3, 1.0, 1.0), Some(3));
        assert_eq!(fit_to_size_grid(1, 1.0, 1.0), Some(1));
    }

    /// A root whose exchange minimum is above one contract must reject a
    /// single-contract order here rather than at the broker.
    #[test]
    fn a_size_below_the_exchange_minimum_is_refused() {
        assert_eq!(fit_to_size_grid(1, 2.0, 1.0), None);
        assert_eq!(fit_to_size_grid(2, 2.0, 1.0), Some(2));
    }

    /// A stepped increment floors onto the grid — 7 contracts on a 5-lot grid
    /// is 5, not 7 (which the exchange would reject) and not 10 (oversized).
    #[test]
    fn a_stepped_increment_floors_onto_the_grid() {
        assert_eq!(fit_to_size_grid(7, 1.0, 5.0), Some(5));
        assert_eq!(fit_to_size_grid(4, 1.0, 5.0), None, "below one whole step");
    }

    /// Zero contracts can never become a placeable size — the sizing already
    /// said this budget buys nothing.
    #[test]
    fn zero_contracts_stays_unplaceable() {
        assert_eq!(fit_to_size_grid(0, 1.0, 1.0), None);
        assert_eq!(fit_to_size_grid(0, 0.0, 0.0), None);
    }

    /// A broker that reports nonsense limits must not zero the order or
    /// divide by zero — fall back to whole contracts, the universal floor.
    #[test]
    fn unusable_broker_limits_fall_back_to_whole_contracts() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                fit_to_size_grid(3, bad, bad),
                Some(3),
                "limits {bad} must degrade to whole contracts"
            );
        }
    }
}
