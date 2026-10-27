use super::Adoption;
use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, ensure};
use serde_json::{Value, json};
use trade_control_core::{
    broker_exit::BrokerExit,
    recording::{CronNote, CronNoteSeverity},
    state::EntryAttempt,
};

/// Atomically attach tracking, preserve replay poison and write audit evidence.
/// The caller holds the broker account's guard and refreshes it before this
/// transaction. Broker-journal resolution must happen only after this returns.
pub async fn persist(
    pool: &sqlx::PgPool,
    adoption: &Adoption,
    now: DateTime<Utc>,
    recheck: impl std::future::Future<Output = Result<()>>,
) -> Result<Value> {
    let mut tx = pool.begin().await?;
    // Existing state writers do not all share advisory locks. A short table
    // lock also excludes inserts while checking ownership across trade IDs.
    sqlx::query("LOCK TABLE entry_attempt IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let rows: Vec<(Value,)> = sqlx::query_as(
        "SELECT body FROM entry_attempt WHERE account IS NOT DISTINCT FROM $1 OR body->>'broker_order_id' = $2 OR body->>'broker_trade_id' = $3 FOR UPDATE")
        .bind(&adoption.proof.account).bind(&adoption.proof.order_id).bind(&adoption.proof.position_id)
        .fetch_all(&mut *tx).await?;
    let attempts = rows
        .into_iter()
        .map(|(body,)| serde_json::from_value(body))
        .collect::<Result<Vec<EntryAttempt>, _>>()?;
    let existing = existing_binding(&attempts, adoption)?;
    if existing.is_none() {
        sqlx::query(
            "INSERT INTO entry_attempt (account,trade_id,attempt_no,body) VALUES ($1,$2,$3,$4)",
        )
        .bind(&adoption.proof.account)
        .bind(&adoption.attempt.trade_id)
        .bind(adoption.attempt.attempt_no as i32)
        .bind(serde_json::to_value(&adoption.attempt)?)
        .execute(&mut *tx)
        .await?;
    }
    preserve_seen(&mut tx, adoption, now).await?;
    let mut receipt = json!({
        "version":1,"operation":"keep-protection-attach", "account":adoption.proof.account,
        "account_id":adoption.proof.account_id, "trade_id":adoption.attempt.trade_id,
        "audit_id":adoption.proof.audit_id,"request_id":adoption.request_id,"intent_id":adoption.intent_id,
        "order_id":adoption.proof.order_id,"position_id":adoption.proof.position_id,
        "stop_loss":adoption.proof.stop_loss,"take_profit":adoption.proof.take_profit,
        "entry_price":adoption.proof.entry_price,"units":adoption.proof.units,
        "closed_exit":adoption.proof.closed_exit,"broker_evidence":adoption.proof.evidence,
    });
    record_note(&mut tx, adoption, now, &receipt).await?;
    recheck.await?;
    tx.commit().await?;
    receipt["committed"] = json!(true);
    tracing::info!(account=%adoption.proof.account,trade_id=%adoption.attempt.trade_id,position_id=%adoption.proof.position_id,"broker fill tracking and reconciliation evidence committed");
    Ok(receipt)
}

fn existing_binding<'a>(
    attempts: &'a [EntryAttempt],
    adoption: &Adoption,
) -> Result<Option<&'a EntryAttempt>> {
    let expected = &adoption.attempt;
    let related = attempts
        .iter()
        .filter(|row| {
            (row.account == expected.account && row.trade_id == expected.trade_id)
                || row.broker_order_id == expected.broker_order_id
                || row.broker_trade_id == expected.broker_trade_id
        })
        .collect::<Vec<_>>();
    ensure!(
        related.len() <= 1,
        "multiple entry attempts conflict with this broker fill"
    );
    let existing = related.first().copied();
    if let Some(row) = existing {
        ensure!(
            row.account == expected.account
                && row.trade_id == expected.trade_id
                && row.attempt_no == expected.attempt_no
                && row.instrument == expected.instrument
                && row.broker_order_id == expected.broker_order_id
                && row.broker_trade_id == expected.broker_trade_id
                && row.direction == expected.direction
                && row.placed_at == expected.placed_at
                && row.shell_time == expected.shell_time
                && row.pip_size == expected.pip_size
                && row.blackout_close == expected.blackout_close
                && row.breakeven == expected.breakeven
                && row.order_control == expected.order_control
                && !row.superseded,
            "entry attempt is already bound to conflicting broker identity or management policy"
        );
    }
    Ok(existing)
}

