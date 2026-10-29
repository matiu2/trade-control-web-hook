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
