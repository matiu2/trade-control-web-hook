//! A plan transition and its detached broker obligations share one transaction.

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, eyre};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use trade_control_core::{
    account::AccountMetadata,
    intent::Action,
    plan_state::{Phase, PlanState},
    recording::{CronNote, CronNoteSeverity},
    state::ArchivedPlan,
    trade_plan::TradePlan,
};

use super::mapping::{reroute, switch_state};

struct Source {
    account: Option<String>,
    plan: TradePlan,
    state: Option<PlanState>,
    archived: bool,
    settlement: Option<trade_control_core::settlement::Settlement>,
}

pub(super) async fn transition(
    pool: &PgPool,
    trade_id: &str,
    action: Action,
    target: Option<&AccountMetadata>,
    now: DateTime<Utc>,
) -> Result<String> {
    let mut transaction = pool.begin().await?;
    let db = transaction.as_mut();
    let source = load_source(db, trade_id, action).await?;
    let next = target
        .map(|account| reroute(&source.plan, account, action == Action::PlanPaper))
        .transpose()?;
    if let Some(target) = target {
        if source.account.as_deref() == Some(&target.name) {
            return Err(eyre!(
                "choose a different account; paper needs an independent market-data source"
            ));
        }
        let occupied: (bool,) = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM plan_state WHERE account = $1 AND trade_id = $2)",
        )
        .bind(&target.name)
        .bind(trade_id)
        .fetch_one(&mut *db)
        .await?;
        if occupied.0 {
            return Err(eyre!("target account already has state for this trade"));
        }
    }
    let message = describe(action, trade_id, &source, target);
    audit(db, action, &source, target, &message, now).await?;
    detach(db, source.account.as_deref(), trade_id).await?;
    if action == Action::PlanClosedManually {
        archive_manual(db, &source, now).await?;
    } else if let (Some(target), Some(next)) = (target, next.as_ref()) {
        move_plan(db, &source, next, target, now).await?;
    } else {
        return Err(eyre!("missing target account for plan transition"));
    }
    transaction.commit().await?;
    Ok(message)
}

async fn load_source(db: &mut PgConnection, trade_id: &str, action: Action) -> Result<Source> {
    let plans: Vec<(Option<String>, Value)> =
        sqlx::query_as("SELECT account, body FROM trade_plan WHERE trade_id = $1 FOR UPDATE")
            .bind(trade_id)
            .fetch_all(&mut *db)
            .await?;
    match plans.as_slice() {
        [(account, body)] => {
            let row: Option<(Value,)> = sqlx::query_as("SELECT body FROM plan_state WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2 FOR UPDATE")
                .bind(account).bind(trade_id).fetch_optional(&mut *db).await?;
            Ok(Source {
                account: account.clone(),
                plan: serde_json::from_value(body.clone())?,
                state: row
                    .map(|(value,)| serde_json::from_value(value))
                    .transpose()?,
                archived: false,
                settlement: None,
            })
        }
        [] if action == Action::PlanClosedManually => load_archived(db, trade_id).await,
        [] => Err(eyre!("no active plan named {trade_id}")),
        _ => Err(eyre!(
            "trade id is ambiguous across accounts; resolve duplicate registrations first"
        )),
    }
}

async fn load_archived(db: &mut PgConnection, trade_id: &str) -> Result<Source> {
    let plans: Vec<(Option<String>, Value)> =
        sqlx::query_as("SELECT account, body FROM archived_plan WHERE trade_id = $1 FOR UPDATE")
            .bind(trade_id)
            .fetch_all(db)
            .await?;
    let [(account, body)] = plans.as_slice() else {
        return Err(eyre!("no unique active or archived plan named {trade_id}"));
    };
    let archived: ArchivedPlan = serde_json::from_value(body.clone())?;
    Ok(Source {
        account: account.clone(),
        plan: archived.plan,
        state: Some(archived.final_state),
        archived: true,
        settlement: archived.settlement,
    })
}

