//! Regression coverage for every spread-widen/restore episode in a replay report.

use super::*;

/// A position crossing two daily spread windows, with a third window in
/// the forward history after its exit. The second spike crosses the placed
/// SL but clears the transient stop (the AUD/NZD 2025-11-13 report bug).
fn multiday_spread_fire(exit_hour: i64, stopped: bool) -> Fire {
    use crate::replay_candles::replay_broker::ReplayBroker;

    let start = Utc.with_ymd_and_hms(2025, 11, 11, 14, 0, 0).unwrap();
    let forward: Vec<_> = (0..64)
        .map(|hour| {
            let mut bar = EngineCandle {
                time: start + Duration::hours(hour),
                o: 1.1542,
                h: 1.1548,
                l: 1.1535,
                c: 1.1542,
                bid_o: 1.154125,
                bid_h: 1.154725,
                bid_l: 1.153425,
                bid_c: 1.154125,
                ask_o: 1.154275,
                ask_h: 1.154875,
                ask_l: 1.153575,
                ask_c: 1.154275,
            };
            if hour % 24 == 8 {
                bar.l = 1.15226;
                bar.bid_l = 1.15136;
                bar.ask_l = 1.15316;
                bar.bid_c = 1.1533;
                bar.ask_c = 1.1551;
            }
            if hour == exit_hour {
                if stopped {
                    bar.l = 1.1514;
                    bar.bid_l = 1.1505;
                    bar.ask_l = 1.1523;
                } else {
                    bar.h = 1.1622;
                    bar.bid_h = 1.162125;
                    bar.ask_h = 1.162275;
                }
            }
            bar
        })
        .collect();
    let intent: Intent = serde_yaml::from_str(
        "v: 1
id: multiday-spread
trade_id: multiday-spread
not_after: '2025-11-17T03:00:00Z'
action: enter
account: tradenation-live
instrument: AUD/NZD
direction: long
entry: { type: stop, from: high, at: 1.15383 }
stop_loss: { absolute: 1.15233 }
take_profit: { absolute: 1.16193 }
risk_pct: 1.0
pip_size: 0.0001
tick_size: 0.00001",
    )
    .expect("spread-window intent");
    let fired = trade_control_engine::FiredIntent {
        rule_id: "05-enter".into(),
        intent,
        candle: forward[0].mid(),
        signal: None,
    };
    let broker = ReplayBroker::new(forward.clone(), 0.0001);
    broker.record_attempt(
        "spread-order".into(),
        fired.intent.clone(),
        Shell::from_candle(&fired.candle),
        None,
    );
    Fire {
        placed_bracket: broker.placed_bracket("spread-order"),
        realized: broker.held_realized_outcome("spread-order"),
        fired,
        forward,
        ma_warmup: Vec::new(),
        gate_outcome: EnterGateOutcome::Placed { order_id: None },
        superseded: false,
    }
}

fn spread_stop_events(fire: &Fire) -> Vec<EntryEvent> {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    render_fire(
        &plan_for(0.0001),
        fire,
        Some(2),
        true,
        &mut ReplayEconomics::new(),
    )
    .into_iter()
    .filter(|event| {
        event.note.contains("SL widened")
            || event.note.contains("SL restored")
            || event.note.contains("SL still widened")
    })
    .collect()
}

#[test]
fn report_shows_every_spread_widen_and_restore_before_exit() {
    let fire = multiday_spread_fire(40, false);
    assert_eq!(
        fire.realized.as_ref().expect("realized TP").kind,
        FillKind::TookProfit
    );
    let events = spread_stop_events(&fire);
    let actual: Vec<_> = events
        .iter()
        .map(|event| (event.at, event.note.as_str()))
        .collect();
    let start = fire.fired.candle.time;
    let times: Vec<_> = events.iter().map(|event| event.at).collect();
    assert_eq!(
        times,
        [
            start + Duration::minutes(450),
            start + Duration::hours(9),
            start + Duration::minutes(1890),
            start + Duration::hours(33),
        ],
        "all episodes, excluding the third spread window after TP: {actual:?}"
    );
    assert!(
        events
            .iter()
            .step_by(2)
            .all(|event| event.note.contains("entry #2 SL widened → 1.15060"))
    );
    assert!(
        events
            .iter()
            .skip(1)
            .step_by(2)
            .all(|event| event.note.contains("entry #2 SL restored → 1.15233"))
    );
}

#[test]
fn report_keeps_a_later_widen_without_inventing_a_restore_after_exit() {
    let fire = multiday_spread_fire(32, true);
    assert_eq!(
        fire.realized.as_ref().expect("realized SL").kind,
        FillKind::StoppedOut
    );
    let events = spread_stop_events(&fire);
    let notes: Vec<_> = events.iter().map(|event| event.note.as_str()).collect();
    assert_eq!(events.len(), 4, "{notes:?}");
    assert!(events[2].note.contains("SL widened → 1.15060"));
    assert!(events[3].note.contains("SL still widened at exit"));
    assert_eq!(events[3].at, fire.fired.candle.time + Duration::hours(32));
    assert_eq!(
        events
            .iter()
            .filter(|event| event.note.contains("SL restored"))
            .count(),
        1
    );
}
