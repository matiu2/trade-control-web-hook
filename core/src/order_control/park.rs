//! Persisting a [`StoredOrder`](super::StoredOrder): park, promote, drop.
//!
//! The effectful half of [`super::stored`] — that module holds the decisions,
//! this one moves them in and out of the [`StateStore`]. Split so the decisions
//! stay unit-testable without a store, and so there is exactly one place that
//! knows how a Stored order is persisted.
//!
//! Stored orders live on [`HeldTradeRecord::stored_orders`] rather than in their
//! own table: they are per-trade, TTL'd on the same clock as everything else the
//! trade is holding, and the record body is one `jsonb` — so this needed **no
//! SQL migration**, exactly as `holders` didn't in v120.

use chrono::{DateTime, Utc};

use super::stored::StoredOrder;
use crate::state::{HeldTradeRecord, StateError, StateStore};

/// Park an intended-but-unplaced order, or replace the one already parked for
/// this trade.
///
/// **Replace, not append.** A trade has at most one Stored order: a fresher
/// signal for the same setup *supersedes* the stale one rather than queueing
/// behind it (rule 3 — a newer read of price is more current, therefore
/// superior). Appending would let one setup accumulate parked orders and then
/// promote several of them when the spread calmed.
///
/// Creates the record if the trade has none. An existing record is preserved
/// otherwise — its holders, remembered stops and cancelled orders all belong to
/// other subsystems and must survive a park.
pub async fn park_order<S: StateStore>(
    store: &S,
    trade_id: &str,
    instrument: &str,
    account: Option<&str>,
    order: StoredOrder,
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<(), StateError> {
    let existing = store.get_held_trade_record(trade_id).await?;
    let mut record = existing.unwrap_or_else(|| HeldTradeRecord {
        trade_id: trade_id.to_string(),
        instrument: instrument.to_string(),
        account: account.map(|s| s.to_string()),
        // `applied` means "this record mutated something at the broker" and is
        // System 2's idempotency guard. A Stored order has by definition NOT
        // touched the broker, so parking must never set it — doing so would
        // make System 2 skip a genuine widen later.
        applied: false,
        holders: crate::hold::Holders::new(),
        opened_at: now,
        expires_at,
        pip_size: 0.0,
        original_stops: Vec::new(),
        cancelled_orders: Vec::new(),
        stored_orders: Vec::new(),
    });
    record.stored_orders = vec![order];
    // Keep the record alive at least as long as the parked order needs to be
    // promotable; a shorter TTL would age the park out from under itself.
    record.expires_at = record.expires_at.max(expires_at);
    let ttl = (record.expires_at - now).num_seconds().max(0) as u64;
    store.upsert_held_trade_record(&record, ttl).await
}

/// The order parked for this trade, if any.
pub async fn stored_order<S: StateStore>(
    store: &S,
    trade_id: &str,
) -> Result<Option<StoredOrder>, StateError> {
    Ok(store
        .get_held_trade_record(trade_id)
        .await?
        .and_then(|r| r.stored_orders.into_iter().next()))
}

/// Remove the parked order for this trade — on promotion, on drop, or when a
/// fresher signal supersedes it.
///
/// Clears **only** the stored order. The record itself is left in place when it
/// still carries anything else (holders, remembered stops, cancelled orders),
/// because those belong to other subsystems whose lifecycles are independent of
/// this one. An otherwise-empty record is cleared entirely so a park doesn't
/// leave a husk behind for the rest of the trade's window.
pub async fn clear_stored_order<S: StateStore>(
    store: &S,
    trade_id: &str,
    now: DateTime<Utc>,
) -> Result<(), StateError> {
    let Some(mut record) = store.get_held_trade_record(trade_id).await? else {
        return Ok(());
    };
    record.stored_orders.clear();
    let still_needed = !record.holders.is_empty()
        || !record.original_stops.is_empty()
        || !record.cancelled_orders.is_empty()
        || record.applied;
    if still_needed {
        let ttl = (record.expires_at - now).num_seconds().max(0) as u64;
        store.upsert_held_trade_record(&record, ttl).await
    } else {
        store.clear_held_trade_record(trade_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hold::HoldReason;
    use crate::order_control::{StoredOrder, StoredReason};
    use crate::state::MemStateStore;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid test timestamp")
            .with_timezone(&Utc)
    }

    fn order(shell: &str, sl: f64) -> StoredOrder {
        StoredOrder {
            signed_intent: format!("{{\"shell\":\"{shell}\"}}"),
            reason: StoredReason::BelowMinR,
            original_sl_distance: sl,
            tp_distance: 0.0200,
            min_r: 1.0,
            stored_at: at(shell),
            drop_at: at("2026-07-23T21:00:00Z"),
            shell_time: at(shell),
        }
    }

    #[test]
    fn park_then_read_back() {
        let store = MemStateStore::default();
        let now = at("2026-07-22T13:30:00Z");
        pollster::block_on(park_order(
            &store,
            "t-1",
            "SGD_JPY",
            None,
            order("2026-07-22T13:30:00Z", 0.0020),
            at("2026-07-24T00:00:00Z"),
            now,
        ))
        .expect("park");

        let got = pollster::block_on(stored_order(&store, "t-1")).expect("read");
        let got = got.expect("a parked order");
        assert_eq!(got.reason, StoredReason::BelowMinR);
        assert!((got.original_sl_distance - 0.0020).abs() < 1e-12);
    }

    /// Rule 3: a fresher signal REPLACES the parked order rather than queueing
    /// behind it. Two parks must leave exactly one stored order — appending
    /// would let one setup promote several entries when the spread calmed.
    #[test]
    fn a_fresher_signal_supersedes_rather_than_queues() {
        let store = MemStateStore::default();
        let now = at("2026-07-22T13:30:00Z");
        let expiry = at("2026-07-24T00:00:00Z");
        for (shell, sl) in [
            ("2026-07-22T13:30:00Z", 0.0020),
            ("2026-07-22T14:30:00Z", 0.0031),
        ] {
            pollster::block_on(park_order(
                &store,
                "t-1",
                "SGD_JPY",
                None,
                order(shell, sl),
                expiry,
                now,
            ))
            .expect("park");
        }
        let record = pollster::block_on(store.get_held_trade_record("t-1"))
            .expect("get")
            .expect("record");
        assert_eq!(record.stored_orders.len(), 1, "exactly one parked order");
        assert!(
            (record.stored_orders[0].original_sl_distance - 0.0031).abs() < 1e-12,
            "the FRESHER signal wins — it reads current price",
        );
    }

    /// Parking must not set `applied`: that flag means "this record mutated
    /// something at the broker" and is System 2's idempotency guard. A Stored
    /// order has never reached the broker, so setting it would make a later
    /// genuine widen skip itself.
    #[test]
    fn parking_does_not_set_applied() {
        let store = MemStateStore::default();
        let now = at("2026-07-22T13:30:00Z");
        pollster::block_on(park_order(
            &store,
            "t-1",
            "SGD_JPY",
            None,
            order("2026-07-22T13:30:00Z", 0.0020),
            at("2026-07-24T00:00:00Z"),
            now,
        ))
        .expect("park");
        let record = pollster::block_on(store.get_held_trade_record("t-1"))
            .expect("get")
            .expect("record");
        assert!(!record.applied, "a park never touches the broker");
    }

    /// A park must not trample state other subsystems own — a trade can be
    /// holding resting orders AND have a parked entry at the same time.
    #[test]
    fn park_preserves_holders_on_an_existing_record() {
        let store = MemStateStore::default();
        let now = at("2026-07-22T13:30:00Z");
        let expiry = at("2026-07-24T00:00:00Z");
        let mut record = HeldTradeRecord {
            trade_id: "t-1".into(),
            instrument: "SGD_JPY".into(),
            account: None,
            applied: true,
            holders: crate::hold::Holders::new(),
            opened_at: now,
            expires_at: expiry,
            pip_size: 0.01,
            original_stops: Vec::new(),
            cancelled_orders: Vec::new(),
            stored_orders: Vec::new(),
        };
        record.holders.hold(HoldReason::NewsPause);
        pollster::block_on(store.upsert_held_trade_record(&record, 3600)).expect("seed");

        pollster::block_on(park_order(
            &store,
            "t-1",
            "SGD_JPY",
            None,
            order("2026-07-22T13:30:00Z", 0.0020),
            expiry,
            now,
        ))
        .expect("park");

        let got = pollster::block_on(store.get_held_trade_record("t-1"))
            .expect("get")
            .expect("record");
        assert!(
            got.holders.contains(HoldReason::NewsPause),
            "a park must not drop another subsystem's hold",
        );
        assert!((got.pip_size - 0.01).abs() < 1e-12, "nor its pip size");
        assert_eq!(got.stored_orders.len(), 1);
    }

    /// Clearing the last stored order on an otherwise-empty record removes the
    /// record, rather than leaving a husk for the rest of the trade's window.
    #[test]
    fn clearing_an_otherwise_empty_record_removes_it() {
        let store = MemStateStore::default();
        let now = at("2026-07-22T13:30:00Z");
        pollster::block_on(park_order(
            &store,
            "t-1",
            "SGD_JPY",
            None,
            order("2026-07-22T13:30:00Z", 0.0020),
            at("2026-07-24T00:00:00Z"),
            now,
        ))
        .expect("park");
        pollster::block_on(clear_stored_order(&store, "t-1", now)).expect("clear");
        assert!(
            pollster::block_on(store.get_held_trade_record("t-1"))
                .expect("get")
                .is_none(),
            "no husk left behind",
        );
    }

    /// ...but a record another subsystem still needs SURVIVES the clear.
    #[test]
    fn clearing_keeps_a_record_another_subsystem_still_needs() {
        let store = MemStateStore::default();
        let now = at("2026-07-22T13:30:00Z");
        let expiry = at("2026-07-24T00:00:00Z");
        let mut record = HeldTradeRecord {
            trade_id: "t-1".into(),
            instrument: "SGD_JPY".into(),
            account: None,
            applied: false,
            holders: crate::hold::Holders::new(),
            opened_at: now,
            expires_at: expiry,
            pip_size: 0.01,
            original_stops: Vec::new(),
            cancelled_orders: Vec::new(),
            stored_orders: vec![order("2026-07-22T13:30:00Z", 0.0020)],
        };
        record.holders.hold(HoldReason::SpreadHour);
        pollster::block_on(store.upsert_held_trade_record(&record, 3600)).expect("seed");

        pollster::block_on(clear_stored_order(&store, "t-1", now)).expect("clear");

        let got = pollster::block_on(store.get_held_trade_record("t-1"))
            .expect("get")
            .expect("record must survive — a hold still needs it");
        assert!(got.stored_orders.is_empty(), "but the park is gone");
        assert!(got.holders.contains(HoldReason::SpreadHour));
    }

    #[test]
    fn clearing_an_absent_record_is_benign() {
        let store = MemStateStore::default();
        pollster::block_on(clear_stored_order(
            &store,
            "nope",
            at("2026-07-22T13:30:00Z"),
        ))
        .expect("absent record is not an error");
    }
}
