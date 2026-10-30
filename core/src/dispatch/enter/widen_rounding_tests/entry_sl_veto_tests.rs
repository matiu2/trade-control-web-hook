use super::*;
use crate::intent::{EntryLevelVeto, VetoSide};

#[test]
fn entry_sl_veto_blocks_fresh_promoted_and_replacement_entries_before_placement() {
    crate::test_logging::init_tracing();
    [
        EntryOrigin::Fresh,
        EntryOrigin::Promotion,
        EntryOrigin::Replacing {
            old_broker_order_id: "pending-1".into(),
        },
    ]
    .into_iter()
    .for_each(|origin| {
        let broker = SpyBroker::quoting(190.407, 0.038);
        let store = store();
        let mut verified = gold_enter("short", 190.407, 190.550, 190.022, 190.500);
        verified.intent.instrument = "CHF/JPY".into();
        verified.intent.tick_size = Some(0.001);
        verified.intent.entry_level_vetos = vec![EntryLevelVeto {
            name: "too-high".into(),
            level: 190.673,
            past: VetoSide::Above,
        }];
        pollster::block_on(async {
            let result = run_enter(
                &broker,
                &store,
                &verified,
                &cfg(),
                now(),
                None,
                None,
                origin,
            )
            .await;
            assert!(
                matches!(result, ActionResult::Rejected { ref outcome, .. }
                if outcome.starts_with("rejected: sl-widen-past-veto (too-high")),
                "must reject the widened stop, got {}",
                describe(&result)
            );
            assert!(
                broker.places.borrow().is_empty(),
                "no order may reach the broker"
            );
            assert!(
                store
                    .list_all_entry_attempts()
                    .await
                    .expect("attempts")
                    .is_empty()
            );
        });
    });
}

#[test]
fn entry_sl_veto_allows_widening_inside_the_boundary() {
    crate::test_logging::init_tracing();
    let broker = SpyBroker::quoting(190.407, 0.038);
    let store = store();
    let mut verified = gold_enter("short", 190.407, 190.550, 190.022, 190.500);
    verified.intent.tick_size = Some(0.001);
    verified.intent.entry_level_vetos = vec![EntryLevelVeto {
        name: "too-high".into(),
        level: 190.800,
        past: VetoSide::Above,
    }];
    let result = pollster::block_on(run_enter(
        &broker,
        &store,
        &verified,
        &cfg(),
        now(),
        None,
        None,
        EntryOrigin::Fresh,
    ));
    assert!(
        matches!(result, ActionResult::Ok(_)),
        "{}",
        describe(&result)
    );
    assert!((broker.only_placement().stop_loss - 190.787).abs() < 1e-9);
}

#[test]
fn entry_sl_veto_aborts_instead_of_parking_a_below_min_r_widen() {
    crate::test_logging::init_tracing();
    let broker = SpyBroker::quoting(190.407, 0.038);
    let store = store();
    let mut verified = gold_enter("short", 190.407, 190.550, 190.207, 190.500);
    verified.intent.tick_size = Some(0.001);
    verified.intent.entry_level_vetos = vec![EntryLevelVeto {
        name: "too-high".into(),
        level: 190.673,
        past: VetoSide::Above,
    }];
    pollster::block_on(async {
        let result = run_enter(
            &broker,
            &store,
            &verified,
            &cfg(),
            now(),
            Some("signed-entry-body"),
            None,
            EntryOrigin::Fresh,
        )
        .await;
        assert!(
            matches!(result, ActionResult::Rejected { ref outcome, .. }
            if outcome.starts_with("rejected: sl-widen-past-veto (too-high")),
            "must abort rather than park: {}",
            describe(&result)
        );
        assert!(broker.places.borrow().is_empty());
        assert!(
            crate::order_control::stored_order(&store, "gold-1")
                .await
                .expect("stored order")
                .is_none()
        );
    });
}

