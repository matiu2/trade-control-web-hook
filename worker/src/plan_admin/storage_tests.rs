use super::mapping::tests::plan;
use super::storage::transition;
use crate::PgStateStore;
use chrono::Utc;
use serde_json::{Value, json};
use trade_control_core::{
    account::{AccountKind, AccountMetadata},
    intent::Action,
    plan_state::{Phase, PlanState},
    state::StateStore,
};

async fn fixture() -> Option<(PgStateStore, trade_control_core::trade_plan::TradePlan)> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        tracing::warn!("skipping plan administration DB test without TEST_DATABASE_URL");
        return None;
    };
    let store = PgStateStore::connect(&url).await.expect("test database");
    store.migrate().await.expect("migrate");
    let mut plan = plan();
    plan.trade_id = format!(
        "admin-test-{}",
        Utc::now().timestamp_nanos_opt().expect("timestamp")
    );
    plan.rules[0].intent.trade_id = Some(plan.trade_id.clone());
    store
        .put_trade_plan(Some("expired"), &plan)
        .await
        .expect("register");
    store
        .put_plan_state(
            Some("expired"),
            &plan.trade_id,
            &PlanState::seed(Phase::AwaitEntry, Utc::now() + chrono::Duration::days(7)),
        )
        .await
        .expect("state");
    let body = json!({"trade_id":plan.trade_id,"account":"expired","instrument":"AUDUSD","attempt_no":1,"broker_order_id":format!("order-{}",plan.trade_id),"direction":"long","placed_at":Utc::now(),"shell_time":Utc::now(),"expires_at":Utc::now()+chrono::Duration::days(7)});
    sqlx::query(
        "INSERT INTO entry_attempt (account,trade_id,attempt_no,body) VALUES ('expired',$1,1,$2)",
    )
    .bind(&plan.trade_id)
    .bind(body)
    .execute(store.pool())
    .await
    .expect("attempt");
    Some((store, plan))
}

async fn cleanup(store: &PgStateStore, trade_id: &str) {
    for statement in [
        "DELETE FROM trade_plan WHERE trade_id = $1",
        "DELETE FROM plan_state WHERE trade_id = $1",
        "DELETE FROM entry_attempt WHERE trade_id = $1",
        "DELETE FROM archived_plan WHERE trade_id = $1",
    ] {
        sqlx::query(statement)
            .bind(trade_id)
            .execute(store.pool())
            .await
            .expect("cleanup");
    }
    sqlx::query("DELETE FROM cron_notes WHERE correlation_id = $1")
        .bind(trade_id)
        .execute(store.pool())
        .await
        .expect("notes cleanup");
}

#[tokio::test]
async fn switch_atomically_detaches_attempts_preserves_audit_and_uses_target_mapping() {
    let Some((store, before)) = fixture().await else {
        return;
    };
    let target = AccountMetadata::new(
        "reversals",
        trade_control_core::intent::BrokerKind::TradeNation,
        AccountKind::Demo,
    );
    let now = Utc::now();
    transition(
        store.pool(),
        &before.trade_id,
        Action::PlanPaper,
        Some(&target),
        now,
    )
    .await
    .expect("paper transition");
    assert!(
        store
            .get_trade_plan(Some("expired"), &before.trade_id)
            .await
            .expect("source")
            .is_none()
    );
    let next = store
        .get_trade_plan(Some("reversals"), &before.trade_id)
        .await
        .expect("target")
        .expect("target plan");
    assert!(next.shadow);
    assert_eq!(next.instrument, "AUD/USD");
    assert_eq!(
        store
            .get_plan_state(Some("reversals"), &before.trade_id)
            .await
            .expect("state")
            .expect("state exists")
            .watermark,
        Some(now)
    );
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM entry_attempt WHERE trade_id = $1")
        .bind(&before.trade_id)
        .fetch_one(store.pool())
        .await
        .expect("attempt count");
    assert_eq!(count, 0);
    let (body,): (Value,) = sqlx::query_as(
        "SELECT body FROM cron_notes WHERE correlation_id = $1 AND source = 'plan-admin'",
    )
    .bind(&before.trade_id)
    .fetch_one(store.pool())
    .await
    .expect("audit");
    assert_eq!(
        body["admin_snapshot"]["detached_attempts"]
            .as_array()
            .expect("snapshot")
            .len(),
        1
    );
    cleanup(&store, &before.trade_id).await;
}

