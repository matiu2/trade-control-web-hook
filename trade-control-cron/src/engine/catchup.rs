//! Report successfully consumed candle backlogs without changing evaluation.

use chrono::{DateTime, Utc};
use trade_control_core::{
    broker::Candle,
    recording::{CronNote, CronNoteSeverity},
    state::StoredPlan,
};

/// Called only after the evaluated state has been persisted. Count real bars,
/// rather than wall-clock intervals, so ordinary weekend gaps do not warn.
/// A terminal guard can stop evaluation before the end of the fetched backlog.
pub(super) fn observation(
    stored: &StoredPlan,
    since: DateTime<Utc>,
    fresh: &[Candle],
    through: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<CronNote> {
    if fresh.len() < 2 {
        return None;
    }
    let first = fresh.first()?;
    let through = through?;
    let evaluated = fresh.iter().filter(|c| c.time <= through).count();
    // Use the explicit open timestamp: H4/D1 session closes are not always a
    // fixed granularity duration later (DST can make a daily bar 23/25 hours).
    let oldest = first.time;
    let age = (now - oldest).num_seconds().max(0);
    let backlog = fresh.len();
    Some(CronNote {
        ts: now.to_rfc3339(),
        trade_id: stored.plan.trade_id.clone(),
        account: stored.account.clone(),
        source: "engine-catchup".into(),
        severity: CronNoteSeverity::Warn,
        message: format!(
            "{} {:?}: recovered backlog of {backlog} closed candles since watermark {since}; \
             oldest pending candle opened at {oldest} ({age}s ago); \
             evaluated {evaluated} through {through}; \
             this does not confirm an entry or successful dispatch",
            stored.plan.instrument, stored.plan.granularity
        ),
        broker_exit: None,
        attempt_resolution: None,
    })
}

#[cfg(test)]
mod tests;
