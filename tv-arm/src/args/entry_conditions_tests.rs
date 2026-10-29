use super::Args;
use clap::Parser;

#[test]
fn offside_is_independent_of_touch_and_rejects_direct_entry() {
    for flags in [
        vec![],
        vec!["--trend"],
        vec!["--enable-on-ma-touch", "200 sma"],
    ] {
        let args = Args::try_parse_from(
            ["tv-arm", "--be-offside", "9 ema|20 sma|200 sma"]
                .into_iter()
                .chain(flags),
        )
        .unwrap()
        .apply_aliases();
        assert_eq!(args.be_offside.len(), 3);
        assert!(args.must_touch.is_empty());
        assert!(args.validate().is_ok());
        assert!(!args.ma_requirements().is_empty());
    }
    for value in ["", "9 ema|", "0 ema", "9 ema|20 wma"] {
        assert!(Args::try_parse_from(["tv-arm", "--be-offside", value]).is_err());
    }
    let direct = Args::try_parse_from(["tv-arm", "--market-entry", "--be-offside", "9 ema"])
        .unwrap()
        .apply_aliases();
    assert!(direct.validate().is_err());
}

#[test]
fn alignment_is_independent_of_veto_and_rejects_direct_entry() {
    for flags in [vec![], vec!["--trend"], vec!["--ma-cross-veto"]] {
        let args = Args::try_parse_from(["tv-arm", "--ma-cross-pause"].into_iter().chain(flags))
            .unwrap()
            .apply_aliases();
        assert!(args.ma_cross_pause);
        assert!(args.validate().is_ok());
    }
    let direct = Args::try_parse_from(["tv-arm", "--market-entry", "--ma-cross-pause"])
        .unwrap()
        .apply_aliases();
    assert!(direct.validate().is_err());
}

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

#[test]
fn enabling_ma_is_independent_of_the_signal_touch_filter() {
    let args = Args::try_parse_from([
        "tv-arm",
        "--enable-on-ma-touch",
        "200 sma",
        "--ma-touch-required",
    ])
    .unwrap()
    .apply_aliases();
    assert_eq!(args.enable_on_ma_touch[0].to_string(), "200 sma");
    assert_eq!(args.must_touch.len(), 3);
    assert_eq!(args.must_touch[2].to_string(), "200 sma");
    assert!(!args.skip_bcr);
    assert!(!args.trend);
    assert!(args.validate().is_ok());
    assert!(Args::try_parse_from(["tv-arm", "--enable-on-ma-touch", "200 ma"]).is_err());
    let direct = Args::try_parse_from([
        "tv-arm",
        "--market-entry",
        "--enable-on-ma-touch",
        "200 sma",
    ])
    .unwrap()
    .apply_aliases();
    assert!(direct.validate().is_err());
}

#[test]
fn replaced_alignment_option_is_not_an_alias() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    assert!(Args::try_parse_from(["tv-arm", "--require-ma-alignment"]).is_err());
}
