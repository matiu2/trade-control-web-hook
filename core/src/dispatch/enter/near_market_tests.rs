//! Stop → limit → market is permitted only by two specific distance rejections.
use super::*;
use crate::broker::*;
use crate::intent::{Direction, Intent, ResolvedEntry, Shell};
use crate::state::{MemStateStore, StateStore};
use chrono::{DateTime, Utc};
use std::cell::RefCell;

#[derive(Clone, Copy)]
enum Reply {
    TooClose,
    GenericTooClose,
    Rejected,
    AccountFailed,
    RiskCap,
    Ambiguous,
    Accepted,
}

struct Scenario {
    replies: [Reply; 3],
    quote: Result<Quote, LookupError>,
    market_quote: Quote,
    pending_quotes: [Quote; 2],
    requests: RefCell<Vec<ResolvedEntry>>,
    sent: RefCell<usize>,
}

impl Scenario {
    fn audjpy() -> Self {
        let quote = Quote {
            bid: 110.123,
            ask: 110.133,
        };
        Self {
            replies: [Reply::TooClose, Reply::TooClose, Reply::Accepted],
            quote: Ok(quote),
            market_quote: quote,
            pending_quotes: [quote; 2],
            requests: RefCell::new(vec![]),
            sent: RefCell::new(0),
        }
    }

    fn reply(&self, req: &EntryRequest<'_>) -> Result<Placement, EntryError> {
        let (index, kind, trigger_price) = match req.entry {
            ResolvedEntry::Stop { trigger_price } => (0, PendingEntryKind::Stop, trigger_price),
            ResolvedEntry::Limit { trigger_price } => (1, PendingEntryKind::Limit, trigger_price),
            ResolvedEntry::Market { reference_price } => {
                (2, PendingEntryKind::Stop, reference_price)
            }
        };
        match self.replies[index] {
            Reply::TooClose => Err(EntryError::EntryWithinMinimumDistance(
                PendingEntryDistance {
                    kind,
                    direction: req.direction,
                    trigger_price,
                    quote: self.pending_quotes[index.min(1)],
                    minimum_distance: 0.010,
                },
            )),
            Reply::GenericTooClose => Err(EntryError::EntryTooCloseToMarket),
            Reply::Rejected => Err(EntryError::OrderRejected),
            Reply::AccountFailed => Err(EntryError::AccountFetch),
            Reply::RiskCap => Err(EntryError::RiskCapExceeded {
                requested: 2.0,
                cap: 1.0,
            }),
            Reply::Ambiguous => Err(EntryError::AmbiguousSuccess(
                "MT5 timeout after send".into(),
            )),
            Reply::Accepted => {
                if !req.dry_run {
                    *self.sent.borrow_mut() += 1;
                }
                Ok(Placement {
                    order_id: "confirmed-order".into(),
                    size: Some(0.1),
                    price: Some(trigger_price),
                })
            }
        }
    }
}

impl Broker for Scenario {
    async fn place_entry(
        &self,
        _: f64,
        _: u32,
        req: &EntryRequest<'_>,
    ) -> Result<Placement, EntryError> {
        self.requests.borrow_mut().push(req.entry.clone());
        self.reply(req)
    }
    async fn place_market_entry_in_band(
        &self,
        cap: f64,
        positions: u32,
        req: &EntryRequest<'_>,
        band: MarketEntryBand,
    ) -> Result<Placement, EntryError> {
        assert_eq!(cap, 100.0);
        assert_eq!(positions, 100);
        assert_eq!(
            req.stop_loss,
            if req.direction == Direction::Long {
                109.720
            } else {
                110.556
            }
        );
        assert_eq!(
            req.take_profit,
            if req.direction == Direction::Long {
                110.999
            } else {
                109.277
            }
        );
        let price = if req.direction == Direction::Long {
            self.market_quote.ask
        } else {
            self.market_quote.bid
        };
        self.requests.borrow_mut().push(req.entry.clone());
        band.check(req, price)?;
        self.reply(req).map(|mut placed| {
            placed.price = Some(price);
            placed
        })
    }
    async fn get_quote(&self, _: &str) -> Result<Quote, LookupError> {
        self.quote.clone()
    }
    async fn close_positions(&self, _: &str) -> CloseOutcome {
        unreachable!()
    }
    async fn cancel_pending_for_instrument(&self, _: &str) -> usize {
        unreachable!()
    }
    async fn lookup_attempt_state(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
    ) -> Result<AttemptState, LookupError> {
        Ok(AttemptState::Cancelled)
    }
    async fn cancel_order(&self, _: &str, _: &str) -> Result<(), CancelError> {
        unreachable!()
    }
    async fn list_open_positions(&self, _: &str) -> Result<Vec<OpenPosition>, LookupError> {
        Ok(vec![])
    }
    async fn amend_stop(&self, _: &str, _: &str, _: f64) -> Result<(), AmendError> {
        unreachable!()
    }
    async fn list_pending_orders(&self, _: &str) -> Result<Vec<PendingOrder>, LookupError> {
        Ok(vec![])
    }
    async fn get_candles(
        &self,
        _: &str,
        _: Granularity,
        _: DateTime<Utc>,
        _: DateTime<Utc>,
    ) -> Result<Vec<Candle>, CandleError> {
        Ok(vec![])
    }
}

