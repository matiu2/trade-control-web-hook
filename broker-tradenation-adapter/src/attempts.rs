//! TradeNation transport and normalization; lifecycle decisions live in core.
use super::{CLOSED_TRADE_HISTORY_DAYS, TradeNationAdapter};
use std::time::{Duration, Instant};
use trade_control_core::{
    attempt_resolution::{self, Event, EventKind, Evidence, Observation, Position},
    broker::{AttemptState, LookupError},
    intent::Direction,
};
use tradenation_api::ActivityRecord;

#[derive(Clone)]
pub(super) struct History {
    fetched: Instant,
    events: Vec<Event>,
    complete: bool,
}

impl TradeNationAdapter {
    pub(super) async fn resolve_attempt(
        &self,
        instrument: &str,
        order_id: &str,
        position_id: Option<&str>,
    ) -> Result<Observation, LookupError> {
        let details = tradenation_api::get_account_details(self.0.session())
            .await
            .map_err(|err| {
                tracing::error!(order_id, ?err, "tn attempt current-state lookup failed");
                LookupError::Transient
            })?;
        // The adapter's authenticated session scopes every input to ONE account.
        let mut evidence = Evidence {
            pending: details
                .opening_orders
                .records
                .iter()
                .filter(|p| same_instrument(&p.market, instrument))
                .map(|p| p.order_id.to_string())
                .collect(),
            positions: details
                .positions
                .records
                .iter()
                .filter(|p| same_instrument(&p.market_name, instrument))
                .map(|p| Position {
                    id: p.position_id.to_string(),
                    order_id: p.order_id.to_string(),
                    entry_price: Some(p.opening_price),
                    opened_at: p.creation_time.map(|t| t.to_utc()),
                })
                .collect(),
            ..Evidence::default()
        };
        let direct = attempt_resolution::resolve(order_id, position_id, &evidence);
        if !matches!(direct.state, AttemptState::Unknown) {
            return Ok(direct);
        }
        let history = match self.activity_history().await {
            Ok(history) => history,
            Err(err) => {
                tracing::warn!(order_id, evidence = ?direct.diagnostic, "tn attempt history unavailable");
                return Err(err);
            }
        };
        evidence.events = history
            .events
            .into_iter()
            .filter(|e| same_instrument(&e.instrument, instrument))
            .collect();
        evidence.history_complete = history.complete;
        Ok(attempt_resolution::resolve(
            order_id,
            position_id,
            &evidence,
        ))
    }

    async fn activity_history(&self) -> Result<History, LookupError> {
        let cached = self.2.lock().ok().and_then(|guard| guard.clone());
        if let Some(history) = cached
            && history.complete
            && history.fetched.elapsed() < Duration::from_secs(30)
        {
            return Ok(history);
        }
        let mut events = Vec::new();
        let mut total = None;
        let mut complete = false;
        let mut malformed = false;
        // Bound work. Exhausted/truncated or changing pagination NEVER grants
        // fallback permission; the next observation retries the history.
        for page in 0..200 {
            let result = tradenation_api::get_activity(
                self.0.client(),
                self.0.session(),
                CLOSED_TRADE_HISTORY_DAYS,
                page,
            )
            .await
            .map_err(|err| {
                tracing::error!(page, ?err, "tn attempt activity page failed");
                LookupError::Transient
            })?;
            let stable = total.is_none_or(|n| n == result.total_pages);
            total = Some(result.total_pages);
            let empty = result.records.is_empty();
            malformed |= result
                .records
                .iter()
                .any(|row| recognized_event(row) && activity_event(row).is_none());
            events.extend(result.records.iter().filter_map(activity_event));
            if !stable || (result.total_pages > 0 && result.current_page != page + 1) {
                break;
            }
            if result.total_pages == 0 {
                complete = empty && page == 0;
                break;
            }
            if empty {
                break;
            }
            if page + 1 == result.total_pages {
                complete = true;
                break;
            }
        }
        if malformed {
            tracing::warn!("tn activity has malformed lifecycle rows; fallback disabled");
        }
        let history = History {
            fetched: Instant::now(),
            events,
            complete: complete && !malformed,
        };
        if let Ok(mut cache) = self.2.lock() {
            *cache = Some(history.clone());
        }
        Ok(history)
    }
}

