use super::*;
fn init_tracing() {
    use tracing_subscriber::prelude::*;
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    });
}

fn event(kind: EventKind, id: &str, seconds: i64) -> Event {
    init_tracing();
    Event {
        kind,
        id: id.into(),
        instrument: "AUD/USD".into(),
        direction: Direction::Long,
        currency: "USD".into(),
        stake: 0.84,
        at: DateTime::from_timestamp(1_791_288_102 + seconds, 0).unwrap(),
        price: Some(0.69850),
    }
}
fn evidence() -> Evidence {
    Evidence {
        history_complete: true,
        events: vec![
            event(EventKind::Open, "27390230", 0),
            event(EventKind::Execute, "27024719", 1),
        ],
        positions: vec![Position {
            id: "27390230".into(),
            order_id: "27024729".into(),
            entry_price: Some(0.69850),
            opened_at: Some(event(EventKind::Open, "", 0).at),
        }],
        ..Evidence::default()
    }
}
#[test]
fn one_second_gap_recovers_open_position_despite_bracket_id() {
    let out = resolve("27024719", None, &evidence());
    assert_eq!(
        out.state,
        AttemptState::OpenPosition {
            broker_trade_id: "27390230".into()
        }
    );
    assert_eq!(out.diagnostic.unwrap().reason, "activity-unique-match");
}
#[test]
fn competing_positions_are_unknown_regardless_of_order() {
    let mut e = evidence();
    e.events.push(event(EventKind::Open, "other", 0));
    assert_eq!(resolve("27024719", None, &e).state, AttemptState::Unknown);
    e.events.reverse();
    assert_eq!(resolve("27024719", None, &e).state, AttemptState::Unknown);
}
#[test]
fn competing_executions_cannot_claim_the_same_position() {
    let mut e = evidence();
    e.events.push(event(EventKind::Execute, "other", 1));
    assert_eq!(resolve("27024719", None, &e).state, AttemptState::Unknown);
}
#[test]
fn incomplete_history_cannot_authorize_a_fallback() {
    let mut e = evidence();
    e.history_complete = false;
    assert_eq!(resolve("27024719", None, &e).state, AttemptState::Unknown);
}
#[test]
fn absence_with_known_position_is_not_cancellation() {
    assert_eq!(
        resolve("27024719", Some("27390230"), &Evidence::default()).state,
        AttemptState::Unknown
    );
}
#[test]
fn wrong_currency_direction_stake_price_or_time_does_not_match() {
    for field in 0..5 {
        let mut e = evidence();
        let open = &mut e.events[0];
        match field {
            0 => open.currency = "AUD".into(),
            1 => open.direction = Direction::Short,
            2 => open.stake = 0.85,
            3 => open.price = Some(0.8),
            _ => open.at += chrono::Duration::seconds(5),
        }
        assert_eq!(
            resolve("27024719", None, &e).state,
            AttemptState::Unknown,
            "field {field}"
        );
    }
}
#[test]
fn duplicate_history_rows_do_not_create_false_ambiguity() {
    let mut e = evidence();
    e.events.extend(e.events.clone());
    assert!(matches!(
        resolve("27024719", None, &e).state,
        AttemptState::OpenPosition { .. }
    ));
}
#[test]
fn exact_current_position_does_not_need_history() {
    let mut e = evidence();
    e.events.clear();
    e.history_complete = false;
    assert!(matches!(
        resolve("27024719", Some("27390230"), &e).state,
        AttemptState::OpenPosition { .. }
    ));
}

#[test]
fn filled_and_closed_between_observations_recovers_id_without_inventing_pnl() {
    let mut e = evidence();
    e.positions.clear();
    let mut close = event(EventKind::Close, "27390230", 60);
    close.price = Some(0.69669);
    e.events.push(close);
    let AttemptState::ClosedConfirmed { exit } = resolve("27024719", None, &e).state else {
        panic!("closure must resolve")
    };
    assert_eq!(exit.broker_trade_id, "27390230");
    assert_eq!(exit.realized_pl, None);
    assert_eq!(exit.exit_price, Some(0.69669));
}
#[test]
fn missing_position_without_close_stays_unknown() {
    let mut e = evidence();
    e.positions.clear();
    assert_eq!(resolve("27024719", None, &e).state, AttemptState::Unknown);
}
#[test]
fn cancellation_needs_evidence_and_must_not_override_an_execution() {
    let mut e = Evidence {
        history_complete: true,
        events: vec![event(EventKind::Cancel, "27024719", 0)],
        ..Evidence::default()
    };
    assert_eq!(resolve("27024719", None, &e).state, AttemptState::Cancelled);
    e.events.push(event(EventKind::Execute, "27024719", 1));
    assert_eq!(resolve("27024719", None, &e).state, AttemptState::Unknown);
}

#[test]
fn missing_price_on_a_competing_event_cannot_make_a_match_unique() {
    for kind in [EventKind::Execute, EventKind::Open] {
        let mut e = evidence();
        let mut other = event(kind, "other", 1);
        other.price = None;
        e.events.push(other);
        assert_eq!(resolve("27024719", None, &e).state, AttemptState::Unknown);
    }
}
#[test]
fn missing_price_on_the_only_candidate_is_unknown() {
    let mut e = evidence();
    e.events[0].price = None;
    assert_eq!(resolve("27024719", None, &e).state, AttemptState::Unknown);
}
