//! `POST /admin/adopt-trade` — hand the worker an existing broker
//! position so the rest of the worker (close, pause/resume, sweep,
//! retry-gate) can manage it as if the worker had placed it itself.
//!
//! Motivation: the worker is not the source of truth for open trades —
//! it only tracks trades it placed. When the operator opens a trade
//! manually in the broker UI (or by any non-worker path) and wants the
//! webhook lifecycle to run against it, the row has to be injected
//! by hand. This endpoint does that injection with broker-side
//! verification so a typo'd position id doesn't silently land a row
//! that close alerts will then no-op against.
//!
//! Shape of an adopted row:
//!
//! - `broker_order_id` and `broker_trade_id` are BOTH populated up
//!   front. That matches what the worker would have written for a
//!   self-placed trade after the lookup snapshot — no sentinel value
//!   anywhere, no special-case in [`crate::tradenation_adapter`].
//! - `attempt_no: 1`. If the operator later wants retries on this
//!   trade_id they layer normally on top.
//! - `placed_at` = `shell_time` = `now` (we have no original signal
//!   bar to point at).
//! - `expires_at` is inferred from the seen-index: take the max
//!   `expires_at` across SeenEntry rows that carry this `trade_id`,
//!   then strip the 1h grace [`incoming::replay_ttl_seconds`] applies.
//!   When no SeenEntry matches (no prep/veto/enter alert ever landed,
//!   or they've already aged out), fall back to a 4-day default —
//!   long enough for the typical H&S window the worker is tuned for.

// The wasm handler in `admin.rs` is the only non-test caller. Under
// the native build it's `#[cfg]`-gated out, so without this allow the
// helpers all read as dead code. Tests still run on native.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use trade_control_core::intent::Direction;
use trade_control_core::state::SeenEntry;

/// Request body for `POST /admin/adopt-trade`. Constructed by
/// `trade-control adopt-trade` and posted with `X-Admin-Key` auth.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdoptRequest {
    /// Worker-configured account name, e.g. `tn-reversals-demo`.
    pub account: String,
    /// Trade grouping id from the original alert pipeline, e.g.
    /// `hs-chf-jpy-efd5e647`. The same string used on the H&S
    /// build-trade output.
    pub trade_id: String,
    /// Broker's market name as the operator entered it. Matched
    /// case-insensitively against the broker's open positions (same
    /// matching policy as [`crate::tradenation_adapter`]).
    pub instrument: String,
    pub direction: Direction,
    /// `AddOrder` id from the broker UI (the originating order id).
    pub broker_order_id: String,
    /// `OpenPosition` id from the broker UI (the distinct position id
    /// on TN; equals order id on OANDA).
    pub broker_trade_id: String,
    /// Resolved stop-loss price. Optional — the sweep skips rows
    /// missing it. Recommended so the SL-breach watcher can act on
    /// the adopted trade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_loss_price: Option<f64>,
}

/// Verdict from comparing the request against the broker's live
/// positions. The endpoint converts each failure into a 409 with the
/// human-readable reason.
#[derive(Debug, PartialEq)]
pub enum VerifyOutcome {
    /// Position matches on every field; safe to write the row.
    Ok,
    /// No open position matches both `broker_order_id` and
    /// `broker_trade_id` on the account.
    NotFound,
    /// Position exists but its market name doesn't match
    /// `req.instrument`.
    InstrumentMismatch { broker_name: String },
    /// Position exists but its direction doesn't match `req.direction`.
    DirectionMismatch { broker_direction: String },
}

impl VerifyOutcome {
    /// One-line human reason, suitable for the 409 body and the seen
    /// index outcome string.
    pub fn reason(&self) -> String {
        match self {
            Self::Ok => "ok".into(),
            Self::NotFound => "no open position matches the supplied order_id + position_id".into(),
            Self::InstrumentMismatch { broker_name } => {
                format!("instrument mismatch: broker says {broker_name}")
            }
            Self::DirectionMismatch { broker_direction } => {
                format!("direction mismatch: broker says {broker_direction}")
            }
        }
    }
}

/// Compare an [`AdoptRequest`] against a slice of broker positions.
/// Pure helper — keeps the verify logic testable without spinning a
/// real broker session.
///
/// Matching is keyed on `(order_id, position_id)`. We don't trust the
/// caller's instrument / direction; we read them off the matched
/// position and report a mismatch if they disagree. Catches both
/// transposed ids and stale ids that happen to belong to a different
/// trade.
pub fn verify_position<P: BrokerPosition>(req: &AdoptRequest, positions: &[P]) -> VerifyOutcome {
    let matched = positions
        .iter()
        .find(|p| p.order_id() == req.broker_order_id && p.position_id() == req.broker_trade_id);
    let Some(p) = matched else {
        return VerifyOutcome::NotFound;
    };
    if !instrument_matches(p.market_name(), &req.instrument) {
        return VerifyOutcome::InstrumentMismatch {
            broker_name: p.market_name().to_string(),
        };
    }
    if !direction_matches(p.direction_label(), req.direction) {
        return VerifyOutcome::DirectionMismatch {
            broker_direction: p.direction_label().to_string(),
        };
    }
    VerifyOutcome::Ok
}

