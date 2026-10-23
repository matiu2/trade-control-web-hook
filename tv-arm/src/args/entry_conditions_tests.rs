use super::Args;
use clap::Parser;

#[test]
fn explicit_ma_list_and_convenience_alias_resolve_to_the_same_set() {
    let explicit = Args::try_parse_from(["tv-arm", "--must-touch", "9 ema|20 sma|200 sma"])
        .unwrap()
        .apply_aliases();
    let alias = Args::try_parse_from(["tv-arm", "--ma-touch-required"])
        .unwrap()
        .apply_aliases();
    assert_eq!(explicit.must_touch, alias.must_touch);
    assert_eq!(explicit.must_touch.len(), 3);
    assert!(!alias.trend, "MA touch also works for normal H&S");
}

#[test]
fn ma_list_rejects_empty_invalid_or_ambiguous_requests() {
    for value in ["", "9 ema|", "0 ema", "9 ema|20 wma"] {
        assert!(
            Args::try_parse_from(["tv-arm", "--must-touch", value]).is_err(),
            "accepted {value:?}"
        );
    }
    assert!(
        Args::try_parse_from(["tv-arm", "--must-touch", "9 ema", "--ma-touch-required"]).is_err()
    );
}

#[test]
fn direct_position_entry_rejects_a_signal_only_ma_requirement() {
    let args = Args::try_parse_from(["tv-arm", "--market-entry", "--ma-touch-required"])
        .unwrap()
        .apply_aliases();
    assert!(
        args.validate()
            .unwrap_err()
            .to_string()
            .contains("candle-pattern")
    );
}