fn same_instrument(a: &str, b: &str) -> bool {
    let canonical = |s: &str| {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .flat_map(char::to_uppercase)
            .collect::<String>()
    };
    canonical(a) == canonical(b)
}

fn recognized_event(row: &ActivityRecord) -> bool {
    [
        "Execute Order",
        "Open Position",
        "Close Position",
        "Delete Order",
    ]
    .iter()
    .any(|verb| row.result.starts_with(verb))
}

pub(super) fn activity_event(row: &ActivityRecord) -> Option<Event> {
    let (verb, rest) = row.result.split_once(':')?;
    let kind = match verb.trim() {
        "Execute Order" => EventKind::Execute,
        "Open Position" => EventKind::Open,
        "Close Position" => EventKind::Close,
        "Delete Order" => EventKind::Cancel,
        _ => return None,
    };
    let id = rest.split_whitespace().next()?.to_owned();
    let stake: f64 = row.stake.parse().ok()?;
    if !stake.is_finite() || stake <= 0.0 || row.market.is_empty() || row.currency.is_empty() {
        return None;
    }
    let price = row.price.or(row.stop_order_price).or(row.limit_order_price);
    if price.is_some_and(|p| !p.is_finite() || p <= 0.0) {
        return None;
    }
    let direction = match row.direction.to_ascii_lowercase().as_str() {
        "buy" => Direction::Long,
        "sell" => Direction::Short,
        _ => return None,
    };
    Some(Event {
        kind,
        id,
        instrument: row.market.clone(),
        direction,
        currency: row.currency.clone(),
        stake,
        at: row.transaction_date?.to_utc(),
        price,
    })
}

#[cfg(test)]
mod tests {
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

    fn events() -> Vec<Event> {
        init_tracing();
        let rows: Vec<ActivityRecord> =
            serde_json::from_str(include_str!("../tests/fixtures/audusd-2026-10-06.json")).unwrap();
        rows.iter().filter_map(activity_event).collect()
    }
    #[test]
    fn actual_export_recovers_all_three_entry_orders_after_they_closed() {
        let evidence = Evidence {
            events: events(),
            history_complete: true,
            ..Evidence::default()
        };
        for (order, position, price) in [
            ("27024719", "27390230", 0.69669),
            ("27030154", "27395937", 0.69552),
            ("27028564", "27394334", 0.69490),
        ] {
            let AttemptState::ClosedConfirmed { exit } =
                attempt_resolution::resolve(order, None, &evidence).state
            else {
                panic!("must resolve {order}")
            };
            assert_eq!(exit.broker_trade_id, position);
            assert_eq!(exit.exit_price, Some(price));
            assert_eq!(exit.realized_pl, None);
        }
    }
    #[test]
    fn actual_export_recovers_third_open_position_with_changed_order_id() {
        let evidence = Evidence {
            events: events()
                .into_iter()
                .filter(|e| e.kind != EventKind::Close)
                .collect(),
            history_complete: true,
            positions: vec![Position {
                id: "27395937".into(),
                order_id: "27030227".into(),
                entry_price: Some(0.69552),
                opened_at: None,
            }],
            ..Evidence::default()
        };
        assert_eq!(
            attempt_resolution::resolve("27030154", None, &evidence).state,
            AttemptState::OpenPosition {
                broker_trade_id: "27395937".into()
            }
        );
    }
    #[test]
    fn exact_close_must_not_match_a_position_id_prefix() {
        let evidence = Evidence {
            events: events(),
            history_complete: true,
            ..Evidence::default()
        };
        assert_eq!(
            attempt_resolution::resolve("unknown", Some("2739023"), &evidence).state,
            AttemptState::Unknown
        );
    }
}
