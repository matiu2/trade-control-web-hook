use super::*;
use crate::intent::Direction;

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

fn snapshot() -> PositionSnapshot {
    init_tracing();
    PositionSnapshot {
        position: OpenPosition {
            instrument: "USDJPY".into(),
            direction: Direction::Long,
            stop_loss: Some(156.705),
            take_profit: Some(158.076),
            position_id: "17074690".into(),
            order_id: "17074689".into(),
            stake: 0.65,
            entry_price: Some(156.952),
            opened_at: None,
        },
        unrealized_pl: Some(300.0),
        currency: Some("USD".into()),
    }
}

fn link() -> PositionLink {
    PositionLink {
        account: Some("the5ers-competition".into()),
        trade_id: "ihs-usdjpy-3e9eb418".into(),
        instrument: "USDJPY".into(),
        direction: Direction::Long,
        broker_order_id: "17074689".into(),
        broker_position_id: None,
    }
}

#[test]
fn order_link_finds_fill_without_a_snapshotted_position_id() {
    let matches = match_positions(&[link()], &[snapshot()]);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].link.trade_id, "ihs-usdjpy-3e9eb418");
    assert_eq!(matches[0].snapshot.unrealized_pl, Some(300.0));
}

#[test]
fn symbol_alone_never_adopts_a_manual_trade() {
    let mut manual = snapshot();
    manual.position.order_id = "manual".into();
    assert!(match_positions(&[link()], &[manual]).is_empty());
}

#[test]
fn ambiguous_owners_and_conflicting_identity_are_not_assigned() {
    let mut other = link();
    other.trade_id = "other".into();
    assert!(match_positions(&[link(), other], &[snapshot()]).is_empty());
    let mut wrong = link();
    wrong.broker_position_id = Some("different".into());
    assert!(match_positions(&[wrong], &[snapshot()]).is_empty());
    let mut wrong = link();
    wrong.direction = Direction::Short;
    assert!(match_positions(&[wrong], &[snapshot()]).is_empty());
}

#[test]
fn duplicate_link_and_position_reobservations_keep_one_position() {
    let matches = match_positions(&[link(), link()], &[snapshot()]);
    assert_eq!(matches.len(), 1);
    let mut proven = link();
    proven.broker_position_id = Some("17074690".into());
    let mut position = snapshot();
    position.position.order_id = "".into();
    assert_eq!(match_positions(&[proven], &[position]).len(), 1);
}

use crate::broker::*;
use chrono::{DateTime, Utc};

struct ReadOnlyBroker {
    state: Result<AttemptState, LookupError>,
    snapshots: Vec<PositionSnapshot>,
}
impl Broker for ReadOnlyBroker {
    async fn place_entry(
        &self,
        _: f64,
        _: u32,
        _: &EntryRequest<'_>,
    ) -> Result<Placement, EntryError> {
        panic!("observer must not place")
    }
    async fn close_positions(&self, _: &str) -> CloseOutcome {
        panic!("observer must not close")
    }
    async fn cancel_pending_for_instrument(&self, _: &str) -> usize {
        panic!("observer must not cancel")
    }
    async fn cancel_order(&self, _: &str, _: &str) -> Result<(), CancelError> {
        panic!("observer must not cancel")
    }
    async fn amend_stop(&self, _: &str, _: &str, _: f64) -> Result<(), AmendError> {
        panic!("observer must not amend")
    }
    async fn get_quote(&self, _: &str) -> Result<Quote, LookupError> {
        panic!("observer does not price positions")
    }
    async fn list_pending_orders(&self, _: &str) -> Result<Vec<PendingOrder>, LookupError> {
        panic!("not needed")
    }
    async fn get_candles(
        &self,
        _: &str,
        _: Granularity,
        _: DateTime<Utc>,
        _: DateTime<Utc>,
    ) -> Result<Vec<Candle>, CandleError> {
        panic!("observer must work without candles")
    }
    async fn list_open_positions(&self, _: &str) -> Result<Vec<OpenPosition>, LookupError> {
        Ok(self.snapshots.iter().map(|s| s.position.clone()).collect())
    }
    async fn position_snapshots(&self, _: &str) -> Result<Vec<PositionSnapshot>, LookupError> {
        Ok(self.snapshots.clone())
    }
    async fn lookup_attempt_state(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
    ) -> Result<AttemptState, LookupError> {
        self.state.clone()
    }
}