fn retry_policy() -> crate::spread_retry::SpreadRetry {
    crate::spread_retry::SpreadRetry {
        timing: crate::spread_retry::RetryTiming::Immediate,
        max_age_secs: 3600,
        max_chase_r: 0.25,
        lookahead_secs: 60,
        min_improvement_r: 0.05,
    }
}

fn retained_enter() -> crate::incoming::Verified {
    let mut verified = gold_enter("short", 190.407, 190.550, 190.022, 190.500);
    verified.intent.tick_size = Some(0.001);
    verified.intent.entry_dedup = Some(crate::intent::EntryDedup::GateOwned);
    verified.intent.spread_retry = Some(retry_policy());
    verified.intent.max_retries = crate::tunable::Tunable::Static(1);
    verified.intent.entry_level_vetos = vec![EntryLevelVeto {
        name: "too-high".into(),
        level: 190.673,
        past: VetoSide::Above,
    }];
    verified
}

#[test]
fn a_widening_final_quote_keeps_the_signal_parked_for_a_later_retry() {
    crate::test_logging::init_tracing();
    let wide = SpyBroker::quoting(190.500, 0.038);
    let calm = SpyBroker::quoting(190.400, 0.010);
    let store = store();
    let verified = retained_enter();
    pollster::block_on(async {
        run_enter(
            &wide,
            &store,
            &verified,
            &cfg(),
            now(),
            Some("signed-body"),
            Some(crate::broker::Granularity::H1),
            EntryOrigin::Fresh,
        )
        .await;
        // Polling sees a usable quote; the final placement check sees a wide one.
        calm.quote_steps
            .borrow_mut()
            .extend([calm.quote, wide.quote]);
        let source = RetrySource(verified);
        let first = crate::order_control::promote_due_orders(
            &calm,
            &store,
            &RetryConfig,
            &source,
            crate::order_control::PromoteScope::Every,
            now() + chrono::Duration::seconds(60),
        )
        .await;
        assert!(
            matches!(
                first[0].1,
                crate::order_control::PromoteOutcome::StillWaiting
            ),
            "{first:?}"
        );
        assert!(calm.places.borrow().is_empty());
        assert!(
            crate::order_control::stored_order(&store, "gold-1")
                .await
                .unwrap()
                .is_some()
        );
        let next = crate::order_control::promote_due_orders(
            &calm,
            &store,
            &RetryConfig,
            &source,
            crate::order_control::PromoteScope::Every,
            now() + chrono::Duration::seconds(120),
        )
        .await;
        assert!(
            matches!(next[0].1, crate::order_control::PromoteOutcome::Promoted(_)),
            "{next:?}"
        );
        assert_eq!(calm.places.borrow().len(), 1);
    });
}

#[test]
fn an_uncertain_broker_response_stops_automatic_retries() {
    crate::test_logging::init_tracing();
    let wide = SpyBroker::quoting(190.500, 0.038);
    let mut calm = SpyBroker::quoting(190.400, 0.010);
    calm.ambiguous_placement = true;
    let store = store();
    let verified = retained_enter();
    pollster::block_on(async {
        run_enter(
            &wide,
            &store,
            &verified,
            &cfg(),
            now(),
            Some("signed-body"),
            Some(crate::broker::Granularity::H1),
            EntryOrigin::Fresh,
        )
        .await;
        let source = RetrySource(verified);
        let first = crate::order_control::promote_due_orders(
            &calm,
            &store,
            &RetryConfig,
            &source,
            crate::order_control::PromoteScope::Every,
            now() + chrono::Duration::seconds(60),
        )
        .await;
        assert!(
            matches!(
                first[0].1,
                crate::order_control::PromoteOutcome::Dropped("spread-retry-ambiguous-placement")
            ),
            "{first:?}"
        );
        assert!(
            crate::order_control::stored_order(&store, "gold-1")
                .await
                .unwrap()
                .is_none()
        );
        crate::order_control::promote_due_orders(
            &calm,
            &store,
            &RetryConfig,
            &source,
            crate::order_control::PromoteScope::Every,
            now() + chrono::Duration::seconds(120),
        )
        .await;
        assert_eq!(calm.places.borrow().len(), 1);
    });
}