fn verified(direction: Direction) -> incoming::Verified {
    let mut intent: Intent = serde_json::from_str(
        r#"{
        "v":1,"id":"audjpy-enter","not_after":"2026-10-22T12:00:00Z",
        "action":"enter","instrument":"AUDJPY","direction":"long",
        "entry":{"type":"stop","at":110.138,"from":"close","offset_pips":0,
                 "recover_entry":{"action":"limit"}},
        "stop_loss":{"absolute":109.720},"take_profit":{"absolute":110.999},
        "broker":"mt5","account":"the5ers-competition","trade_id":"audjpy",
        "pip_size":0.01,"tick_size":0.001,"max_retries":2,"entry_dedup":"gate_owned",
        "breakeven":{"threshold":0.5}
    }"#,
    )
    .unwrap();
    intent.direction = Some(direction);
    if direction == Direction::Short {
        intent.stop_loss = Some(serde_json::from_str(r#"{"absolute":110.556}"#).unwrap());
        intent.take_profit = Some(serde_json::from_str(r#"{"absolute":109.277}"#).unwrap());
    }
    incoming::Verified {
        intent,
        shell: Shell::from_candle(&Candle {
            time: "2026-10-19T01:00:00Z".parse().unwrap(),
            o: 110.0,
            h: 110.1375,
            l: 109.914,
            c: 110.118,
        }),
    }
}

fn run(broker: &Scenario, verified: incoming::Verified) -> (ActionResult, MemStateStore) {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let now = "2026-10-19T02:00:18Z".parse().unwrap();
    let store = MemStateStore::default();
    store.set_clock(now);
    let result = pollster::block_on(run_enter(
        broker,
        &store,
        &verified,
        &DispatchConfig {
            worker_max_risk_pct: 100.0,
            worker_max_open_positions: 100,
            pip_size: 0.01,
            tick_size: Some(0.001),
            caps: Default::default(),
        },
        now,
        None,
        Some(Granularity::H1),
        EntryOrigin::Fresh,
    ));
    (result, store)
}

#[test]
fn recorded_audjpy_prices_recover_via_stop_limit_market_once() {
    let broker = Scenario::audjpy();
    let (result, store) = run(&broker, verified(Direction::Long));
    let ActionResult::Ok(message) = result else {
        panic!("{}", result.describe());
    };
    assert!(message.starts_with("entered:"), "{message}");
    assert!(
        message.contains("stop") && message.contains("limit") && message.contains("market"),
        "{message}"
    );
    assert!(matches!(
        broker.requests.borrow().as_slice(),
        [
            ResolvedEntry::Stop { .. },
            ResolvedEntry::Limit { .. },
            ResolvedEntry::Market { .. }
        ]
    ));
    assert_eq!(*broker.sent.borrow(), 1);
    let attempts =
        pollster::block_on(store.list_entry_attempts(Some("the5ers-competition"), "audjpy"))
            .unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].breakeven.as_ref().unwrap().entry_price, 110.133);
}

#[test]
fn non_distance_stop_or_limit_errors_never_authorize_market() {
    [
        Reply::Rejected,
        Reply::AccountFailed,
        Reply::RiskCap,
        Reply::Ambiguous,
        Reply::GenericTooClose,
    ]
    .into_iter()
    .for_each(|reply| {
        [0, 1].into_iter().for_each(|stage| {
            let mut broker = Scenario::audjpy();
            broker.replies[stage] = reply;
            let (result, _) = run(&broker, verified(Direction::Long));
            assert!(
                !broker
                    .requests
                    .borrow()
                    .iter()
                    .any(|e| matches!(e, ResolvedEntry::Market { .. })),
                "{}",
                result.describe()
            );
            assert_eq!(*broker.sent.borrow(), 0);
            if matches!(reply, Reply::Ambiguous) {
                assert!(
                    matches!(result, ActionResult::AmbiguousSuccess(_)),
                    "{}",
                    result.describe()
                );
            }
        });
    });
}

#[test]
fn a_far_wrong_side_rejection_does_not_authorize_the_third_attempt() {
    let mut broker = Scenario::audjpy();
    broker.quote = Ok(Quote {
        bid: 110.498,
        ask: 110.502,
    });
    broker.pending_quotes = [broker.quote.clone().unwrap(); 2];
    let (result, _) = run(&broker, verified(Direction::Long));
    assert!(matches!(result, ActionResult::Failed(_)));
    assert!(
        !broker
            .requests
            .borrow()
            .iter()
            .any(|e| matches!(e, ResolvedEntry::Market { .. }))
    );
}

#[test]
fn broker_rechecks_its_fresh_market_quote_after_account_reads() {
    let mut broker = Scenario::audjpy();
    broker.market_quote = Quote {
        bid: 110.160,
        ask: 110.164,
    };
    let (result, _) = run(&broker, verified(Direction::Long));
    assert!(matches!(result, ActionResult::Failed(_)));
    assert_eq!(*broker.sent.borrow(), 0);
    assert!(result.describe().contains("market-recovery-declined"));
}

#[test]
fn accepted_limit_ends_recovery_and_ambiguous_market_is_never_retryable() {
    let mut broker = Scenario::audjpy();
    broker.replies[1] = Reply::Accepted;
    let (result, _) = run(&broker, verified(Direction::Long));
    assert!(
        matches!(result, ActionResult::Ok(_)),
        "{}",
        result.describe()
    );
    assert_eq!(broker.requests.borrow().len(), 2);
    let mut broker = Scenario::audjpy();
    broker.replies[2] = Reply::Ambiguous;
    let (result, _) = run(&broker, verified(Direction::Long));
    assert!(
        matches!(result, ActionResult::AmbiguousSuccess(_)),
        "{}",
        result.describe()
    );
    assert!(matches!(
        crate::dispatch::seen_decision(&result),
        crate::dispatch::SeenDecision::Mark { .. }
    ));
    assert_eq!(broker.requests.borrow().len(), 3);
}

#[test]
fn short_proximity_failures_recover_at_bid_and_record_one_attempt() {
    let mut broker = Scenario::audjpy();
    let quote = Quote {
        bid: 110.143,
        ask: 110.147,
    };
    broker.quote = Ok(quote);
    broker.market_quote = quote;
    broker.pending_quotes = [quote; 2];
    let (result, store) = run(&broker, verified(Direction::Short));
    assert!(
        matches!(result, ActionResult::Ok(_)),
        "{}",
        result.describe()
    );
    assert!(
        matches!(broker.requests.borrow().as_slice(), [ResolvedEntry::Stop {..}, ResolvedEntry::Limit {..}, ResolvedEntry::Market { reference_price }] if *reference_price == 110.143)
    );
    assert_eq!(*broker.sent.borrow(), 1);
    let attempts =
        pollster::block_on(store.list_entry_attempts(Some("the5ers-competition"), "audjpy"))
            .unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].breakeven.as_ref().unwrap().entry_price, 110.143);
}

