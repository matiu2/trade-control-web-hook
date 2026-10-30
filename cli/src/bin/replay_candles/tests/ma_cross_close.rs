use super::*;
use crate::replay_candles::report::FillKind;
use tracing_subscriber::prelude::*;
use trade_control_core::intent::{EntrySpec, PriceRef, TakeProfit};
use trade_control_core::trade_plan::{ConditionRule, FireMode};

fn setup(direction: Direction, pending: bool) -> (TradePlan, Vec<EngineCandle>) {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let start = Utc.with_ymd_and_hms(2026, 5, 29, 0, 0, 0).unwrap();
    let price = |long: f64| match direction {
        Direction::Long => long,
        Direction::Short => 2.21 - long,
    };
    let candles: Vec<_> = (0..135)
        .map(|i| {
            let close = if i < 110 {
                1.1 + i as f64 * 0.00004
            } else {
                1.1044 - (i - 110) as f64 * 0.0003
            };
            let extreme = if pending && i == 130 {
                1.151
            } else {
                close + 0.0001
            };
            let (high, low) = match direction {
                Direction::Long => (price(extreme), price(close - 0.0001)),
                Direction::Short => (price(close - 0.0001), price(extreme)),
            };
            ohlc_at_spread(
                &(start + Duration::hours(i)).to_rfc3339(),
                price(close),
                high,
                low,
                price(close),
                0.00002,
            )
        })
        .collect();
    let mut plan = plain_enter_plan("CAD_CHF", 1.1);
    plan.direction = direction;
    plan.armed_at = Some(candles[100].time);
    let entry = &mut plan.rules[0];
    entry.trigger = Trigger::MwEveryBar;
    entry.intent.direction = Some(direction);
    entry.intent.entry = Some(if pending {
        EntrySpec::Stop {
            from: trade_control_core::intent::PriceAnchor::Close,
            offset_pips: 0.0,
            at: Some(price(1.15)),
            offset_atr_pct: None,
            recover_entry: None,
        }
    } else {
        EntrySpec::Market {}
    });
    entry.intent.stop_loss = Some(PriceRef::Absolute {
        absolute: price(1.0),
    });
    entry.intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
        absolute: price(1.3),
    }));
    let mut intent = crate::build_close_intent(
        &plan.instrument,
        &plan.trade_id,
        candles[100].time,
        "ma-cross",
    );
    intent.not_after = entry.intent.not_after;
    intent.broker = entry.intent.broker;
    plan.rules.push(ConditionRule {
        rule_id: "08-close-on-ma-cross".into(),
        trigger: Trigger::AdverseMaCross { dir: direction },
        fire_mode: FireMode::EveryBar,
        kind: trade_control_conventions::RuleKind::PerPositionClose,
        intent,
    });
    (plan, candles)
}

#[tokio::test]
async fn adverse_ma_cross_closes_losing_positions_without_veto_or_pause() {
    for direction in [Direction::Long, Direction::Short] {
        let (plan, candles) = setup(direction, false);
        let mids: Vec<_> = candles.iter().map(EngineCandle::mid).collect();
        let cross = (100..candles.len())
            .find(|&i| trade_control_core::ma_cross::adverse_cross_at(&mids, i, direction))
            .unwrap();
        let replay = run(
            &plan,
            &candles,
            Granularity::H1,
            candles[100].time,
            candles.last().unwrap().time + Duration::days(1),
            no_marks(),
            None,
        )
        .await;
        let entry = replay
            .fires
            .iter()
            .find(|f| f.fired.intent.action == Action::Enter)
            .unwrap();
        let realized = entry.realized.as_ref().expect("position filled");
        assert_eq!(realized.kind, FillKind::ClosedOnMaCross);
        assert_eq!(realized.until, candles[cross].time);
        let exit = realized.exit_price.unwrap();
        assert!(
            match direction {
                Direction::Long => exit < realized.entry_price,
                Direction::Short => exit > realized.entry_price,
            },
            "losing positions must close too: {realized:?}"
        );
        assert!(!replay.final_state.entries_blocked);
        assert!(!replay.done);
        assert!(
            replay
                .fires
                .iter()
                .all(|f| matches!(f.fired.intent.action, Action::Enter | Action::Close))
        );
    }
}

#[tokio::test]
async fn adverse_ma_close_leaves_pending_orders_free_to_fill_later() {
    for direction in [Direction::Long, Direction::Short] {
        let (plan, candles) = setup(direction, true);
        let replay = run(
            &plan,
            &candles,
            Granularity::H1,
            candles[100].time,
            candles.last().unwrap().time + Duration::days(1),
            no_marks(),
            None,
        )
        .await;
        assert!(
            replay
                .fires
                .iter()
                .any(|f| f.fired.intent.action == Action::Close)
        );
        let entry = replay
            .fires
            .iter()
            .find(|f| f.fired.intent.action == Action::Enter)
            .unwrap();
        let realized = entry.realized.as_ref().expect("pending order still fills");
        assert_eq!(realized.fill_at, candles[130].time);
        assert_eq!(realized.kind, FillKind::Open);
        assert!(!replay.final_state.entries_blocked);
    }
}
