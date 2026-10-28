//! Recovery must use the executable side of the spread, like broker placement.
use super::*;
use crate::broker::*;
use crate::intent::{
    Direction, RecoverEntryAction, ResolvedEntry, ResolvedRecoverEntry, RiskBudget,
};
use std::cell::RefCell;

struct QuoteBroker {
    quote: Result<Quote, LookupError>,
    placed: RefCell<Vec<ResolvedEntry>>,
}

impl Broker for QuoteBroker {
    async fn place_entry(
        &self,
        _: f64,
        _: u32,
        req: &EntryRequest<'_>,
    ) -> Result<Placement, EntryError> {
        self.placed.borrow_mut().push(req.entry.clone());
        assert_eq!(req.on_oversize, crate::intent::OversizePolicy::Reject);
        Ok(Placement::id_only("recovered-order"))
    }
    async fn get_quote(&self, _: &str) -> Result<Quote, LookupError> {
        self.quote.clone()
    }
    async fn close_positions(&self, _: &str) -> CloseOutcome {
        unreachable!("recovery must not close positions")
    }
    async fn cancel_pending_for_instrument(&self, _: &str) -> usize {
        unreachable!("recovery must not cancel orders")
    }
    async fn lookup_attempt_state(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
    ) -> Result<AttemptState, LookupError> {
        unreachable!("recovery must not look up attempts")
    }
    async fn cancel_order(&self, _: &str, _: &str) -> Result<(), CancelError> {
        unreachable!("recovery must not cancel orders")
    }
    async fn list_open_positions(&self, _: &str) -> Result<Vec<OpenPosition>, LookupError> {
        unreachable!("recovery must not list positions")
    }
    async fn amend_stop(&self, _: &str, _: &str, _: f64) -> Result<(), AmendError> {
        unreachable!("recovery must not amend stops")
    }
    async fn list_pending_orders(&self, _: &str) -> Result<Vec<PendingOrder>, LookupError> {
        unreachable!("recovery must not list orders")
    }
    async fn get_candles(
        &self,
        _: &str,
        _: Granularity,
        _: chrono::DateTime<chrono::Utc>,
        _: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<Candle>, CandleError> {
        unreachable!("recovery must not fetch candles")
    }
}

fn resolved(direction: Direction, action: RecoverEntryAction) -> Resolved {
    Resolved {
        on_oversize: crate::intent::OversizePolicy::Reject,
        setup_id: "test-setup".into(),
        id: "audjpy-enter".into(),
        not_after: "2026-10-22T12:00:00Z".parse().unwrap(),
        instrument: "AUDJPY".into(),
        direction,
        entry: ResolvedEntry::Stop {
            trigger_price: 110.138,
        },
        stop_loss: if direction == Direction::Long {
            109.720
        } else {
            110.556
        },
        take_profit: if direction == Direction::Long {
            110.999
        } else {
            109.277
        },
        risk: RiskBudget::Percent(1.0),
        min_r: 1.0,
        dry_run: false,
        recover_entry: Some(ResolvedRecoverEntry {
            action,
            max_slippage_price: Some(0.005),
        }),
        breakeven: None,
        contract_multiplier: None,
    }
}

fn recover(
    direction: Direction,
    action: RecoverEntryAction,
    quote: Result<Quote, LookupError>,
) -> (
    Result<Placement, EntryError>,
    Option<&'static str>,
    Vec<ResolvedEntry>,
) {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let broker = QuoteBroker {
        quote,
        placed: RefCell::new(vec![]),
    };
    let mut skip_reason = None;
    let result = pollster::block_on(place_entry_too_close_fallback(
        &broker,
        &resolved(direction, action),
        "audjpy-enter",
        1.0,
        10,
        &mut skip_reason,
    ));
    (result, skip_reason, broker.placed.into_inner())
}

#[test]
fn long_limit_recovers_when_ask_crossed_the_trigger_but_mid_has_not() {
    // Incident shape: mid 110.128 is below the 110.138 trigger, ask is above.
    let (result, reason, placed) = recover(
        Direction::Long,
        RecoverEntryAction::Limit,
        Ok(Quote {
            bid: 110.108,
            ask: 110.148,
        }),
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(reason, None);
    assert!(
        matches!(placed.as_slice(), [ResolvedEntry::Limit { trigger_price }] if *trigger_price == 110.138)
    );
}

#[test]
fn short_limit_recovers_when_bid_crossed_the_trigger_but_mid_has_not() {
    let (result, reason, placed) = recover(
        Direction::Short,
        RecoverEntryAction::Limit,
        Ok(Quote {
            bid: 110.128,
            ask: 110.168,
        }),
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(reason, None);
    assert!(
        matches!(placed.as_slice(), [ResolvedEntry::Limit { trigger_price }] if *trigger_price == 110.138)
    );
}

#[test]
fn market_recovery_respects_slippage_at_ask_and_bid_even_when_mid_is_within_bound() {
    [
        (
            Direction::Long,
            Quote {
                bid: 110.138,
                ask: 110.146,
            },
        ),
        (
            Direction::Short,
            Quote {
                bid: 110.130,
                ask: 110.138,
            },
        ),
    ]
    .into_iter()
    .for_each(|(direction, quote)| {
        let (result, reason, placed) = recover(direction, RecoverEntryAction::Market, Ok(quote));
        assert!(matches!(result, Err(EntryError::EntryTooCloseToMarket)));
        assert_eq!(reason, Some("recover-entry-slippage"));
        assert!(placed.is_empty());
    });
}

#[test]
fn allowed_market_recovery_sizes_from_the_executable_reference() {
    let (result, reason, placed) = recover(
        Direction::Long,
        RecoverEntryAction::Market,
        Ok(Quote {
            bid: 110.136,
            ask: 110.142,
        }),
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(reason, None);
    assert!(
        matches!(placed.as_slice(), [ResolvedEntry::Market { reference_price }] if *reference_price == 110.142)
    );
}

#[test]
fn quote_read_failure_keeps_the_skip_reason_and_submits_nothing() {
    let (result, reason, placed) = recover(
        Direction::Long,
        RecoverEntryAction::Limit,
        Err(LookupError::Transient),
    );
    assert!(matches!(result, Err(EntryError::EntryTooCloseToMarket)));
    assert_eq!(reason, Some("recover-entry-price-read-failed"));
    assert!(placed.is_empty());
}

#[test]
fn limit_recovery_still_skips_when_the_executable_price_has_not_crossed() {
    [
        (
            Direction::Long,
            Quote {
                bid: 110.110,
                ask: 110.130,
            },
        ),
        (
            Direction::Short,
            Quote {
                bid: 110.140,
                ask: 110.160,
            },
        ),
    ]
    .into_iter()
    .for_each(|(direction, quote)| {
        let (result, reason, placed) = recover(direction, RecoverEntryAction::Limit, Ok(quote));
        assert!(matches!(result, Err(EntryError::EntryTooCloseToMarket)));
        assert_eq!(reason, Some("recover-entry-limit-wrong-side"));
        assert!(placed.is_empty());
    });
}

#[test]
fn recorded_audjpy_recovery_quote_correctly_declines_the_limit() {
    // 2026-10-19 02:00:17.779Z: both ask and mid had returned below the
    // trigger, so fixing quote selection must not turn this skip into a trade.
    let (result, reason, placed) = recover(
        Direction::Long,
        RecoverEntryAction::Limit,
        Ok(Quote {
            bid: 110.123,
            ask: 110.133,
        }),
    );
    assert!(matches!(result, Err(EntryError::EntryTooCloseToMarket)));
    assert_eq!(reason, Some("recover-entry-limit-wrong-side"));
    assert!(placed.is_empty());
}