async fn preserve_seen(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    adoption: &Adoption,
    now: DateTime<Utc>,
) -> Result<()> {
    let result = sqlx::query(
        "INSERT INTO seen (id,action,seen_at,outcome,trade_id,expires_at) VALUES ($1,'enter',$2,'reconciled broker fill; do not resend',$3,$4)
         ON CONFLICT (id) DO UPDATE SET expires_at = GREATEST(seen.expires_at,EXCLUDED.expires_at)
         WHERE seen.action = 'enter' AND seen.trade_id = EXCLUDED.trade_id")
        .bind(&adoption.intent_id).bind(now).bind(&adoption.attempt.trade_id).bind(adoption.attempt.expires_at)
        .execute(&mut **tx).await?;
    ensure!(
        result.rows_affected() == 1,
        "intent replay marker belongs to a different action or trade"
    );
    Ok(())
}

async fn record_note(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    adoption: &Adoption,
    now: DateTime<Utc>,
    receipt: &Value,
) -> Result<()> {
    let source = format!("entry-reconciliation:{}", adoption.proof.audit_id);
    let previous: Option<Value> = sqlx::query_scalar(
        "SELECT body FROM cron_notes WHERE account = $1 AND correlation_id = $2 AND source = $3 LIMIT 1")
        .bind(&adoption.proof.account).bind(&adoption.attempt.trade_id).bind(&source)
        .fetch_optional(&mut **tx).await?;
    if let Some(previous) = previous {
        for field in [
            "account",
            "account_id",
            "trade_id",
            "audit_id",
            "request_id",
            "intent_id",
            "order_id",
            "position_id",
        ] {
            ensure!(
                previous["entry_reconciliation"][field] == receipt[field],
                "existing reconciliation note has a different {field} binding"
            );
        }
        return Ok(());
    }
    let note = CronNote {
        ts: now.to_rfc3339(),
        trade_id: adoption.attempt.trade_id.clone(),
        account: adoption.attempt.account.clone(),
        source,
        severity: CronNoteSeverity::Info,
        message: format!("Accepted existing broker protection and attached tracking: {receipt}"),
        broker_exit: adoption
            .proof
            .closed_exit
            .clone()
            .map(|execution| BrokerExit {
                broker_order_id: adoption.proof.order_id.clone(),
                attempt_no: adoption.attempt.attempt_no,
                direction: adoption.proof.direction,
                pip_size: Some(adoption.proof.pip_size),
                execution,
            }),
        attempt_resolution: None,
    };
    let mut body = serde_json::to_value(&note)?;
    body["entry_reconciliation"] = receipt.clone();
    sqlx::query("INSERT INTO cron_notes (ts,correlation_id,account,source,severity,message,body) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(now).bind(&note.trade_id).bind(note.account.as_deref()).bind(&note.source)
        .bind(note.severity.to_string()).bind(&note.message).bind(body)
        .execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry_reconciliation::{recover, tests::fixture};

    #[test]
    fn retries_preserve_mutable_state_but_reject_another_position_owner() {
        let (record, proof, now) = fixture();
        let adoption = recover(&[record], &[17; 32], "pos-eurnzd-example", proof, now).unwrap();
        let mut existing = adoption.attempt.clone();
        existing.stop_loss_price = Some(1.9704);
        existing.expires_at += chrono::Duration::days(1);
        let rows = [existing.clone()];
        assert_eq!(existing_binding(&rows, &adoption).unwrap(), Some(&existing));
        existing.trade_id = "another-owner".into();
        assert!(existing_binding(&[existing], &adoption).is_err());
    }
}
