//! Statistical quality quota shared by collection, resume and promotion.
use color_eyre::{Result, eyre::ensure};
/// Require excluded / (retained + excluded) <= 1/1000 for each instrument.
/// The equivalent retained >= 999*excluded avoids summing potentially large counts.
/// If the checked product overflows, no representable retained count can meet it.
pub fn validate_fraction(n_bars: usize, excluded_minutes: usize) -> Result<()> {
    ensure!(
        excluded_minutes
            .checked_mul(999)
            .is_some_and(|required| n_bars >= required),
        "crossed-minute exclusions exceed the 0.1% per-instrument quality limit: {excluded_minutes} excluded, {n_bars} retained M1 observations"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_per_thousand_boundary_is_exact_and_overflow_safe() {
        crate::ctrader::test_logging();
        assert!(validate_fraction(999, 1).is_ok());
        assert!(validate_fraction(998, 1).is_err());
        assert!(validate_fraction(19_980, 20).is_ok());
        assert!(validate_fraction(19_979, 20).is_err());
        assert!(validate_fraction(usize::MAX, usize::MAX).is_err());
        assert!(validate_fraction(usize::MAX, 1).is_ok());
    }
    #[test]
    fn explicit_allowance_is_bounded_and_existing_five_minute_policy_is_unchanged() {
        crate::ctrader::test_logging();
        assert_eq!(
            super::super::Policy::new(20)
                .unwrap()
                .max_minutes_per_instrument,
            20
        );
        assert!(super::super::Policy::new(100).is_ok());
        assert!(super::super::Policy::new(101).is_err());
        assert!(super::super::Policy::new(0).is_err());
        let old: super::super::Policy =
            serde_json::from_str(r#"{"max_minutes_per_instrument":5}"#).unwrap();
        old.validate().unwrap();
        assert_eq!(old.max_minutes_per_instrument, 5);
    }
    #[test]
    fn zero_exclusions_preserve_every_existing_observation_threshold() {
        crate::ctrader::test_logging();
        for retained in [0, 1, 48, 999, usize::MAX] {
            assert!(validate_fraction(retained, 0).is_ok());
        }
    }
}