#[test]
fn observer_retains_partial_failure_and_snapshot_time_without_touching_trading() {
    init_tracing();
    let broker = ReadOnlyBroker {
        state: Err(LookupError::Transient),
        snapshots: vec![],
    };
    let at = Utc::now();
    let result = pollster::block_on(observe(&broker, "demo", &mut [link()], at)).unwrap();
    assert_eq!(result.observed_at, at);
    assert_eq!(result.warnings.len(), 1);
    assert!(result.positions.is_empty());
    assert!(result.exits.is_empty());
}

#[test]
fn observer_falls_back_to_positive_closure_and_keeps_unknown_details_unknown() {
    init_tracing();
    let mut proven = link();
    proven.broker_position_id = Some("17074690".into());
    let broker = ReadOnlyBroker {
        state: Ok(AttemptState::ClosedWin { realized_pl: 300.0 }),
        snapshots: vec![],
    };
    let result = pollster::block_on(observe(&broker, "demo", &mut [proven], Utc::now())).unwrap();
    assert_eq!(result.exits.len(), 1);
    assert_eq!(result.exits[0].1.realized_pl, Some(300.0));
    assert!(result.exits[0].1.closed_at.is_none());
    assert!(result.exits[0].1.account_currency.is_none());
    assert_eq!(
        result.exits[0].1.reason,
        crate::broker_exit::ExitReason::Unknown
    );
}

#[test]
fn observer_captures_a_fill_that_closed_before_its_first_snapshot() {
    init_tracing();
    let exit = super::observation::unknown_exit("17074690", 300.0);
    let broker = ReadOnlyBroker {
        state: Ok(AttemptState::ClosedConfirmed {
            exit: Box::new(exit),
        }),
        snapshots: vec![],
    };
    let result = pollster::block_on(observe(&broker, "demo", &mut [link()], Utc::now())).unwrap();
    assert_eq!(result.exits.len(), 1);
    assert_eq!(
        result.exits[0].0.broker_position_id.as_deref(),
        Some("17074690")
    );
}

#[test]
fn observer_exposes_conflicting_ownership_in_refresh_evidence() {
    init_tracing();
    let broker = ReadOnlyBroker {
        state: Ok(AttemptState::Unknown),
        snapshots: vec![snapshot()],
    };
    let mut other = link();
    other.trade_id = "other-trade".into();
    let result =
        pollster::block_on(observe(&broker, "demo", &mut [link(), other], Utc::now())).unwrap();
    assert!(result.positions.is_empty());
    assert_eq!(result.warnings.len(), 1);
}

#[test]
fn positive_unfilled_cancellation_resolves_the_order_without_inventing_a_position() {
    init_tracing();
    let broker = ReadOnlyBroker {
        state: Ok(AttemptState::Cancelled),
        snapshots: vec![],
    };
    let result = pollster::block_on(observe(&broker, "demo", &mut [link()], Utc::now())).unwrap();
    assert_eq!(result.cancelled.len(), 1);
    assert!(result.positions.is_empty());
    assert!(result.exits.is_empty());
    let mut filled = link();
    filled.broker_position_id = Some("17074690".into());
    let result = pollster::block_on(observe(&broker, "demo", &mut [filled], Utc::now())).unwrap();
    assert!(result.cancelled.is_empty());
    assert_eq!(result.warnings.len(), 1);
}

#[test]
fn placement_links_are_idempotent_and_exclude_dry_runs_and_unidentified_trades() {
    init_tracing();
    use crate::state::{MemStateStore, StateStore};
    let mut intent:crate::intent::Intent=serde_json::from_value(serde_json::json!({"v":1,"id":"single-shot","action":"enter","instrument":"USDJPY","direction":"long","trade_id":"tracked","not_after":"2099-01-01T00:00:00Z","dry_run":true})).unwrap();
    let store = MemStateStore::new();
    pollster::block_on(async {
        record_placement(&store, &intent, "dry-order").await;
        assert!(store.list_position_links().await.unwrap().is_empty());
        intent.dry_run = Some(false);
        record_placement(&store, &intent, "live-order").await;
        record_placement(&store, &intent, "live-order").await;
        assert_eq!(store.list_position_links().await.unwrap().len(), 1);
        intent.trade_id = None;
        record_placement(&store, &intent, "anonymous-order").await;
        assert_eq!(store.list_position_links().await.unwrap().len(), 1);
    });
}
