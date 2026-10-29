use super::*;
use crate::intent::{Intent, Shell, VetoSide};

fn bracket(entry: f64, stop_loss: f64, take_profit: f64) -> Resolved {
    crate::test_logging::init_tracing();
    let direction = if stop_loss > entry { "short" } else { "long" };
    let intent: Intent = serde_json::from_value(serde_json::json!({
        "v": 1, "id": "spread-veto-test", "action": "enter",
        "instrument": "CHF/JPY", "direction": direction, "broker": "tradenation",
        "not_after": "2026-10-30T00:00:00Z",
        "entry": {"type": "stop", "at": entry, "from": "close", "offset_pips": 0.0},
        "stop_loss": {"absolute": stop_loss},
        "take_profit": {"absolute": take_profit}
    }))
    .expect("valid test intent");
    let shell: Shell = serde_json::from_value(serde_json::json!({
        "time": "2026-10-29T06:30:00Z", "close": entry,
        "high": entry + 0.1, "low": entry - 0.1
    }))
    .expect("valid test shell");
    Resolved::from_intent(&intent, &shell, 0.01, 0.001).expect("valid bracket")
}

fn veto(name: &str, level: f64, past: VetoSide) -> EntryLevelVeto {
    EntryLevelVeto {
        name: name.into(),
        level,
        past,
    }
}

#[test]
fn chfjpy_widen_above_too_high_is_rejected_even_with_enough_reward() {
    // Incident entry/TP/spread; a representative tight signal stop.
    let resolved = bracket(190.407, 190.550, 190.022);
    let boundary = veto("too-high", 190.673, VetoSide::Above);
    let rejected =
        widen_entry_sl_to_spread_floor(&resolved, 0.038, 0.001, std::slice::from_ref(&boundary))
            .expect_err("38-pip stop must not pass too-high");
    assert_eq!(rejected.veto, boundary);
    assert!((rejected.widened_stop_loss - 190.787).abs() < 1e-9);
}

#[test]
fn long_widen_below_too_low_is_rejected() {
    let resolved = bracket(190.407, 190.300, 190.900);
    let boundary = veto("too-low", 190.100, VetoSide::Below);
    let rejected =
        widen_entry_sl_to_spread_floor(&resolved, 0.038, 0.001, std::slice::from_ref(&boundary))
            .expect_err("long stop must not pass too-low");
    assert_eq!(rejected.veto, boundary);
    assert!((rejected.widened_stop_loss - 190.027).abs() < 1e-9);
}

#[test]
fn touching_a_boundary_is_rejected_for_both_directions() {
    [
        (
            bracket(100.0, 100.1, 99.0),
            veto("too-high", 100.5, VetoSide::Above),
        ),
        (
            bracket(100.0, 99.9, 101.0),
            veto("too-low", 99.5, VetoSide::Below),
        ),
    ]
    .into_iter()
    .for_each(|(resolved, boundary)| {
        assert!(widen_entry_sl_to_spread_floor(&resolved, 0.05, 0.001, &[boundary]).is_err());
    });
}

#[test]
fn tick_rounding_cannot_push_a_widened_stop_onto_the_boundary() {
    let resolved = bracket(100.0, 100.1, 99.0);
    let boundary = veto("too-high", 100.35, VetoSide::Above);
    let rejected = widen_entry_sl_to_spread_floor(&resolved, 0.0343, 0.01, &[boundary])
        .expect_err("100.343 rounds up to the forbidden 100.35");
    assert!((rejected.widened_stop_loss - 100.35).abs() < 1e-9);
}

#[test]
fn inside_boundaries_and_absent_boundaries_still_allow_widening() {
    let resolved = bracket(100.0, 100.1, 99.0);
    let boundaries = [
        veto("too-low", 99.5, VetoSide::Below),
        veto("too-high", 100.501, VetoSide::Above),
    ];
    [&boundaries[..], &[]].into_iter().for_each(|boundaries| {
        assert!(matches!(
            widen_entry_sl_to_spread_floor(&resolved, 0.05, 0.001, boundaries),
            Ok(SlWiden::Widened { .. })
        ));
    });
}

#[test]
fn unchanged_stops_are_outside_the_scope_of_the_widening_veto() {
    let resolved = bracket(100.0, 100.6, 99.0);
    let boundary = veto("too-high", 100.5, VetoSide::Above);
    assert_eq!(
        widen_entry_sl_to_spread_floor(&resolved, 0.05, 0.001, &[boundary]),
        Ok(SlWiden::Unchanged)
    );
}

#[test]
fn boundary_rejection_takes_precedence_over_recoverable_min_r_parking() {
    let resolved = bracket(100.0, 100.1, 99.8);
    let boundary = veto("too-high", 100.4, VetoSide::Above);
    assert!(widen_entry_sl_to_spread_floor(&resolved, 0.05, 0.001, &[boundary]).is_err());
    assert!(matches!(
        widen_entry_sl_to_spread_floor(&resolved, 0.05, 0.001, &[]),
        Ok(SlWiden::Reject { .. })
    ));
}
