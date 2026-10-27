//! Platform spread measurements belong to the named supplier account.
use std::borrow::Cow;

/// Qualify a native MT5 symbol without changing broker requests or stored symbols.
pub fn mt5_spread_key(account: &str, instrument: &str) -> String {
    format!("mt5:{account}:{instrument}")
}

pub fn ctrader_spread_key(account: &str, instrument: &str) -> String {
    format!("ctrader:{account}:{instrument}")
}

/// Prefer this supplier account's measured profile. Calibrated OANDA and cTrader
/// accounts remain qualified even for uncovered symbols; demo OANDA retains its
/// existing native-symbol profiles. No measured account borrows another's rows.
pub fn spread_lookup_key<'a>(instrument: &'a str, account: Option<&str>) -> Cow<'a, str> {
    scoped_key(
        instrument,
        account,
        super::baseline_ctrader::SPREAD_BASELINE_CTRADER
            .iter()
            .chain(super::baseline_mt5::SPREAD_BASELINE_MT5.iter())
            .chain(super::baseline_oanda::SPREAD_BASELINE_OANDA.iter())
            .map(|r| r.1),
    )
}

fn scoped_key<'a>(
    instrument: &'a str,
    account: Option<&str>,
    rows: impl Iterator<Item = &'a str>,
) -> Cow<'a, str> {
    let Some(account) = account else {
        return Cow::Borrowed(instrument);
    };
    let mt5_key = mt5_spread_key(account, instrument);
    let ctrader_prefix = format!("ctrader:{account}:");
    let oanda_prefix = format!("oanda:{account}:");
    rows.into_iter()
        .find_map(|symbol| {
            if symbol.starts_with(&oanda_prefix) {
                Some(Cow::Owned(format!("{oanda_prefix}{instrument}")))
            } else if symbol.starts_with(&ctrader_prefix) {
                // Every symbol for a measured cTrader account stays qualified,
                // including uncovered symbols. Never borrow a global profile.
                Some(Cow::Owned(ctrader_spread_key(account, instrument)))
            } else if symbol == mt5_key {
                Some(Cow::Owned(mt5_key.clone()))
            } else {
                None
            }
        })
        .unwrap_or(Cow::Borrowed(instrument))
}

#[cfg(test)]
mod tests {
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
    fn identical_native_symbols_do_not_share_account_profiles() {
        let rows = ["mt5:five:EURUSD", "mt5:other:EURUSD"];
        assert_eq!(
            scoped_key("EURUSD", Some("five"), rows.into_iter()),
            rows[0]
        );
        assert_eq!(
            scoped_key("EURUSD", Some("other"), rows.into_iter()),
            rows[1]
        );
        assert_eq!(
            scoped_key("EURUSD", Some("missing"), rows.into_iter()),
            "EURUSD"
        );
        assert_eq!(scoped_key("EURUSD", None, rows.into_iter()), "EURUSD");
    }

    #[test]
    fn existing_broker_keys_are_preserved() {
        assert_eq!(spread_lookup_key("EUR_USD", Some("m-and-w")), "EUR_USD");
        assert_eq!(spread_lookup_key("EUR/USD", Some("dev")), "EUR/USD");
    }

    #[test]
    fn account_scoped_oanda_never_borrows_the_generic_demo_profile() {
        init_tracing();
        let rows = ["oanda:oanda-live:EUR_USD", "oanda:other-live:EUR_USD"];
        assert_eq!(
            scoped_key("EUR_USD", Some("oanda-live"), rows.into_iter()),
            rows[0]
        );
        assert_eq!(
            scoped_key("EUR_USD", Some("other-live"), rows.into_iter()),
            rows[1]
        );
        assert_eq!(
            scoped_key("UNKNOWN", Some("oanda-live"), rows.into_iter()),
            "oanda:oanda-live:UNKNOWN"
        );
        assert_eq!(
            scoped_key("EUR_USD", Some("m-and-w"), rows.into_iter()),
            "EUR_USD"
        );
    }

