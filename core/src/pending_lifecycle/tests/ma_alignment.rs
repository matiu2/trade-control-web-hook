use super::*;

#[test]
fn ma_pause_preserves_an_order_past_the_backstop_and_respects_sibling_pauses() {
    exercise(false);
}

#[test]
fn ma_recovery_cannot_restore_an_order_after_another_veto() {
    exercise(true);
}

fn exercise(veto: bool) {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let broker = MockBroker::with_pending(pending("order-redriven", "AUD/CHF"));
    broker.set_quote(0.5600, 0.5602);
    let store = MemStateStore::new();
    let now = ts("2026-07-08T12:00:00Z");
    store.set_clock(now);
    let mut verified = armed_verified("AUD/CHF");
    verified.intent.not_after = now + Duration::days(3);
    verified.intent.vetos.push("other-veto".into());
    let body = crate::resign::resign(&verified, &KEY).unwrap();
    let placed = run(async {
        crate::dispatch::run_enter(
            &broker,
            &store,
            &verified,
            &StubCfg.dispatch_config(&verified).await,
            now,
            Some(&body),
            Some(Granularity::H1),
            crate::dispatch::EntryOrigin::Fresh,
        )
        .await
    });
    assert!(matches!(placed, crate::dispatch::ActionResult::Ok(_)));
    run(async {
        store
            .set_pause(
                "t",
                crate::ma_cross::ALIGNMENT_PAUSE_ID,
                None,
                now,
                3 * 86400,
            )
            .await
            .unwrap();
        let held = pending_order_lifecycle(
            &broker,
            &store,
            &StubCfg,
            &src(),
            Some("reversals"),
            now,
            ClearPolicy::ClearRecord,
        )
        .await;
        assert_eq!(held.cancelled, ["order-redriven"]);
        broker.pendings.borrow_mut().clear();
        let later = now + Duration::hours(24);
        store.set_clock(later);
        let held = pending_order_lifecycle(
            &broker,
            &store,
            &StubCfg,
            &src(),
            Some("reversals"),
            later,
            ClearPolicy::ClearRecord,
        )
        .await;
        assert!(
            held.restored.is_empty(),
            "an MA pause must survive the 12h backstop"
        );
        let record = store
            .get_held_trade_record("t")
            .await
            .unwrap()
            .expect("held order must not expire during an MA pause");
        assert_eq!(record.cancelled_orders.len(), 1);
        // Another pause must remain effective after the MA-specific resume.
        store
            .set_pause("t", "news-test", None, later, 3600)
            .await
            .unwrap();
        store
            .clear_pause("t", crate::ma_cross::ALIGNMENT_PAUSE_ID)
            .await
            .unwrap();
        assert!(!off(&broker, &store, &record, later).await);
        let sibling = pending_order_lifecycle(
            &broker,
            &store,
            &StubCfg,
            &src(),
            Some("reversals"),
            later,
            ClearPolicy::ClearRecord,
        )
        .await;
        assert!(sibling.restored.is_empty());
        assert!(store.get_held_trade_record("t").await.unwrap().is_some());
        store.clear_pause("t", "news-test").await.unwrap();
        if veto {
            store
                .set_veto(
                    verified.intent.account.as_deref(),
                    "t",
                    "AUD/CHF",
                    "other-veto",
                    86400,
                )
                .await
                .unwrap();
        }
        assert!(off(&broker, &store, &record, later).await);
        broker
            .place_ids
            .borrow_mut()
            .push_back("restored-after-alignment".into());
        let resumed = pending_order_lifecycle(
            &broker,
            &store,
            &StubCfg,
            &src(),
            Some("reversals"),
            later,
            ClearPolicy::ClearRecord,
        )
        .await;
        assert_eq!(resumed.restored, [("t".into(), RestoreReason::Recovered)]);
        assert_eq!(
            broker.place_ids.borrow().is_empty(),
            !veto,
            "the preserved order is re-placed only if no veto has fired"
        );
    });
}