#[tokio::test]
async fn manual_close_never_needs_a_working_broker_and_queues_audit() {
    let Some((store, before)) = fixture().await else {
        return;
    };
    let orphan = format!("orphan-{}", before.trade_id);
    let unrelated = format!("unrelated-{}", before.trade_id);
    for (id, account) in [(&orphan, "expired"), (&unrelated, "another-account")] {
        sqlx::query("INSERT INTO order_bodies (order_id,signed_body) VALUES ($1,$2)")
            .bind(id)
            .bind(format!(
                "trade_id: {}\naccount: {account}\naction: enter\n",
                before.trade_id
            ))
            .execute(store.pool())
            .await
            .expect("orphan signed body");
    }
    transition(
        store.pool(),
        &before.trade_id,
        Action::PlanClosedManually,
        None,
        Utc::now(),
    )
    .await
    .expect("manual close without credentials");
    assert!(
        store
            .get_trade_plan(Some("expired"), &before.trade_id)
            .await
            .expect("active")
            .is_none()
    );
    let archives = store.list_all_archived_plans().await.expect("archives");
    let archived = archives
        .iter()
        .find(|archived| archived.plan.trade_id == before.trade_id)
        .expect("archive");
    assert_eq!(archived.final_state.phase, Phase::Done);
    assert!(
        archived
            .final_state
            .fired
            .contains("operator:closed-manually")
    );
    let (body,): (Value,) = sqlx::query_as(
        "SELECT body FROM cron_notes WHERE correlation_id = $1 AND source = 'plan-admin'",
    )
    .bind(&before.trade_id)
    .fetch_one(store.pool())
    .await
    .expect("audit");
    assert_eq!(body["admin_snapshot"]["audit_status"], "pending");
    assert!(body.get("broker_exit").is_none());
    let remaining: Vec<(String,)> =
        sqlx::query_as("SELECT order_id FROM order_bodies WHERE order_id = $1 OR order_id = $2")
            .bind(&orphan)
            .bind(&unrelated)
            .fetch_all(store.pool())
            .await
            .expect("remaining bodies");
    assert_eq!(remaining, vec![(unrelated.clone(),)]);
    sqlx::query("DELETE FROM order_bodies WHERE order_id = $1")
        .bind(unrelated)
        .execute(store.pool())
        .await
        .expect("unrelated body cleanup");
    cleanup(&store, &before.trade_id).await;
}

#[tokio::test]
async fn unsafe_mapping_rolls_back_without_detaching_existing_exposure() {
    let Some((store, mut before)) = fixture().await else {
        return;
    };
    before.pip_size = 0.01;
    store
        .put_trade_plan(Some("expired"), &before)
        .await
        .expect("invalid pip fixture");
    let target = AccountMetadata::new(
        "reversals",
        trade_control_core::intent::BrokerKind::TradeNation,
        AccountKind::Demo,
    );
    assert!(
        transition(
            store.pool(),
            &before.trade_id,
            Action::PlanSwapAccount,
            Some(&target),
            Utc::now()
        )
        .await
        .is_err()
    );
    assert!(
        store
            .get_trade_plan(Some("expired"), &before.trade_id)
            .await
            .expect("source")
            .is_some()
    );
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM entry_attempt WHERE trade_id = $1")
        .bind(&before.trade_id)
        .fetch_one(store.pool())
        .await
        .expect("attempt count");
    assert_eq!(count, 1);
    cleanup(&store, &before.trade_id).await;
}
