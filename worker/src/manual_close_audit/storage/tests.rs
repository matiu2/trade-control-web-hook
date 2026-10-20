use super::*;
use trade_control_core::settlement::{SettledTrade, Settlement};

#[test]
fn unfinished_audits_run_before_previously_checked_pending_notes() {
    let mut jobs = [
        Job {
            id: 1,
            account: None,
            trade_id: "checked".into(),
            body: json!({"admin_snapshot":{"audit_checked_at":"2026-10-20T02:00:00Z"}}),
        },
        Job {
            id: 2,
            account: None,
            trade_id: "not-yet-checked".into(),
            body: json!({"admin_snapshot":{}}),
        },
        Job {
            id: 3,
            account: None,
            trade_id: "older".into(),
            body: json!({"admin_snapshot":{"audit_checked_at":"2026-10-20T01:00:00Z"}}),
        },
    ];
    oldest_first(&mut jobs);
    assert_eq!(jobs.iter().map(|job| job.id).collect::<Vec<_>>(), [2, 3, 1]);
}

#[tokio::test]
async fn audit_cooldown_failures_and_confirmation_preserve_operator_closure() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .ok();
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        return;
    };
    let store = crate::PgStateStore::connect(&url).await.unwrap();
    store.migrate().await.unwrap();
    let trade_id = format!("audit-test-{}", Utc::now().timestamp_nanos_opt().unwrap());
    let account = format!("account-{trade_id}");
    let note = CronNote {
        ts: Utc::now().to_rfc3339(),
        trade_id: trade_id.clone(),
        account: Some(account.clone()),
        source: "plan-admin".into(),
        severity: CronNoteSeverity::Info,
        message: "operator-confirmed close; management retired".into(),
        broker_exit: None,
        attempt_resolution: None,
    };
    let mut body = serde_json::to_value(note).unwrap();
    body["admin_snapshot"] = json!({"audit_status":"pending", "detached_attempts":[]});
    sqlx::query("INSERT INTO archived_plan (account, trade_id, body) VALUES ($1,$2,$3)")
        .bind(&account)
        .bind(&trade_id)
        .bind(json!({"final_state":{"fired":["operator:closed-manually"]}, "settlement":{"broker":"previous", "trades":[{"realized_pl":42.0}]}}))
        .execute(store.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO cron_notes (ts,correlation_id,account,source,severity,message,body) VALUES (now(),$1,$2,'plan-admin','info','operator-confirmed close; management retired',$3)")
        .bind(&trade_id).bind(&account).bind(body).execute(store.pool()).await.unwrap();
    let jobs = claim(store.pool()).await.unwrap();
    let job = jobs.iter().find(|job| job.trade_id == trade_id).unwrap();
    finish(
        store.pool(),
        job,
        Audit::Pending("expired credentials".into()),
    )
    .await
    .unwrap();
    assert!(
        claim(store.pool())
            .await
            .unwrap()
            .iter()
            .all(|job| job.trade_id != trade_id)
    );
    let body: Value = sqlx::query_scalar("SELECT body FROM cron_notes WHERE correlation_id=$1")
        .bind(&trade_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(body["admin_snapshot"]["audit_status"], "pending");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("operator-confirmed close")
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("expired credentials")
    );
    finish(
        store.pool(),
        job,
        Audit::Observed(Settlement {
            broker: "mt5".into(),
            fetched_at: Utc::now(),
            trades: vec![],
            ledger: vec![],
            warnings: vec![],
        }),
    )
    .await
    .unwrap();
    let previous: Value =
        sqlx::query_scalar("SELECT body->'settlement' FROM archived_plan WHERE trade_id = $1")
            .bind(&trade_id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(previous["trades"][0]["realized_pl"], 42.0);
    let settlement = Settlement {
        broker: "mt5".into(),
        fetched_at: Utc::now(),
        ledger: vec![],
        warnings: vec![],
        trades: vec![SettledTrade {
            broker_order_id: Some("order-1".into()),
            broker_trade_id: "position-1".into(),
            instrument: Some("AUDUSD".into()),
            entry_price: None,
            exit_price: Some(0.7),
            size: None,
            opened_at: None,
            closed_at: Some(Utc::now()),
            realized_pl: None,
            financing: None,
            currency: None,
        }],
    };
    finish(store.pool(), job, Audit::Observed(settlement.clone()))
        .await
        .unwrap();
    finish(store.pool(), job, Audit::Observed(settlement))
        .await
        .unwrap();
    let notes = crate::recording_pg::cron_notes_for_trade(store.pool(), &trade_id)
        .await
        .unwrap();
    assert_eq!(notes.len(), 2);
    let audit = notes
        .iter()
        .find(|note| note.source == "manual-close-audit")
        .unwrap();
    assert!(audit.message.contains("1 closed and 0 open"));
    assert!(audit.broker_exit.is_none());
    let (status,): (String,) = sqlx::query_as("SELECT body->'admin_snapshot'->>'audit_status' FROM cron_notes WHERE correlation_id=$1 AND source='plan-admin'")
        .bind(&trade_id).fetch_one(store.pool()).await.unwrap();
    assert_eq!(status, "confirmed");
    let archived: Value = sqlx::query_scalar("SELECT body FROM archived_plan WHERE trade_id = $1")
        .bind(&trade_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(
        archived["settlement"]["trades"][0]["broker_trade_id"],
        "position-1"
    );
    assert!(archived["settlement"]["trades"][0]["realized_pl"].is_null());
    sqlx::query("DELETE FROM archived_plan WHERE trade_id = $1")
        .bind(&trade_id)
        .execute(store.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM cron_notes WHERE correlation_id=$1")
        .bind(trade_id)
        .execute(store.pool())
        .await
        .unwrap();
}
