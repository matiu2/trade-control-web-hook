use super::*;
use chrono::Duration;
use serde_json::json;
use tracing_subscriber::prelude::*;
use trade_control_core::position_tracking::PositionStatus;

fn tracked() -> TrackedPosition {
    serde_json::from_value(json!({
        "link":{"account":"demo","trade_id":"ihs-usdjpy-3e9eb418","instrument":"USDJPY","direction":"long","broker_order_id":"17074689","broker_position_id":"17074690"},
        "snapshot":{"position":{"instrument":"USDJPY","direction":"long","stop_loss":156.705,"take_profit":158.076,"position_id":"17074690","order_id":"17074689","stake":0.65,"entry_price":156.952,"opened_at":"2026-10-26T05:00:00Z"},"unrealized_pl":300.0,"currency":"USD"}
    })).unwrap()
}

#[tokio::test]
async fn durable_snapshots_survive_outages_missing_positions_and_retry_expiry() -> Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .ok();
    let url = std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgresql://candle_cache:candle_cache@localhost:5432/trade_control_dev".into()
    });
    let admin = sqlx::PgPool::connect(&url).await?;
    let schema = format!(
        "position_test_{}_{}",
        std::process::id(),
        Utc::now().timestamp_micros()
    );
    sqlx::QueryBuilder::new("CREATE SCHEMA ")
        .push(&schema)
        .build()
        .execute(&admin)
        .await?;
    let path = schema.clone();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |connection, _| {
            let path = path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path',$1,false)")
                    .bind(path)
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await?;
    let result = exercise(&pool).await;
    pool.close().await;
    sqlx::QueryBuilder::new("DROP SCHEMA ")
        .push(&schema)
        .push(" CASCADE")
        .build()
        .execute(&admin)
        .await?;
    admin.close().await;
    result
}

