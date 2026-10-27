//! Preserve the authorized cash-risk limit when market fills slip before normalization.
use super::*;

pub(super) fn max_stop_distance(
    risk: &RiskBudget,
    equity: f64,
    conversion: f64,
    cents: i64,
    cap: f64,
) -> std::result::Result<f64, EntryError> {
    if ![equity, conversion, cap]
        .iter()
        .all(|v| v.is_finite() && *v > 0.0)
        || cents <= 0
    {
        return Err(reject("invalid market risk snapshot"));
    }
    let requested_cash = match risk {
        RiskBudget::Percent(percent) => equity * percent / 100.0,
        RiskBudget::Amount(amount) => *amount,
        RiskBudget::Units(units) if units.is_finite() && *units > 0.0 => equity * cap / 100.0,
        RiskBudget::Units(_) => return Err(reject("invalid market unit budget")),
    };
    let allowed_cash = requested_cash.min(equity * cap / 100.0);
    let distance = allowed_cash / ((cents as f64 / 100.0) * conversion);
    if !requested_cash.is_finite()
        || requested_cash <= 0.0
        || !distance.is_finite()
        || distance <= 0.0
    {
        return Err(reject("invalid market risk distance"));
    }
    Ok(distance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    #[test]
    fn percentage_amount_and_units_retain_their_original_budget_and_cap() {
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
        for (risk, expected) in [
            (RiskBudget::Percent(1.0), 0.005),
            (RiskBudget::Amount(500.0), 0.0025),
            (RiskBudget::Units(100000.0), 0.01),
        ] {
            let limit = max_stop_distance(&risk, 100000.0, 2.0, 10000000, 2.0).unwrap();
            assert!((limit - expected).abs() < 1e-12);
        }
        assert!(
            (max_stop_distance(&RiskBudget::Percent(3.0), 100000.0, 2.0, 10000000, 2.0).unwrap()
                - 0.01)
                .abs()
                < 1e-12
        );
        assert!(
            (max_stop_distance(&RiskBudget::Amount(5000.0), 100000.0, 2.0, 10000000, 2.0).unwrap()
                - 0.01)
                .abs()
                < 1e-12
        );
        // Stepping down position volume creates genuine unused risk budget;
        // it does not authorize an arbitrary number of slippage pips.
        assert!(
            (max_stop_distance(&RiskBudget::Percent(1.0), 100000.0, 2.0, 8000000, 2.0).unwrap()
                - 0.00625)
                .abs()
                < 1e-12
        );
    }
}
