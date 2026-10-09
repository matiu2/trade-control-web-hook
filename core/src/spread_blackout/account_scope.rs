//! MT5 spread measurements belong to the named supplier account.
use std::borrow::Cow;

/// Qualify a native MT5 symbol without changing broker requests or stored symbols.
pub fn mt5_spread_key(account: &str, instrument: &str) -> String {
    format!("mt5:{account}:{instrument}")
}

/// Prefer this account's measured profile, retaining the existing non-MT5 keys.
/// MT5 rows are always qualified, so a missing account never borrows another
/// account's measurement. Uncovered symbols retain the usual coverage refusal.
pub fn spread_lookup_key<'a>(instrument: &'a str, account: Option<&str>) -> Cow<'a, str> {
    scoped_key(instrument, account, super::baseline_mt5::SPREAD_BASELINE_MT5.iter().map(|r| r.1))
}

fn scoped_key<'a>(instrument: &'a str, account: Option<&str>, rows: impl Iterator<Item = &'a str>) -> Cow<'a, str> {
    let Some(account) = account else { return Cow::Borrowed(instrument) };
    let key = mt5_spread_key(account, instrument);
    if rows.into_iter().any(|symbol| symbol == key) {
        Cow::Owned(key)
    } else {
        Cow::Borrowed(instrument)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_native_symbols_do_not_share_account_profiles() {
        let rows = ["mt5:five:EURUSD", "mt5:other:EURUSD"];
        assert_eq!(scoped_key("EURUSD", Some("five"), rows.into_iter()), rows[0]);
        assert_eq!(scoped_key("EURUSD", Some("other"), rows.into_iter()), rows[1]);
        assert_eq!(scoped_key("EURUSD", Some("missing"), rows.into_iter()), "EURUSD");
        assert_eq!(scoped_key("EURUSD", None, rows.into_iter()), "EURUSD");
    }

    #[test]
    fn existing_broker_keys_are_preserved() {
        assert_eq!(spread_lookup_key("EUR_USD", Some("m-and-w")), "EUR_USD");
        assert_eq!(spread_lookup_key("EUR/USD", Some("dev")), "EUR/USD");
    }
}