    #[test]
    fn live_oanda_profiles_reach_every_shared_runtime_reader() {
        init_tracing();
        use chrono::{TimeZone, Timelike};
        let rows = super::super::baseline_oanda::SPREAD_BASELINE_OANDA;
        assert!(
            !rows.is_empty(),
            "The live account needs completed spread measurements"
        );
        let instant = chrono::Utc
            .with_ymd_and_hms(2026, 10, 26, 12, 0, 0)
            .unwrap();
        for row in rows {
            let (account, symbol) = row
                .1
                .strip_prefix("oanda:")
                .unwrap()
                .split_once(':')
                .unwrap();
            assert_eq!(spread_lookup_key(symbol, Some(account)), row.1);
            assert_eq!(spread_lookup_key(symbol, Some("m-and-w")), symbol);
            assert!(super::super::coverage(row.1).is_covered());
            assert_eq!(
                super::super::baked_baseline(row.1),
                Some((row.7, row.8, row.6))
            );
            let hour = instant
                .with_timezone(&super::super::schedule_tz(row.2).unwrap())
                .hour() as usize;
            let expected = |hour: usize| {
                if row.4 & (1 << hour) == 0 {
                    row.9[hour]
                } else {
                    0.0
                }
            };
            assert_eq!(
                super::super::spread_forecast_frac(row.1, instant),
                (expected(hour), expected((hour + 1) % 24))
            );
        }
        assert_eq!(
            super::super::coverage(&spread_lookup_key("UNKNOWN", Some("oanda-live"))),
            super::super::Coverage::Missing
        );
    }

    #[test]
    fn ctrader_account_rows_never_share_quotes_with_other_accounts_or_plain_symbols() {
        let rows = ["ctrader:ctrader-demo:EURUSD", "mt5:five:EURUSD"];
        assert_eq!(
            scoped_key("EURUSD", Some("ctrader-demo"), rows.into_iter()),
            "ctrader:ctrader-demo:EURUSD"
        );
        assert_eq!(
            scoped_key("EUR/USD", Some("ctrader-demo"), rows.into_iter()),
            "ctrader:ctrader-demo:EUR/USD"
        );
        assert_eq!(
            scoped_key("EURUSD", Some("future-live"), rows.into_iter()),
            "EURUSD"
        );
        assert_eq!(
            scoped_key("EURUSD", Some("five"), rows.into_iter()),
            "mt5:five:EURUSD"
        );
    }

    #[test]
    fn every_baked_mt5_profile_is_reachable_only_through_its_account_key() {
        use chrono::{TimeZone, Timelike};
        for row in super::super::baseline_mt5::SPREAD_BASELINE_MT5 {
            let (account, native) = row.1.strip_prefix("mt5:").unwrap().split_once(':').unwrap();
            assert_eq!(spread_lookup_key(native, Some(account)), row.1);
            assert!(super::super::coverage(row.1).is_covered());
            assert_eq!(
                super::super::coverage(native),
                super::super::Coverage::Missing
            );
            assert_ne!(spread_lookup_key(native, Some("unmeasured-account")), row.1);
            let timezone = super::super::schedule_tz(row.2).unwrap();
            let instant = chrono::Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap();
            let hour = instant.with_timezone(&timezone).hour() as usize;
            let expected = |h: usize| {
                if row.4 & (1_u32 << h) == 0 {
                    row.9[h]
                } else {
                    0.0
                }
            };
            assert_eq!(
                super::super::spread_forecast_frac(row.1, instant),
                (expected(hour), expected((hour + 1) % 24))
            );
        }
    }

    #[test]
    fn baked_ctrader_profiles_reach_every_shared_runtime_reader() {
        use chrono::{TimeZone, Timelike};
        for row in super::super::baseline_ctrader::SPREAD_BASELINE_CTRADER {
            let (account, native) = row
                .1
                .strip_prefix("ctrader:")
                .unwrap()
                .split_once(':')
                .unwrap();
            assert_eq!(spread_lookup_key(native, Some(account)), row.1);
            assert!(super::super::coverage(row.1).is_covered());
            assert_eq!(
                super::super::baked_baseline(row.1),
                Some((row.7, row.8, row.6))
            );
            let timezone = super::super::schedule_tz(row.2).unwrap();
            let instant = chrono::Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap();
            let hour = instant.with_timezone(&timezone).hour() as usize;
            let expected = |h: usize| {
                if row.4 & (1_u32 << h) == 0 {
                    row.9[h]
                } else {
                    0.0
                }
            };
            assert_eq!(
                super::super::spread_forecast_frac(row.1, instant),
                (expected(hour), expected((hour + 1) % 24))
            );
            assert_ne!(spread_lookup_key(native, Some("future-live")), row.1);
        }
    }
}