/// Broker-agnostic view of one open position, narrowed to the fields
/// `verify_position` needs. Implemented for `tradenation_api::Position`
/// in the wasm handler; in tests we use a local stub. Keeps the
/// `tradenation-api` types out of the pure-helper signature so the
/// `#[cfg(test)]` block doesn't need to construct one of every field.
pub trait BrokerPosition {
    fn order_id(&self) -> String;
    fn position_id(&self) -> String;
    fn market_name(&self) -> &str;
    /// `"Buy"` / `"Sell"` on TradeNation; comparison is
    /// case-insensitive against `req.direction`.
    fn direction_label(&self) -> &str;
}

fn instrument_matches(broker: &str, requested: &str) -> bool {
    broker.eq_ignore_ascii_case(requested)
}

fn direction_matches(broker_label: &str, requested: Direction) -> bool {
    let want = match requested {
        Direction::Long => "buy",
        Direction::Short => "sell",
    };
    broker_label.eq_ignore_ascii_case(want)
}

/// Default lifetime applied when no SeenEntry anchors the adopted
/// trade. Chosen to match the typical H&S `not_after` window the
/// worker is tuned for — long enough that a close alert sent within
/// the usual setup horizon will find the row, short enough that
/// abandoned adopt rows age out instead of pinning forever.
pub const DEFAULT_EXPIRY_DAYS: i64 = 4;