#[test]
fn stale_evidence_or_a_missing_quote_cannot_authorize_market() {
    [
        Ok(Quote {
            bid: 110.160,
            ask: 110.164,
        }),
        Ok(Quote {
            bid: f64::NAN,
            ask: 110.133,
        }),
        Err(LookupError::Transient),
    ]
    .into_iter()
    .for_each(|quote| {
        let mut broker = Scenario::audjpy();
        broker.quote = quote;
        let (result, _) = run(&broker, verified(Direction::Long));
        assert!(
            matches!(result, ActionResult::Failed(_)),
            "{}",
            result.describe()
        );
        assert_eq!(broker.requests.borrow().len(), 2);
        assert_eq!(*broker.sent.borrow(), 0);
    });
}

#[test]
fn a_limit_rejection_outside_the_distance_band_is_not_market_permission() {
    let mut broker = Scenario::audjpy();
    broker.pending_quotes[1] = Quote {
        bid: 110.0,
        ask: 110.004,
    };
    let (result, _) = run(&broker, verified(Direction::Long));
    assert!(matches!(result, ActionResult::Failed(_)));
    assert_eq!(broker.requests.borrow().len(), 2);
    assert!(
        result
            .describe()
            .contains("limit rejection does not establish proximity")
    );
}

#[test]
fn recovery_skip_and_absent_policy_never_opt_into_market() {
    [r#"{"type":"stop","at":110.138,"from":"close","offset_pips":0}"#,
     r#"{"type":"stop","at":110.138,"from":"close","offset_pips":0,"recover_entry":{"action":"skip"}}"#]
        .into_iter().for_each(|entry| {
            let broker = Scenario::audjpy();
            let mut verified = verified(Direction::Long);
            verified.intent.entry = Some(serde_json::from_str(entry).unwrap());
            let (result, _) = run(&broker, verified);
            assert!(matches!(result, ActionResult::Failed(_)));
            assert_eq!(broker.requests.borrow().len(), 1);
        });
}

#[test]
fn legacy_wrong_side_recovery_preserves_ambiguity_without_new_market_fallback() {
    let mut broker = Scenario::audjpy();
    broker.replies = [Reply::GenericTooClose, Reply::Ambiguous, Reply::Accepted];
    broker.quote = Ok(Quote {
        bid: 110.138,
        ask: 110.142,
    });
    let (result, _) = run(&broker, verified(Direction::Long));
    assert!(
        matches!(result, ActionResult::AmbiguousSuccess(_)),
        "{}",
        result.describe()
    );
    assert_eq!(broker.requests.borrow().len(), 2);
}

#[test]
fn failures_name_the_actual_stage_and_dry_run_reports_the_whole_chain() {
    let mut broker = Scenario::audjpy();
    broker.replies[1] = Reply::AccountFailed;
    let (result, _) = run(&broker, verified(Direction::Long));
    let message = result.describe();
    [
        "account-fetch",
        "stop rejected",
        "limit failed",
        "market not attempted",
        "110.138",
        "minimum=0.01",
    ]
    .into_iter()
    .for_each(|part| assert!(message.contains(part), "missing {part}: {message}"));
    let broker = Scenario::audjpy();
    let mut verified = verified(Direction::Long);
    verified.intent.dry_run = Some(true);
    let (result, store) = run(&broker, verified);
    assert!(
        matches!(result, ActionResult::Ok(_)),
        "{}",
        result.describe()
    );
    assert!(
        result.describe().contains("market"),
        "{}",
        result.describe()
    );
    assert_eq!(*broker.sent.borrow(), 0);
    assert!(
        pollster::block_on(store.list_entry_attempts(Some("the5ers-competition"), "audjpy"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn explicit_slippage_allowance_can_only_narrow_the_broker_distance_band() {
    let broker = Scenario::audjpy();
    let mut verified = verified(Direction::Long);
    verified.intent.entry = Some(
        serde_json::from_str(
            r#"{
        "type":"stop","at":110.138,"from":"close","offset_pips":0,
        "recover_entry":{"action":"limit","max_slippage_pips":0.3}
    }"#,
        )
        .unwrap(),
    );
    let (result, _) = run(&broker, verified);
    assert!(matches!(result, ActionResult::Failed(_)));
    assert_eq!(broker.requests.borrow().len(), 2);
    assert!(
        result
            .describe()
            .contains("outside the market fallback band")
    );
}

#[test]
fn changed_market_price_must_respect_continuous_entry_veto_levels() {
    [
        (
            Direction::Long,
            Quote {
                bid: 110.139,
                ask: 110.143,
            },
            crate::intent::VetoSide::Above,
            110.140,
        ),
        (
            Direction::Short,
            Quote {
                bid: 110.133,
                ask: 110.137,
            },
            crate::intent::VetoSide::Below,
            110.136,
        ),
    ]
    .into_iter()
    .for_each(|(direction, quote, past, level)| {
        [false, true]
            .into_iter()
            .for_each(|move_during_broker_reads| {
                let mut broker = Scenario::audjpy();
                if direction == Direction::Short {
                    let pending = Quote {
                        bid: 110.143,
                        ask: 110.147,
                    };
                    broker.pending_quotes = [pending; 2];
                    broker.quote = Ok(pending);
                }
                broker.market_quote = quote;
                if !move_during_broker_reads {
                    broker.quote = Ok(quote);
                }
                let mut verified = verified(direction);
                verified.intent.entry_level_vetos = vec![crate::intent::EntryLevelVeto {
                    name: "entry-cutoff".into(),
                    level,
                    past,
                }];
                let (result, _) = run(&broker, verified);
                assert!(
                    matches!(result, ActionResult::Failed(_)),
                    "{}",
                    result.describe()
                );
                assert!(
                    result.describe().contains("entry-cutoff"),
                    "{}",
                    result.describe()
                );
                assert_eq!(*broker.sent.borrow(), 0);
            });
    });
}
