//! UTC is a fixed spread-learning clock, independent of exchange session hours.
use super::*;
use chrono::{Offset, TimeZone, Timelike, Utc};

fn init_tracing() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

#[test]
fn utc_learning_clock_has_no_summer_or_winter_shift() {
    init_tracing();
    let timezone = schedule_tz("utc").unwrap();
    assert_eq!(timezone, chrono_tz::UTC);
    let mask = 1_u32 << 15;
    let mut widen = [0.0; 24];
    widen[15] = 0.002;
    for month in [1, 7] {
        let instant = Utc.with_ymd_and_hms(2026, month, 15, 15, 0, 0).unwrap();
        let local = instant.with_timezone(&timezone);
        assert_eq!(local.hour(), 15);
        assert_eq!(local.offset().fix().local_minus_utc(), 0);
        assert!(mask_active_with_lead(mask, timezone, instant));
        assert_eq!(
            spread_hour_widen_for(mask, &widen, timezone, instant, 30),
            Some(0.002)
        );
        let before = instant - chrono::Duration::hours(1);
        let after = instant + chrono::Duration::hours(1);
        assert!(!mask_active_with_lead(mask, timezone, before));
        assert!(!mask_active_with_lead(mask, timezone, after));
    }
}

#[test]
fn qualified_abbott_stock_profile_reaches_runtime_coverage_baseline_and_forecast() {
    init_tracing();
    let key = ctrader_spread_key("ctrader-demo", "ABBOTT");
    let row = baseline_ctrader::SPREAD_BASELINE_CTRADER
        .iter()
        .find(|row| row.1 == key)
        .unwrap();
    assert_eq!(spread_lookup_key("ABBOTT", Some("ctrader-demo")), key);
    assert_eq!(row.2, "utc");
    assert!(row.3);
    assert_eq!(
        row.4, 0,
        "the measured short session has no ranked spread-hour mask"
    );
    assert_eq!(coverage(&key), Coverage::Covered);
    assert_eq!(baked_baseline(&key), Some((row.7, row.8, row.6)));
    assert!(row.9[15] > 0.0 && row.9[16] > 0.0);
    for month in [1, 7] {
        let instant = Utc.with_ymd_and_hms(2026, month, 15, 15, 30, 0).unwrap();
        assert_eq!(spread_forecast_frac(&key, instant), (row.9[15], row.9[16]));
        assert!(spread_hour_widen_frac(&key, instant).is_none());
    }
}

#[test]
fn unknown_schedules_and_unmeasured_accounts_do_not_acquire_utc_stock_coverage() {
    init_tracing();
    assert_eq!(schedule_tz("none"), None);
    assert_eq!(schedule_tz("unknown-future-schedule"), None);
    let instant = Utc.with_ymd_and_hms(2026, 7, 15, 15, 30, 0).unwrap();
    for key in [
        "UNKNOWN-INSTRUMENT".to_string(),
        "ABBOTT".to_string(),
        ctrader_spread_key("future-live", "ABBOTT"),
        spread_lookup_key("UNKNOWN-INSTRUMENT", Some("ctrader-demo")).into_owned(),
        spread_lookup_key("ABBOTT", Some("future-live")).into_owned(),
    ] {
        assert_eq!(coverage(&key), Coverage::Missing, "{key}");
        assert_eq!(spread_forecast_frac(&key, instant), (0.0, 0.0), "{key}");
    }
}
