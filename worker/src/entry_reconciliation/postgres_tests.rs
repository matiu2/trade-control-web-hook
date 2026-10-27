//! Runs in a private schema, never against application tables or broker state.
use super::{persist, recover, tests::fixture};
use chrono::Duration;
use color_eyre::eyre::{Result, eyre};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::sync::atomic::{AtomicU64, Ordering};

static SCHEMA_SEQUENCE: AtomicU64 = AtomicU64::new(1);

async fn isolated_pool() -> Result<(PgPool, PgPool, String)> {
    let url = std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgresql://candle_cache:candle_cache@localhost:5432/trade_control_dev".into()
    });
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let schema = format!(
        "entry_reconcile_test_{}_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_micros(),
        SCHEMA_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    // Identifiers contain only our fixed prefix, process ID and timestamp.
    sqlx::QueryBuilder::new("CREATE SCHEMA ")
        .push(&schema)
        .build()
        .execute(&admin)
        .await?;
    let search_path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .after_connect(move |connection, _| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path',$1,false)")
                    .bind(search_path)
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await?;
    sqlx::raw_sql("CREATE TABLE entry_attempt(account text,trade_id text NOT NULL,attempt_no integer NOT NULL,body jsonb NOT NULL);
        CREATE UNIQUE INDEX entry_attempt_key ON entry_attempt(COALESCE(account,''),trade_id,attempt_no);
        CREATE TABLE seen(id text PRIMARY KEY,action text NOT NULL,seen_at timestamptz,outcome text NOT NULL,trade_id text,expires_at timestamptz NOT NULL);
        CREATE TABLE cron_notes(id bigserial PRIMARY KEY,ts timestamptz NOT NULL,correlation_id text NOT NULL,account text,source text NOT NULL,severity text NOT NULL,message text NOT NULL,body jsonb NOT NULL);")
        .execute(&pool).await?;
    Ok((admin, pool, schema))
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    let query = match table {
        "entry_attempt" => "SELECT COUNT(*) FROM entry_attempt",
        "cron_notes" => "SELECT COUNT(*) FROM cron_notes",
        "seen" => "SELECT COUNT(*) FROM seen",
        _ => panic!("unknown test table"),
    };
    sqlx::query_scalar(query).fetch_one(pool).await.unwrap()
}

#[tokio::test]
#[ignore = "requires local PostgreSQL; creates and removes a private test schema"]
async fn transaction_is_atomic_restart_safe_and_preserves_poison_and_mutable_state() {
    let (record, proof, now) = fixture();
    let adoption = recover(&[record], &[17; 32], "pos-eurnzd-example", proof, now).unwrap();
    let (admin, pool, schema) = isolated_pool().await.unwrap();

    // Rechecking fails after all SQL writes, reproducing a broker-state race.
    assert!(
        persist(&pool, &adoption, now, async {
            Err(eyre!("broker changed"))
        })
        .await
        .is_err()
    );
    assert_eq!(count(&pool, "entry_attempt").await, 0);
    assert_eq!(count(&pool, "cron_notes").await, 0);
    assert_eq!(count(&pool, "seen").await, 0);

    // Audit-write failure also rolls back the attempt and replay marker.
    sqlx::query("ALTER TABLE cron_notes ADD CONSTRAINT reject_note CHECK (severity <> 'info')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        persist(&pool, &adoption, now, async { Ok(()) })
            .await
            .is_err()
    );
    assert_eq!(count(&pool, "entry_attempt").await, 0);
    assert_eq!(count(&pool, "seen").await, 0);
    sqlx::query("ALTER TABLE cron_notes DROP CONSTRAINT reject_note")
        .execute(&pool)
        .await
        .unwrap();

    let poison_expiry = now + Duration::days(2);
    sqlx::query("INSERT INTO seen VALUES ($1,'enter',$2,'original ambiguous poison',$3,$4)")
        .bind(&adoption.intent_id)
        .bind(now)
        .bind(&adoption.attempt.trade_id)
        .bind(poison_expiry)
        .execute(&pool)
        .await
        .unwrap();
    let receipt = persist(&pool, &adoption, now, async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(receipt["committed"], json!(true));
    assert_eq!(count(&pool, "entry_attempt").await, 1);
    assert_eq!(count(&pool, "cron_notes").await, 1);
    let (outcome, expiry): (String, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT outcome,expires_at FROM seen")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(outcome, "original ambiguous poison");
    assert_eq!(expiry, poison_expiry);

    // Simulate a process dying after DB commit and before journal resolution,
    // followed by a management tick changing its mutable state.
    let mut evolved = adoption.attempt.clone();
    evolved.stop_loss_price = Some(1.9704);
    evolved.adverse_extreme = Some(1.9690);
    evolved.expires_at += Duration::days(3);
    sqlx::query("UPDATE entry_attempt SET body = $1")
        .bind(serde_json::to_value(&evolved).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    persist(&pool, &adoption, now + Duration::minutes(1), async {
        Ok(())
    })
    .await
    .unwrap();
    let stored: Value = sqlx::query_scalar("SELECT body FROM entry_attempt")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, serde_json::to_value(&evolved).unwrap());
    assert_eq!(count(&pool, "cron_notes").await, 1);

    // Another trade cannot steal the position, including across accounts.
    sqlx::query("UPDATE entry_attempt SET body = jsonb_set(body,'{trade_id}','\"other-trade\"')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        persist(&pool, &adoption, now, async { Ok(()) })
            .await
            .is_err()
    );
    assert_eq!(count(&pool, "entry_attempt").await, 1);
    pool.close().await;
    sqlx::QueryBuilder::new("DROP SCHEMA ")
        .push(&schema)
        .push(" CASCADE")
        .build()
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}

#[tokio::test]
#[ignore = "requires local PostgreSQL; creates and removes a private test schema"]
async fn already_closed_fill_persists_broker_exit_without_reopening() {
    let (record, mut proof, now) = fixture();
    proof.closed_exit = Some(
        serde_json::from_value(json!({
            "broker_trade_id":"position-1", "reason":"other", "closed_at":now,
            "transaction_id":"closing-deal", "exit_price":1.9705
        }))
        .unwrap(),
    );
    let adoption = recover(&[record], &[17; 32], "pos-eurnzd-example", proof, now).unwrap();
    let (admin, pool, schema) = isolated_pool().await.unwrap();
    persist(&pool, &adoption, now, async { Ok(()) })
        .await
        .unwrap();
    let note: Value = sqlx::query_scalar("SELECT body FROM cron_notes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        note["broker_exit"]["execution"]["broker_trade_id"],
        json!("position-1")
    );
    assert_eq!(
        note["broker_exit"]["execution"]["transaction_id"],
        json!("closing-deal")
    );
    pool.close().await;
    sqlx::QueryBuilder::new("DROP SCHEMA ")
        .push(&schema)
        .push(" CASCADE")
        .build()
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