async fn exercise(pool: &sqlx::PgPool) -> Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    let mut link = tracked().link;
    link.broker_position_id = None;
    crate::position_links::record(pool, &link).await?;
    crate::position_links::record(pool, &link).await?;
    assert_eq!(
        candidate_links(pool).await?.len(),
        1,
        "a single-shot placement is observed without retry rows or position snapshots"
    );
    let mut conflicting = link;
    conflicting.trade_id = "different-owner".into();
    assert!(
        crate::position_links::record(pool, &conflicting)
            .await
            .is_err()
    );
    // Postgres timestamps have microsecond precision.
    let now = DateTime::from_timestamp_micros(Utc::now().timestamp_micros()).unwrap();
    let trade = "ihs-usdjpy-3e9eb418";
    let known = serde_json::json!({"account":"demo","trade_id":trade,"attempt_no":99,"instrument":"USDJPY","direction":"long","broker_order_id":"17074689","broker_trade_id":"17074690","placed_at":now,"shell_time":now,"expires_at":now});
    sqlx::query(
        "INSERT INTO entry_attempt(account,trade_id,attempt_no,body) VALUES ('demo',$1,99,$2)",
    )
    .bind(trade)
    .bind(known)
    .execute(pool)
    .await?;
    assert_eq!(
        candidate_links(pool).await?[0]
            .broker_position_id
            .as_deref(),
        Some("17074690"),
        "proven retry fill identity must upgrade a permanent placement with no position ID"
    );
    sqlx::query("DELETE FROM entry_attempt WHERE attempt_no=99")
        .execute(pool)
        .await?;
    record(pool, Some("demo"), now, &[tracked()], &[], &[]).await?;
    failure(
        pool,
        Some("demo"),
        now + Duration::seconds(15),
        "broker disconnected",
    )
    .await?;
    let positions = for_trade(pool, trade).await?;
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].status, PositionStatus::Open);
    assert_eq!(positions[0].observed_at, now);
    assert_eq!(
        positions[0].snapshot.as_ref().unwrap().unrealized_pl,
        Some(300.0)
    );
    assert_eq!(
        sync_for_trade(pool, trade).await?[0].error.as_deref(),
        Some("broker disconnected")
    );
    assert_eq!(
        active_links(pool).await?.len(),
        1,
        "no retry or plan row is needed to keep tracking"
    );
    record(
        pool,
        Some("demo"),
        now + Duration::seconds(30),
        &[],
        &[],
        &[],
    )
    .await?;
    assert_eq!(
        for_trade(pool, trade).await?[0].status,
        PositionStatus::Unconfirmed
    );
    assert!(sync_for_trade(pool, trade).await?[0].error.is_none());
    record(
        pool,
        Some("demo"),
        now + Duration::seconds(45),
        &[tracked()],
        &[],
        &[],
    )
    .await?;
    assert_eq!(
        for_trade(pool, trade).await?[0].status,
        PositionStatus::Open
    );
    let mut wrong = tracked();
    wrong.link.trade_id = "wrong-owner".into();
    assert!(
        record(
            pool,
            Some("demo"),
            now + Duration::seconds(60),
            &[wrong],
            &[],
            &[]
        )
        .await
        .is_err()
    );
    assert_eq!(
        for_trade(pool, trade).await?[0].status,
        PositionStatus::Open,
        "ownership failure rolls back missing flags too"
    );
    let mut another = tracked();
    another.link.account = Some("other-account".into());
    record(pool, Some("other-account"), now, &[another], &[], &[]).await?;
    assert_eq!(
        for_trade(pool, trade).await?.len(),
        2,
        "same broker ID on another account is independent"
    );
    let row = serde_json::json!([{"trade_id":trade,"account":"demo"}]);
    let list: Vec<Value> =
        serde_yaml::from_str(&enrich_list(pool, &serde_yaml::to_string(&row)?).await?)?;
    assert_eq!(list[0]["positions"].as_array().unwrap().len(), 1);
    let exit = serde_json::from_value(
        json!({"broker_trade_id":"17074690","closed_at":"2026-10-27T03:00:00Z","reason":"stop_loss","realized_pl":250.0,"account_currency":"USD"}),
    )?;
    record(
        pool,
        Some("demo"),
        now + Duration::seconds(75),
        &[],
        &[(tracked().link, exit)],
        &[],
    )
    .await?;
    let closed = for_trade(pool, trade).await?;
    let closed = closed
        .iter()
        .find(|p| p.link.account.as_deref() == Some("demo"))
        .unwrap();
    assert_eq!(closed.status, PositionStatus::Closed);
    assert_eq!(closed.exit.as_ref().unwrap().realized_pl, Some(250.0));
    assert_eq!(
        active_links(pool).await?.len(),
        1,
        "only the other account remains open"
    );
    assert!(
        candidate_links(pool)
            .await?
            .iter()
            .all(|link| !(link.account.as_deref() == Some("demo")
                && link.broker_order_id == "17074689")),
        "closed permanent placement links must not resume polling"
    );
    // A complete fill+close can arrive without a prior open snapshot.
    let mut rapid = tracked().link;
    rapid.broker_position_id = Some("rapid".into());
    rapid.broker_order_id = "rapid-order".into();
    let exit = serde_json::from_value(
        json!({"broker_trade_id":"rapid","reason":"unknown","realized_pl":42.0}),
    )?;
    record(
        pool,
        Some("demo"),
        now + Duration::seconds(90),
        &[],
        &[(rapid.clone(), exit)],
        &["partial history failure".into()],
    )
    .await?;
    let rapid_row = for_trade(pool, trade)
        .await?
        .into_iter()
        .find(|p| p.link.broker_order_id == "rapid-order")
        .unwrap();
    assert!(rapid_row.snapshot.is_none());
    assert_eq!(rapid_row.status, PositionStatus::Closed);
    let attempt = json!({"account":"demo","trade_id":trade,"attempt_no":1,"instrument":"USDJPY","direction":"long","broker_order_id":"rapid-order","broker_trade_id":"rapid","placed_at":now,"shell_time":now,"expires_at":now});
    sqlx::query(
        "INSERT INTO entry_attempt(account,trade_id,attempt_no,body) VALUES ('demo',$1,1,$2)",
    )
    .bind(trade)
    .bind(attempt)
    .execute(pool)
    .await?;
    assert!(
        candidate_links(pool)
            .await?
            .iter()
            .all(|link| link.broker_order_id != "rapid-order"),
        "retained filled retry rows must not reopen closed polling"
    );
    assert!(
        sync_for_trade(pool, trade)
            .await?
            .iter()
            .any(|sync| sync.error.as_deref() == Some("partial history failure"))
    );
    // The ordinary reconcile pass can win the race to observe a quick close.
    let proof = json!({"broker_order_id":"note-order","attempt_no":2,"direction":"long","execution":{"broker_trade_id":"note-position","reason":"unknown","realized_pl":123.0}});
    sqlx::query("INSERT INTO cron_notes(ts,correlation_id,account,source,severity,message,body) VALUES ($1,$2,'demo','broker-exit','info','closed',$3)")
        .bind(now).bind(trade).bind(json!({"broker_exit":proof})).execute(pool).await?;
    let note_position = for_trade(pool, trade)
        .await?
        .into_iter()
        .find(|p| p.link.broker_order_id == "note-order")
        .unwrap();
    assert_eq!(note_position.status, PositionStatus::Closed);
    assert!(note_position.snapshot.is_none());
    assert_eq!(note_position.exit.unwrap().realized_pl, Some(123.0));
    let mut cancelled = tracked().link;
    cancelled.broker_order_id = "cancelled-order".into();
    cancelled.broker_position_id = None;
    crate::position_links::record(pool, &cancelled).await?;
    assert!(
        candidate_links(pool)
            .await?
            .iter()
            .any(|link| link.broker_order_id == "cancelled-order")
    );
    crate::position_links::finish_cancelled(pool, &[cancelled]).await?;
    assert!(
        candidate_links(pool)
            .await?
            .iter()
            .all(|link| link.broker_order_id != "cancelled-order"),
        "unfilled cancellations must not be polled forever"
    );
    let mut links = vec![tracked().link];
    let mut conflict = tracked().link;
    conflict.broker_position_id = Some("conflicting-position".into());
    assert!(append_link(&mut links, conflict).is_err());
    Ok(())
}