/// Resolve the `expires_at` field for the new `EntryAttempt`.
///
/// Walks `seen` for entries whose `trade_id` matches and picks the
/// latest `expires_at`. The `+1h` grace [`incoming::replay_ttl_seconds`]
/// adds is stripped so the result matches the trade's original
/// `not_after` — the `EntryAttempt` TTL plumbing will re-add the grace
/// the same way a self-placed row would have.
///
/// When nothing matches (no signed alert ever landed for this trade,
/// or they've already aged out of the index), fall back to
/// `now + DEFAULT_EXPIRY_DAYS`. Silent fallback per design — the
/// alternative is forcing the operator to type a number they almost
/// always want to be "whatever the original alerts said," and the
/// default lines up with the typical H&S horizon.
pub fn resolve_not_after(trade_id: &str, seen: &[SeenEntry], now: DateTime<Utc>) -> DateTime<Utc> {
    let grace = Duration::hours(1);
    let inferred = seen
        .iter()
        .filter(|e| e.trade_id.as_deref() == Some(trade_id))
        .map(|e| e.expires_at)
        .max();
    match inferred {
        Some(latest) => latest - grace,
        None => now + Duration::days(DEFAULT_EXPIRY_DAYS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_core::intent::Action;

    struct Stub {
        order_id: &'static str,
        position_id: &'static str,
        market_name: &'static str,
        direction: &'static str,
    }

    impl BrokerPosition for Stub {
        fn order_id(&self) -> String {
            self.order_id.into()
        }
        fn position_id(&self) -> String {
            self.position_id.into()
        }
        fn market_name(&self) -> &str {
            self.market_name
        }
        fn direction_label(&self) -> &str {
            self.direction
        }
    }

    fn req(direction: Direction, instrument: &str) -> AdoptRequest {
        AdoptRequest {
            account: "tn-reversals-demo".into(),
            trade_id: "hs-chf-jpy-efd5e647".into(),
            instrument: instrument.into(),
            direction,
            broker_order_id: "26773227".into(),
            broker_trade_id: "27169081".into(),
            stop_loss_price: Some(173.5),
        }
    }

    #[test]
    fn verify_ok_on_exact_match() {
        let positions = vec![Stub {
            order_id: "26773227",
            position_id: "27169081",
            market_name: "CHF/JPY",
            direction: "Sell",
        }];
        assert_eq!(
            verify_position(&req(Direction::Short, "CHF/JPY"), &positions),
            VerifyOutcome::Ok
        );
    }

    #[test]
    fn verify_ok_case_insensitive_instrument() {
        // Operator types `chf/jpy`, broker returns `CHF/JPY`.
        let positions = vec![Stub {
            order_id: "26773227",
            position_id: "27169081",
            market_name: "CHF/JPY",
            direction: "Sell",
        }];
        assert_eq!(
            verify_position(&req(Direction::Short, "chf/jpy"), &positions),
            VerifyOutcome::Ok
        );
    }

    #[test]
    fn verify_not_found_when_neither_id_matches() {
        let positions = vec![Stub {
            order_id: "11111111",
            position_id: "22222222",
            market_name: "CHF/JPY",
            direction: "Sell",
        }];
        assert_eq!(
            verify_position(&req(Direction::Short, "CHF/JPY"), &positions),
            VerifyOutcome::NotFound
        );
    }

    #[test]
    fn verify_not_found_when_only_one_id_matches() {
        // Order id matches a different position than the position id
        // matches — likely a copy/paste error. Refuse rather than
        // pick one.
        let positions = vec![
            Stub {
                order_id: "26773227",
                position_id: "99999999",
                market_name: "CHF/JPY",
                direction: "Sell",
            },
            Stub {
                order_id: "00000000",
                position_id: "27169081",
                market_name: "EUR/USD",
                direction: "Buy",
            },
        ];
        assert_eq!(
            verify_position(&req(Direction::Short, "CHF/JPY"), &positions),
            VerifyOutcome::NotFound
        );
    }

    #[test]
    fn verify_instrument_mismatch_surfaces_broker_name() {
        let positions = vec![Stub {
            order_id: "26773227",
            position_id: "27169081",
            market_name: "EUR/USD",
            direction: "Sell",
        }];
        let outcome = verify_position(&req(Direction::Short, "CHF/JPY"), &positions);
        match outcome {
            VerifyOutcome::InstrumentMismatch { broker_name } => assert_eq!(broker_name, "EUR/USD"),
            other => panic!("expected InstrumentMismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_direction_mismatch_surfaces_broker_label() {
        // Operator believed they were short, broker says it's a Buy.
        // The most common real-world cause is mis-typing direction on
        // the CLI; refuse rather than write a wrong-direction row.
        let positions = vec![Stub {
            order_id: "26773227",
            position_id: "27169081",
            market_name: "CHF/JPY",
            direction: "Buy",
        }];
        let outcome = verify_position(&req(Direction::Short, "CHF/JPY"), &positions);
        match outcome {
            VerifyOutcome::DirectionMismatch { broker_direction } => {
                assert_eq!(broker_direction, "Buy")
            }
            other => panic!("expected DirectionMismatch, got {other:?}"),
        }
    }

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn seen(trade_id: Option<&str>, expires: &str) -> SeenEntry {
        SeenEntry {
            id: "msg-id".into(),
            action: Action::Enter,
            seen_at: None,
            outcome: String::new(),
            expires_at: ts(expires),
            trade_id: trade_id.map(str::to_string),
        }
    }

    #[test]
    fn resolve_not_after_picks_latest_matching_seen_entry() {
        // Three SeenEntries for our trade_id with different
        // `expires_at` (typical: prep, veto, enter alerts landed at
        // different times, each with the same `not_after` but slightly
        // different arrival timestamps → grace-window expiries vary
        // by a few seconds). Take the max so we honour whichever
        // alert was processed latest.
        let now = ts("2026-06-03T10:00:00Z");
        let trade_id = "hs-chf-jpy-efd5e647";
        let entries = vec![
            seen(Some(trade_id), "2026-06-07T10:00:00Z"),
            seen(Some(trade_id), "2026-06-07T10:00:30Z"),
            seen(Some(trade_id), "2026-06-07T09:59:00Z"),
            // Unrelated trade — must not influence the result.
            seen(Some("hs-eur-usd-deadbeef"), "2026-07-01T00:00:00Z"),
            // Pre-grouping entry with no trade_id — also ignored.
            seen(None, "2026-08-01T00:00:00Z"),
        ];
        let resolved = resolve_not_after(trade_id, &entries, now);
        // Grace stripped: 2026-06-07T10:00:30Z - 1h.
        assert_eq!(resolved, ts("2026-06-07T09:00:30Z"));
    }

    #[test]
    fn resolve_not_after_defaults_when_no_matching_seen() {
        // No SeenEntry for our trade_id — fall back to now + 4d.
        let now = ts("2026-06-03T10:00:00Z");
        let entries = vec![seen(Some("other-trade"), "2026-06-07T00:00:00Z")];
        let resolved = resolve_not_after("hs-chf-jpy-efd5e647", &entries, now);
        assert_eq!(resolved, ts("2026-06-07T10:00:00Z"));
    }

    #[test]
    fn resolve_not_after_defaults_on_empty_index() {
        let now = ts("2026-06-03T10:00:00Z");
        let resolved = resolve_not_after("hs-chf-jpy-efd5e647", &[], now);
        assert_eq!(resolved, ts("2026-06-07T10:00:00Z"));
    }
}