fn describe(
    action: Action,
    trade_id: &str,
    source: &Source,
    target: Option<&AccountMetadata>,
) -> String {
    let old = source.account.as_deref().unwrap_or("<global>");
    match (action, target) {
        (Action::PlanClosedManually, _) => format!(
            "plan-closed-manually: {trade_id} account={old}; operator-confirmed close; local management retired; broker was not contacted"
        ),
        (Action::PlanPaper, Some(target)) => format!(
            "plan-paper: {trade_id}; observe-only; market data account={}; old account={old} exposure detached (not closed); no simulated fills/P&L; bars opened before this switch are skipped",
            target.name
        ),
        (_, Some(target)) => format!(
            "plan-swapped: {trade_id} {old} -> {}; old broker exposure detached (not closed or migrated); observe_only={}; only bars opened after this switch are evaluated",
            target.name, source.plan.shadow
        ),
        _ => format!("plan administration: {trade_id}"),
    }
}

async fn audit(
    db: &mut PgConnection,
    action: Action,
    source: &Source,
    target: Option<&AccountMetadata>,
    message: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    let attempts: Vec<(Value,)> = sqlx::query_as("SELECT body FROM entry_attempt WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2 FOR UPDATE")
        .bind(&source.account).bind(&source.plan.trade_id).fetch_all(&mut *db).await?;
    let held: Option<(Value,)> =
        sqlx::query_as("SELECT body FROM held_trade_record WHERE trade_id = $1 FOR UPDATE")
            .bind(&source.plan.trade_id)
            .fetch_optional(&mut *db)
            .await?;
    let order_bodies =
        matching_order_bodies(db, source.account.as_deref(), &source.plan.trade_id).await?;
    let note = CronNote {
        ts: now.to_rfc3339(),
        trade_id: source.plan.trade_id.clone(),
        account: source.account.clone(),
        source: "plan-admin".into(),
        severity: CronNoteSeverity::Info,
        message: message.into(),
        broker_exit: None,
        attempt_resolution: None,
    };
    let mut body = serde_json::to_value(&note)?;
    body["admin_snapshot"] = json!({ "action": action, "audit_status": if action == Action::PlanClosedManually { "pending" } else { "not_requested" }, "source_account": source.account, "target_account": target.map(|account| &account.name), "plan": source.plan, "state": source.state, "was_archived": source.archived, "detached_order_bodies": order_bodies, "detached_attempts": attempts.into_iter().map(|(value,)| value).collect::<Vec<_>>(), "detached_hold": held.map(|(value,)| value) });
    sqlx::query("INSERT INTO cron_notes (ts, correlation_id, account, source, severity, message, body) VALUES ($1, $2, $3, 'plan-admin', 'info', $4, $5)")
        .bind(now).bind(&source.plan.trade_id).bind(&source.account).bind(message).bind(body).execute(db).await?;
    Ok(())
}

fn owns_order_body(body: &str, account: Option<&str>, trade_id: &str) -> bool {
    let Ok(body) = serde_yaml::from_str::<serde_yaml::Value>(body) else {
        return false;
    };
    body.get("trade_id").and_then(serde_yaml::Value::as_str) == Some(trade_id)
        && body.get("account").and_then(serde_yaml::Value::as_str) == account
}

async fn matching_order_bodies(
    db: &mut PgConnection,
    account: Option<&str>,
    trade_id: &str,
) -> Result<Vec<(String, String)>> {
    let rows: Vec<(String, String, bool)> = sqlx::query_as("SELECT order_id, signed_body, EXISTS(SELECT 1 FROM entry_attempt WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2 AND body->>'broker_order_id' = order_bodies.order_id) FROM order_bodies FOR UPDATE")
        .bind(account).bind(trade_id).fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .filter(|(_, body, linked)| *linked || owns_order_body(body, account, trade_id))
        .map(|(id, body, _)| (id, body))
        .collect())
}

async fn detach(db: &mut PgConnection, account: Option<&str>, trade_id: &str) -> Result<()> {
    // Bodies survive normal attempt retirement, so also match their signed
    // ownership instead of relying only on the currently surviving attempts.
    let bodies = matching_order_bodies(db, account, trade_id).await?;
    for (order_id, _) in bodies {
        sqlx::query("DELETE FROM order_bodies WHERE order_id = $1")
            .bind(order_id)
            .execute(&mut *db)
            .await?;
    }
    sqlx::query(
        "DELETE FROM entry_attempt WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2",
    )
    .bind(account)
    .bind(trade_id)
    .execute(&mut *db)
    .await?;
    sqlx::query("DELETE FROM retry_fire WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2")
        .bind(account)
        .bind(trade_id)
        .execute(&mut *db)
        .await?;
    sqlx::query("DELETE FROM held_trade_record WHERE trade_id = $1")
        .bind(trade_id)
        .execute(db)
        .await?;
    Ok(())
}

