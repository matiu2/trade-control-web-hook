use super::{entry, history, mapping, sizing};
use chrono::Utc;
use mt5_data_source::web::trading::{
    Deal, History, Lots, OutcomeState, Rules, TradeOutcome, TradeResult, Trades,
};
use trade_control_core::{
    broker::{AttemptState, EntryError},
    intent::{Direction, RiskBudget},
};
fn logging() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}
fn rules() -> Rules {
    Rules {
        path: String::new(),
        spread_points: 0,
        spread_balance: 0,
        trade_mode: 4,
        stops_points: 0,
        execution: 2,
        filling_flags: 2,
        expiration_flags: 1,
        order_flags: 127,
        request_timeout_seconds: 7,
        min_volume_raw: 1_000_000,
        max_volume_raw: 10_000_000_000,
        volume_step_raw: 1_000_000,
    }
}
#[test]
fn risk_is_account_currency_lots_floor_down_and_never_inflate_minimum() {
    logging();
    // 20 pips on a 100k EURCAD lot = CAD 200, at CADUSD .73 = USD 146.
    let r = rules();
    let lots = sizing::lots(RiskBudget::Amount(10.0), 10_000.0, 146.0, 1.0, &r).unwrap();
    assert_eq!(lots.raw(), 6_000_000);
    assert!(lots.as_f64() * 146.0 <= 10.0);
    assert!(matches!(
        sizing::lots(RiskBudget::Amount(1.0), 10_000.0, 146.0, 1.0, &r),
        Err(EntryError::UnitsBelowMinimum)
    ));
    assert_eq!(
        sizing::lots(RiskBudget::Units(0.01), 10_000.0, 146.0, 1.0, &r)
            .unwrap()
            .raw(),
        1_000_000
    );
    assert!(matches!(
        sizing::lots(RiskBudget::Units(1.0), 10_000.0, 146.0, 1.0, &r),
        Err(EntryError::RiskCapExceeded { .. })
    ));
}
#[test]
fn invalid_equity_and_budget_cannot_size() {
    logging();
    assert!(sizing::lots(RiskBudget::Amount(f64::NAN), 10_000.0, 146.0, 1.0, &rules()).is_err());
    assert!(
        sizing::lots(
            RiskBudget::Percent(1.0),
            f64::INFINITY,
            146.0,
            1.0,
            &rules()
        )
        .is_err()
    );
    assert!(sizing::lots(RiskBudget::Amount(10.0), 10_000.0, 0.0, 1.0, &rules()).is_err());
}
#[test]
fn short_stop_geometry_is_on_the_correct_sides() {
    logging();
    assert!(entry::validate_stops(Direction::Short, 1.59, 1.592, 1.586).is_ok());
    assert!(entry::validate_stops(Direction::Short, 1.59, 1.588, 1.586).is_err());
    assert!(entry::validate_stops(Direction::Long, 1.59, 1.588, 1.594).is_ok());
}
#[test]
fn unknown_submission_is_ambiguous_and_partial_fill_reports_actual_lots() {
    logging();
    let unknown = TradeOutcome {
        request_id: 42,
        state: OutcomeState::Unknown,
        submission_code: None,
        result: None,
        uncertainty: Some("timeout after send".into()),
    };
    assert!(matches!(
        mapping::placement(unknown, Lots::from_raw(2_000_000).unwrap(), 1.59),
        Err(EntryError::AmbiguousSuccess(_))
    ));
    let partial = TradeOutcome {
        request_id: 43,
        state: OutcomeState::PartiallyFilled,
        submission_code: Some(0),
        uncertainty: None,
        result: Some(TradeResult {
            code: 10010,
            deal: 12,
            order: 11,
            volume_raw: 1_000_000,
            price: 1.5899,
            bid: 1.5899,
            ask: 1.59,
            comment: String::new(),
        }),
    };
    let p = mapping::placement(partial, Lots::from_raw(2_000_000).unwrap(), 1.59).unwrap();
    assert_eq!(p.size, Some(0.01));
    assert_eq!(p.price, Some(1.59)); // requested rate, never substituted with the fill
}
fn deal(ticket: u64, order: u64, entry: u32, volume_raw: u64, profit: f64) -> Deal {
    Deal {
        ticket,
        order,
        position: 200,
        symbol: "EURCAD".into(),
        deal_type: if entry == 0 { 1 } else { 0 },
        entry,
        volume_raw,
        created_at: Utc::now(),
        price: 1.59,
        stop_loss: 1.592,
        take_profit: 1.586,
        profit,
        commission: -0.02,
        fee: 0.0,
        swap: 0.0,
        comment: String::new(),
    }
}
#[test]
fn closed_before_first_poll_is_correlated_by_deal_order_and_includes_both_fees() {
    logging();
    let h = History {
        deals: vec![
            deal(1, 100, 0, 1_000_000, 0.0),
            deal(2, 101, 1, 1_000_000, 0.03),
        ],
        orders: vec![],
    };
    let live = Trades {
        positions: vec![],
        orders: vec![],
    };
    let state = history::attempt("EURCAD", "100", None, &live, &h);
    assert!(
        matches!(state,AttemptState::ClosedLossOrBreakeven{realized_pl} if (realized_pl+0.01).abs()<1e-12)
    );
    let s = history::settlement("EURCAD", &["100".into()], &live, &h, "USD", Utc::now());
    assert_eq!(s.trades[0].broker_trade_id, "200");
    assert_eq!(s.ledger.len(), 2);
    assert!((s.total_realized_pl().unwrap() + 0.01).abs() < 1e-12);
}
#[test]
fn partial_close_missing_history_and_foreign_symbol_remain_unknown() {
    logging();
    let h = History {
        deals: vec![
            deal(1, 100, 0, 2_000_000, 0.0),
            deal(2, 101, 1, 1_000_000, 0.03),
        ],
        orders: vec![],
    };
    let live = Trades {
        positions: vec![],
        orders: vec![],
    };
    assert_eq!(
        history::attempt("EURCAD", "100", None, &live, &h),
        AttemptState::Unknown
    );
    assert_eq!(
        history::attempt("EURUSD", "100", None, &live, &h),
        AttemptState::Unknown
    );
    assert_eq!(
        history::attempt("EURCAD", "999", None, &live, &h),
        AttemptState::Unknown
    );
}

#[test]
fn amendments_resolve_originating_order_to_live_position_ticket() {
    logging();
    use mt5_data_source::web::trading::{Direction as Side, Position};
    let p = Position {
        ticket: 200,
        symbol: "EURCAD".into(),
        side: Side::Sell,
        lots: Lots::from_raw(1_000_000).unwrap(),
        opened_at: Utc::now(),
        updated_at: Utc::now(),
        open_price: 1.59,
        current_price: 1.59,
        stop_loss: 1.592,
        take_profit: 1.586,
        profit: 0.0,
        commission: 0.0,
        swap: 0.0,
        comment: String::new(),
    };
    let live = Trades {
        positions: vec![p],
        orders: vec![],
    };
    let h = History {
        deals: vec![deal(1, 100, 0, 1_000_000, 0.0)],
        orders: vec![],
    };
    assert_eq!(mapping::amend_position_ticket(100, &live, &h), Some(200));
    assert_eq!(mapping::amend_position_ticket(200, &live, &h), Some(200));
    assert_eq!(mapping::amend_position_ticket(999, &live, &h), None);
}
