use super::*;

use crate::ctrader::output::test_table::table;

#[test]
fn append_preserves_every_existing_row_and_header_byte() {
    let old = table(&["AUDUSD", "EURUSD", "USDZAR"]);
    let incoming = table(&["XPDUSD"]);
    let merged = tables(&old, "demo", &incoming).unwrap();
    assert!(merged.starts_with(old.strip_suffix("];\n").unwrap()));
    let original_rows = old
        .lines()
        .filter(|line| line.trim_start().starts_with('('));
    for row in original_rows {
        assert_eq!(merged.lines().filter(|line| *line == row).count(), 1);
    }
    assert_eq!(merged.matches("ctrader:demo:XPDUSD").count(), 1);
    assert!(merged.ends_with("];\n"));
}

#[test]
fn a_partial_new_report_cannot_erase_a_completed_full_table() {
    let symbols = (0..54).map(|n| format!("FX{n}")).collect::<Vec<_>>();
    let old = table(&symbols.iter().map(String::as_str).collect::<Vec<_>>());
    let merged = tables(&old, "demo", &table(&["XPDUSD"])).unwrap();
    assert_eq!(
        merged
            .lines()
            .filter(|line| line.trim_start().starts_with('('))
            .count(),
        55
    );
    for symbol in symbols {
        assert!(merged.contains(&format!("ctrader:demo:{symbol}")));
    }
}

#[test]
fn same_key_replacement_wrong_identity_or_window_and_corrupt_tables_fail() {
    let old = table(&["EURUSD"]);
    for incoming in [
        table(&["EURUSD"]),
        table(&["XPDUSD"]).replace("\"account_id\":42", "\"account_id\":43"),
        table(&["XPDUSD"]).replace("2026-07-01", "2026-07-02"),
        table(&["XPDUSD", "XPDUSD"]),
        table(&[]),
        "garbage".into(),
    ] {
        assert!(tables(&old, "demo", &incoming).is_err());
    }
    assert!(tables(&old, "other", &table(&["XPDUSD"])).is_err());
    assert!(tables("", "demo", &table(&["XPDUSD"])).is_err());
    assert!(
        tables(
            &old.replace("ctrader:demo:EURUSD", "oanda:demo:EURUSD"),
            "demo",
            &table(&["XPDUSD"])
        )
        .is_err()
    );
}

#[test]
fn failed_promotion_keeps_destination_and_existing_report_untouched() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ctrader.rs");
    let report = directory.path().join("original.json");
    let old = table(&["EURUSD"]);
    std::fs::write(&path, &old).unwrap();
    std::fs::write(&report, "original completed report").unwrap();
    assert!(super::super::merge(&path, "demo", &table(&["EURUSD"])).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), old);
    assert_eq!(
        std::fs::read_to_string(report).unwrap(),
        "original completed report"
    );
}

#[test]
fn ordinary_bake_cannot_drop_a_previously_calibrated_palladium_row() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ctrader.rs");
    let fx = (0..54).map(|n| format!("FX{n}")).collect::<Vec<_>>();
    let mut symbols = fx.iter().map(String::as_str).collect::<Vec<_>>();
    symbols.push("XPDUSD");
    let old = table(&symbols);
    std::fs::write(&path, &old).unwrap();
    assert!(super::super::write(&path, "demo", &table(&symbols[..54])).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), old);
}

#[test]
fn normal_and_additive_writers_share_the_same_exclusive_lock() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ctrader.rs");
    let old = table(&["EURUSD"]);
    std::fs::write(&path, &old).unwrap();
    let lock = super::super::lock(&path).unwrap();
    assert!(super::super::write(&path, "demo", &old).is_err());
    assert!(super::super::merge(&path, "demo", &table(&["XPDUSD"])).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
    lock.unlock().unwrap();
    super::super::merge(&path, "demo", &table(&["XPDUSD"])).unwrap();
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .contains("ctrader:demo:XPDUSD")
    );
}

#[test]
fn verified_native_symbols_keep_spaces_dots_and_other_printable_punctuation() {
    crate::ctrader::test_logging();
    let old = table(&["EURUSD"]);
    for symbol in [
        "BRK.B",
        "Wall Street 30",
        "US30.cash",
        "Silver/EUR",
        "A-B_C",
    ] {
        let incoming = table(&[symbol]);
        let merged = tables(&old, "demo", &incoming).unwrap();
        assert!(merged.contains(&format!("ctrader:demo:{symbol}")));
        assert!(merged.starts_with(old.strip_suffix("];\n").unwrap()));
    }
    for symbol in ["bad\nrow", "bad\trow", "non-ascii-£"] {
        assert!(tables(&old, "demo", &table(&[symbol])).is_err());
    }
}
