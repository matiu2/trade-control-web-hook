use super::*;
use crate::tests::logging;

#[test]
fn linear_contracts_size_fx_metals_indices_oil_crypto_and_stocks() {
    logging();
    // mode, contract size, entry, stop, conversion, account-currency risk per lot
    [
        (0, 100_000.0, 1.59, 1.592, 0.73, 146.0),
        (5, 100_000.0, 1.59, 1.592, 0.73, 146.0),
        (2, 5_000.0, 32.0, 31.8, 1.0, 1_000.0),
        (2, 100.0, 2_600.0, 2_590.0, 1.0, 1_000.0),
        (3, 1.0, 5_700.0, 5_680.0, 1.0, 20.0),
        (4, 1_000.0, 70.0, 69.5, 1.0, 500.0),
        (2, 1.0, 60_000.0, 59_500.0, 1.0, 500.0),
        (32, 1.0, 220.0, 218.0, 1.0, 2.0),
        (38, 1.0, 220.0, 218.0, 0.01, 0.02),
    ]
    .into_iter()
    .for_each(|(mode, size, entry, stop, rate, expected)| {
        let loss = loss_per_lot(mode, size, entry, stop, rate).expect("linear contract");
        assert!((loss - expected).abs() < 1e-8, "mode {mode}: {loss}");
        let lots = crate::sizing::lots(
            trade_control_core::intent::RiskBudget::Amount(10.0),
            10_000.0,
            loss,
            1.0,
            &crate::tests::rules(),
        )
        .expect("budget buys at least the minimum lot");
        assert!(lots.as_f64() * loss <= 10.0 + 1e-8);
    });
}

#[test]
fn unsupported_profit_formulas_are_refused() {
    logging();
    [1, 6, 33, 34, 35, 36, 37, 39, 64, u32::MAX]
        .into_iter()
        .for_each(|mode| {
            assert!(matches!(
                loss_per_lot(mode, 100.0, 32.0, 31.8, 1.0),
                Err(EntryError::ContractSizeUnavailable)
            ));
        });
}

#[test]
fn invalid_metadata_prices_conversion_and_zero_risk_are_refused() {
    logging();
    [0.0, -1.0, f64::NAN, f64::INFINITY]
        .into_iter()
        .for_each(|invalid| {
            assert!(loss_per_lot(0, invalid, 32.0, 31.8, 1.0).is_err());
            assert!(loss_per_lot(0, 5_000.0, invalid, 31.8, 1.0).is_err());
            assert!(loss_per_lot(0, 5_000.0, 32.0, invalid, 1.0).is_err());
            assert!(loss_per_lot(0, 5_000.0, 32.0, 31.8, invalid).is_err());
        });
    assert!(loss_per_lot(0, 5_000.0, 32.0, 32.0, 1.0).is_err());
    assert!(loss_per_lot(0, f64::MAX, 32.0, 31.8, f64::MAX).is_err());
}