struct RetryConfig;
impl crate::pending_lifecycle::EnterConfigProvider for RetryConfig {
    async fn dispatch_config(&self, _: &crate::incoming::Verified) -> DispatchConfig {
        cfg()
    }
}
struct RetrySource(crate::incoming::Verified);
impl crate::pending_lifecycle::VerifiedSource for RetrySource {
    async fn recover(
        &self,
        _: &str,
        _: Option<&str>,
        _: chrono::DateTime<chrono::Utc>,
    ) -> crate::pending_lifecycle::Recovered {
        crate::pending_lifecycle::Recovered::Ok(Box::new(self.0.clone()))
    }
}

#[test]
fn opted_in_boundary_rejection_is_parked_then_promoted_at_a_fresh_bounded_quote() {
    crate::test_logging::init_tracing();
    let wide = SpyBroker::quoting(190.500, 0.038);
    let calm = SpyBroker::quoting(190.400, 0.010);
    let store = store();
    let verified = retained_enter();
    pollster::block_on(async {
        let result = run_enter(
            &wide,
            &store,
            &verified,
            &cfg(),
            now(),
            Some("signed-body"),
            Some(crate::broker::Granularity::H1),
            EntryOrigin::Fresh,
        )
        .await;
        assert!(
            matches!(result, ActionResult::Rejected { ref outcome, .. } if outcome.contains("stored until")),
            "{}",
            describe(&result)
        );
        assert!(wide.places.borrow().is_empty());
        assert!(
            crate::order_control::stored_order(&store, "gold-1")
                .await
                .unwrap()
                .unwrap()
                .spread_recovery
                .is_some()
        );
        let outcomes = crate::order_control::promote_due_orders(
            &calm,
            &store,
            &RetryConfig,
            &RetrySource(verified),
            crate::order_control::PromoteScope::Every,
            now() + chrono::Duration::seconds(60),
        )
        .await;
        assert!(
            matches!(
                &outcomes[0].1,
                crate::order_control::PromoteOutcome::Promoted(_)
            ),
            "{outcomes:?}"
        );
        assert_eq!(calm.places.borrow().len(), 1);
        assert!(
            (calm.only_placement().stop_loss - 190.550).abs() < 1e-9,
            "keep the original protective stop"
        );
        assert!(
            crate::order_control::stored_order(&store, "gold-1")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.list_all_entry_attempts().await.unwrap().len(),
            1,
            "one placement, not an attempt per poll"
        );
    });
}

#[test]
fn recovery_drops_a_runaway_signal_instead_of_chasing_after_spread_recovers() {
    crate::test_logging::init_tracing();
    let wide = SpyBroker::quoting(190.500, 0.038);
    let far = SpyBroker::quoting(190.200, 0.010);
    let store = store();
    let verified = retained_enter();
    pollster::block_on(async {
        run_enter(
            &wide,
            &store,
            &verified,
            &cfg(),
            now(),
            Some("signed-body"),
            Some(crate::broker::Granularity::H1),
            EntryOrigin::Fresh,
        )
        .await;
        let outcomes = crate::order_control::promote_due_orders(
            &far,
            &store,
            &RetryConfig,
            &RetrySource(verified),
            crate::order_control::PromoteScope::Every,
            now() + chrono::Duration::seconds(60),
        )
        .await;
        assert!(
            matches!(
                &outcomes[0].1,
                crate::order_control::PromoteOutcome::Dropped("spread-retry-price-ran-away")
            ),
            "{outcomes:?}"
        );
        assert!(far.places.borrow().is_empty());
    });
}
