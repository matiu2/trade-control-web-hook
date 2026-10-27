//! Paper entries resolve the same brackets as replay, without simulating fills.

use std::future::Future;

use chrono::{DateTime, Utc};
use trade_control_core::{
    intent::{Action, Resolved, ResolvedEntry, Shell},
    plan_eval::FiredIntent,
    recording::{CronNote, CronNoteSeverity},
    state::StoredPlan,
};

use crate::CronEnv;

/// A failed journal write leaves the watermark untouched so the next tick can
/// retry. Native storage deduplicates entries already written before a crash.
pub(super) async fn record_then_advance<C, F, Fut, T>(
    cron: &C,
    stored: &StoredPlan,
    fired: &[FiredIntent],
    now: DateTime<Utc>,
    advance: F,
) -> Result<T, String>
where
    C: CronEnv,
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    if stored.plan.shadow {
        for entry in fired.iter().filter(|f| f.intent.action == Action::Enter) {
            let note = entry_note(stored, entry, now);
            cron.record_paper_entry(&entry_identity(entry), note)
                .await
                .map_err(|error| format!("paper entry journal write failed: {error}"))?;
        }
    }
    Ok(advance().await)
}

fn entry_identity(fired: &FiredIntent) -> String {
    let rule_id = &fired.rule_id;
    let intent_id = &fired.intent.id;
    let bar = fired.candle.time.timestamp_millis();
    format!(
        "{}:{rule_id}{}:{intent_id}:{bar}",
        rule_id.len(),
        intent_id.len()
    )
}

fn entry_note(stored: &StoredPlan, fired: &FiredIntent, now: DateTime<Utc>) -> CronNote {
    let shell = match &fired.signal {
        Some(signal) => Shell::from_candle_and_signal(&fired.candle, signal),
        None => Shell::from_candle(&fired.candle),
    };
    let pip_size = fired.intent.pip_size.unwrap_or(stored.plan.pip_size);
    let tick_size = fired.intent.tick_size.unwrap_or(pip_size);
    let (severity, result) = match Resolved::from_intent(&fired.intent, &shell, pip_size, tick_size)
    {
        Ok(resolved) => (CronNoteSeverity::Info, resolved_message(&resolved)),
        Err(error) => (
            CronNoteSeverity::Warn,
            format!("entry declined at resolution: {error}"),
        ),
    };
    CronNote {
        ts: now.to_rfc3339(),
        trade_id: stored.plan.trade_id.clone(),
        account: stored.account.clone(),
        source: "paper-entry".into(),
        severity,
        message: format!(
            "PAPER ENTRY: {} {:?} {result}; rule={} intent={} signal_bar={}; signal only, no broker order, fill or P&L",
            stored.plan.instrument,
            stored.plan.granularity,
            fired.rule_id,
            fired.intent.id,
            fired.candle.time.to_rfc3339(),
        ),
        broker_exit: None,
        attempt_resolution: None,
    }
}

fn resolved_message(resolved: &Resolved) -> String {
    let kind = match resolved.entry {
        ResolvedEntry::Market { .. } => "market reference",
        ResolvedEntry::Stop { .. } => "stop trigger",
        ResolvedEntry::Limit { .. } => "limit trigger",
    };
    format!(
        "{:?} {kind}={} SL={} TP={} risk={:?}",
        resolved.direction,
        resolved.entry.reference_price(),
        resolved.stop_loss,
        resolved.take_profit,
        resolved.risk
    )
}

#[cfg(test)]
mod tests;
