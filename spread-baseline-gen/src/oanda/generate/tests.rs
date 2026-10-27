use super::*;

fn init_tracing() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

#[test]
fn oanda_native_identity_selects_the_correct_alias_schedule_and_precision() {
    init_tracing();
    let items = catalogue_items(&["EU50_EUR".into()], None).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].spread_schedule, "frankfurt");
    assert_eq!(
        items[0].spread_schedule_tz.as_deref(),
        Some("Europe/Berlin")
    );
    let native = instrument_lookup::by_broker_symbol_instrument(
        instrument_lookup::Broker::Oanda,
        "EU50_EUR",
    )
    .unwrap()
    .unwrap();
    assert_eq!(items[0].pip_size, native.pip_size);
}

#[test]
fn thin_or_stale_checkpoint_measurements_are_retried() {
    init_tracing();
    use chrono::TimeZone;
    let items = catalogue_items(&["EUR_USD".into()], None).unwrap();
    let from = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let to = from + Duration::days(90);
    let mut row = Measurement {
        symbol: items[0].symbol.clone(),
        schedule: items[0].spread_schedule.clone(),
        pip_size: items[0].pip_size,
        first_utc: from,
        last_utc: to - Duration::hours(1),
        observed_utc_days: 64,
        profile: crate::compute::SpreadProfile::empty(90_000),
    };
    assert!(
        !reusable_measurement(&row, &items, from, to),
        "InsufficientData must retry"
    );
    row.profile.review = crate::compute::ReviewStatus::Reviewed;
    row.profile.baseline_low_pips = 0.7;
    row.profile.baseline_median_pips = 0.8;
    row.profile.baseline_high_pips = 1.0;
    assert!(reusable_measurement(&row, &items, from, to));
    row.observed_utc_days = 0;
    assert!(
        !reusable_measurement(&row, &items, from, to),
        "Old checkpoint without date coverage must retry"
    );
    row.observed_utc_days = 64;
    row.pip_size *= 10.0;
    assert!(
        !reusable_measurement(&row, &items, from, to),
        "Changed instrument precision must recompute from cached books"
    );
}