async fn archive_manual(db: &mut PgConnection, source: &Source, now: DateTime<Utc>) -> Result<()> {
    let mut final_state = source
        .state
        .clone()
        .unwrap_or_else(|| PlanState::seed(Phase::Done, now));
    final_state.phase = Phase::Done;
    final_state.entries_blocked = true;
    final_state.fired.insert("operator:closed-manually".into());
    let archived = ArchivedPlan {
        account: source.account.clone(),
        plan: source.plan.clone(),
        final_state,
        archived_at: now,
        settlement: source.settlement.clone(),
    };
    sqlx::query(
        "DELETE FROM archived_plan WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2",
    )
    .bind(&source.account)
    .bind(&source.plan.trade_id)
    .execute(&mut *db)
    .await?;
    sqlx::query("INSERT INTO archived_plan (account, trade_id, body) VALUES ($1, $2, $3)")
        .bind(&source.account)
        .bind(&source.plan.trade_id)
        .bind(serde_json::to_value(archived)?)
        .execute(&mut *db)
        .await?;
    remove_live(db, source).await
}

async fn remove_live(db: &mut PgConnection, source: &Source) -> Result<()> {
    sqlx::query("DELETE FROM trade_plan WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2")
        .bind(&source.account)
        .bind(&source.plan.trade_id)
        .execute(&mut *db)
        .await?;
    sqlx::query("DELETE FROM plan_state WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2")
        .bind(&source.account)
        .bind(&source.plan.trade_id)
        .execute(db)
        .await?;
    Ok(())
}

async fn move_plan(
    db: &mut PgConnection,
    source: &Source,
    next: &TradePlan,
    target: &AccountMetadata,
    now: DateTime<Utc>,
) -> Result<()> {
    move_controls(db, source, next, target).await?;
    remove_live(db, source).await?;
    sqlx::query("INSERT INTO trade_plan (account, trade_id, body) VALUES ($1, $2, $3)")
        .bind(&target.name)
        .bind(&next.trade_id)
        .bind(serde_json::to_value(next)?)
        .execute(&mut *db)
        .await?;
    if let Some(state) = source.state.clone() {
        sqlx::query("INSERT INTO plan_state (account, trade_id, body) VALUES ($1, $2, $3)")
            .bind(&target.name)
            .bind(&next.trade_id)
            .bind(serde_json::to_value(switch_state(state, now))?)
            .execute(db)
            .await?;
    }
    Ok(())
}

async fn move_controls(
    db: &mut PgConnection,
    source: &Source,
    next: &TradePlan,
    target: &AccountMetadata,
) -> Result<()> {
    // Prep/prep-block keys are shared per instrument, so COPY them and retain
    // the originals for any other plan still using the old account.
    sqlx::query("INSERT INTO prep (account, instrument, step, set_at, setter_id, expires_at) SELECT $3, $4, step, set_at, setter_id, expires_at FROM prep WHERE account IS NOT DISTINCT FROM $1 AND instrument = $2 ON CONFLICT DO NOTHING")
        .bind(&source.account).bind(&source.plan.instrument).bind(&target.name).bind(&next.instrument).execute(&mut *db).await?;
    sqlx::query("INSERT INTO prep_block (account, instrument, step, set_at, expires_at) SELECT $3, $4, step, set_at, expires_at FROM prep_block WHERE account IS NOT DISTINCT FROM $1 AND instrument = $2 ON CONFLICT DO NOTHING")
        .bind(&source.account).bind(&source.plan.instrument).bind(&target.name).bind(&next.instrument).execute(&mut *db).await?;
    sqlx::query("UPDATE veto SET account = $3, instrument = $4 WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2")
        .bind(&source.account).bind(&next.trade_id).bind(&target.name).bind(&next.instrument).execute(&mut *db).await?;
    sqlx::query(
        "UPDATE mw_state SET account = $3 WHERE account IS NOT DISTINCT FROM $1 AND trade_id = $2",
    )
    .bind(&source.account)
    .bind(&next.trade_id)
    .bind(&target.name)
    .execute(db)
    .await?;
    Ok(())
}
