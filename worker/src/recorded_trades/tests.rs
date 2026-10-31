use super::*;
use serde_json::json;
use tracing_subscriber::prelude::*;

fn init() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

fn record() -> RequestRecord {
    init();
    serde_json::from_value(json!({
        "ts":"2026-10-30T03:34:06Z", "request_id":"original", "method":"POST", "path":"/",
        "headers":[], "body":"v: 1\nid: pos-eur-gbp-test-enter\nnot_after: '2026-11-04T22:00:00Z'\naction: enter\ninstrument: EUR/GBP\ndirection: short\nentry: {type: limit, from: close, offset_pips: 0.0, at: 0.83568}\nstop_loss: {absolute: 0.8365781160079634}\ntake_profit: {absolute: 0.8331562846515476}\nbroker: tradenation\naccount: tradenation-live\ntrade_id: pos-eur-gbp-test\n",
        "trade_id":"pos-eur-gbp-test", "intent_id":"pos-eur-gbp-test-enter",
        "status":200, "outcome":"placed: order=96566199 size=0.044223 @ 0.83568", "logs":[]
    })).unwrap()
}

#[test]
fn accepted_direct_order_keeps_original_identity_prices_and_time() -> Result<()> {
    let trade = from_record(&record())?.unwrap();
    assert_eq!(trade["plan"]["trade_id"], "pos-eur-gbp-test");
    assert_eq!(trade["account"], "tradenation-live");
    assert_eq!(trade["plan"]["direction"], "short");
    assert_eq!(trade["plan"]["armed_at"], "2026-10-30T03:34:06Z");
    assert_eq!(trade["plan"]["rules"][0]["intent"]["entry"]["at"], 0.83568);
    assert_eq!(
        trade["plan"]["rules"][0]["intent"]["stop_loss"]["absolute"],
        0.8365781160079634
    );
    assert_eq!(trade["recorded_only"], true);
    assert_eq!(trade["plan"]["granularity"], "");
    Ok(())
}

#[test]
fn refusals_dry_runs_and_duplicate_responses_are_not_trades() -> Result<()> {
    for (status, outcome) in [
        (422, "rejected"),
        (200, "dry-run: id=test"),
        (200, "duplicate"),
        (500, "entered: order=unknown"),
    ] {
        let mut request = record();
        request.status = status;
        request.outcome = outcome.into();
        assert!(from_record(&request)?.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn historical_orders_are_recovered_once_without_creating_managed_plans() -> Result<()> {
    init();
    let database = std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgresql://candle_cache:candle_cache@localhost:5432/trade_control_dev".into()
    });
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database)
        .await?;
    sqlx::raw_sql("CREATE TEMP TABLE request_records(id bigserial,ts timestamptz,request_id text,intent_id text,trade_id text,status int,outcome text,body jsonb);
        CREATE TEMP TABLE trade_plan(trade_id text,account text);
        CREATE TEMP TABLE archived_plan(trade_id text,account text);").execute(&pool).await?;
    let request = record();
    crate::recording_pg::record_request(&pool, &request).await?;
    crate::recording_pg::record_request(&pool, &request).await?;
    let entries = list(&pool).await?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["trade_id"], "pos-eur-gbp-test");
    assert_eq!(
        show(&pool, "pos-eur-gbp-test").await?.unwrap()["recorded_only"],
        true
    );
    assert!(show(&pool, "missing").await?.is_none());
    let response = crate::http::recorded::show(
        &pool,
        trade_control_core::dispatch::ControlResult::error("no managed plan", 404),
        Some("pos-eur-gbp-test"),
    )
    .await;
    assert_eq!(response.status, 200);
    let details: Vec<Value> = serde_yaml::from_str(&response.body)?;
    assert_eq!(
        details[0]["plan"]["trade_id"], "pos-eur-gbp-test",
        "terminal plan-export and web journal must share the standard detail envelope"
    );
    let active_only = crate::http::recorded::list(
        &pool,
        trade_control_core::dispatch::ControlResult::ok("[]"),
        false,
    )
    .await;
    let active_rows: Vec<Value> = serde_yaml::from_str(&active_only.body)?;
    assert!(
        active_rows.is_empty(),
        "auto-replace must enumerate managed plans only"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM trade_plan")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 0, "journal recovery must never arm another order");
    sqlx::query("INSERT INTO archived_plan VALUES ('pos-eur-gbp-test','tradenation-live')")
        .execute(&pool)
        .await?;
    assert!(
        list(&pool).await?.is_empty(),
        "an existing archive owns its journal row"
    );
    pool.close().await;
    Ok(())
}
