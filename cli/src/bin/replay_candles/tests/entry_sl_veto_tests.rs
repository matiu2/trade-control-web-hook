use super::*;
use tracing_subscriber::prelude::*;
use trade_control_core::intent::{EntryLevelVeto, VetoSide};

fn enter(direction: &str, stop: f64, tp: f64, boundary: f64) -> (Intent, Shell) {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let mut intent = base_enter();
    intent.direction = Some(if direction == "short" {
        Direction::Short
    } else {
        Direction::Long
    });
    intent.entry = Some(EntrySpec::Stop {
        from: PriceAnchor::Close,
        at: Some(190.407),
        offset_pips: 0.0,
        offset_atr_pct: None,
        recover_entry: None,
    });
    intent.stop_loss = Some(PriceRef::Absolute { absolute: stop });
    intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute { absolute: tp }));
    intent.tick_size = Some(0.001);
    intent.entry_level_vetos = vec![EntryLevelVeto {
        name: if direction == "short" {
            "too-high"
        } else {
            "too-low"
        }
        .into(),
        level: boundary,
        past: if direction == "short" {
            VetoSide::Above
        } else {
            VetoSide::Below
        },
    }];
    let shell =
        Shell::from_candle(&candle("2026-06-17T10:00:00Z", 190.407, 190.6, 190.2, 190.407).mid());
    (intent, shell)
}

#[test]
fn entry_sl_veto_prevents_replay_brackets_for_both_directions() {
    [
        enter("short", 190.550, 190.022, 190.673),
        enter("long", 190.300, 190.900, 190.100),
    ]
    .into_iter()
    .for_each(|(intent, shell)| {
        let path = [spread_fire_bar("2026-06-17T10:00:00Z", 190.407, 0.038)];
        assert!(matches!(
            resolve_effective_bracket(&intent, &shell, 0.01, &path, Some(0.038)),
            Err(BracketReject::FloorRejected)
        ));
        assert!(widened_stop_at(&intent, &shell, 0.01, &path, 20.0, Some(0.038)).is_none());
    });
}

#[test]
fn entry_sl_veto_allows_replay_bracket_when_the_widen_stays_inside() {
    let (intent, shell) = enter("short", 190.550, 190.022, 190.800);
    let path = [spread_fire_bar("2026-06-17T10:00:00Z", 190.407, 0.038)];
    let Ok(resolved) = resolve_effective_bracket(&intent, &shell, 0.01, &path, Some(0.038)) else {
        panic!("widening within too-high remains allowed");
    };
    assert!((resolved.stop_loss - 190.787).abs() < 1e-9);
}
