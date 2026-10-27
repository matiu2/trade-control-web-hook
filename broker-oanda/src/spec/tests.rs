use super::*;

#[test]
fn fractional_crypto_and_indices_floor_to_broker_steps() {
    crate::order::tests::logging();
    let btc = Spec {
        price_places: 1,
        unit_places: 3,
        minimum: 0.001,
        maximum: 100.0,
    };
    assert_eq!(btc.units(0.0129).unwrap(), 0.012);
    assert_eq!(btc.signed_units(0.012, false), "-0.012");
    assert_eq!(btc.price(12345.6), "12345.6");
    assert!(btc.units(0.00099).is_err());
    assert!(btc.units(f64::NAN).is_err());
    assert!(btc.units(100.001).is_err());
    let index = Spec {
        price_places: 1,
        unit_places: 1,
        minimum: 0.1,
        maximum: 1000.0,
    };
    assert_eq!(index.units(0.95).unwrap(), 0.9);
    assert_eq!(index.signed_units(0.9, true), "0.9");
    let fx = Spec {
        price_places: 5,
        unit_places: 0,
        minimum: 1.0,
        maximum: 1e8,
    };
    assert_eq!(fx.units(1234.99).unwrap(), 1234.0);
    assert_eq!(fx.signed_units(1234.0, true), "1234");
}
