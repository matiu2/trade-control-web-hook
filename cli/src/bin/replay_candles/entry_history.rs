//! Replay history requirements for optional entry conditions.

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Context, Result, eyre};
use trade_control_core::trade_plan::TradePlan;

pub fn warmup_bars(plan: &TradePlan, start: DateTime<Utc>, requested: usize) -> Result<usize> {
    let floor = requested.max(plan.detector_history_bars());
    let Some(line) = &plan.entry_conditions.retrace else {
        return Ok(floor);
    };
    let anchor = line.a.at_epoch.min(line.b.at_epoch);
    let anchor = DateTime::from_timestamp(anchor, 0)
        .ok_or_else(|| eyre!("retracement anchor timestamp is out of range"))?;
    // Include the earliest anchor plus a bar of slack, just as live does.
    // Counting wall-time bars here is deliberately conservative across gaps:
    // pull_with_warmup subsequently ensures that many actual candles exist.
    let span = (start - anchor).num_seconds().max(0);
    let bars = span / plan.granularity.seconds() + 1;
    let bars = usize::try_from(bars).wrap_err("retracement lookback is too large")?;
    Ok(floor.max(bars))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Duration};
    use trade_control_core::broker::Candle;
    use trade_control_engine::{Phase, PlanState, evaluate_plan};

    fn ts(value: &str) -> DateTime<Utc> {
        value.parse().unwrap()
    }

    fn plan() -> TradePlan {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
        serde_json::from_str(
            r#"{
            "trade_id":"old-retrace", "instrument":"EUR_USD", "direction":"short",
            "granularity":"h1", "pip_size":0.0001, "rules":[],
            "entry_conditions":{"retrace":{
                "a":{"at_epoch":1780646400,"price":1.30},
                "b":{"at_epoch":1780653600,"price":1.298}
            }}
        }"#,
        )
        .unwrap()
    }

    fn retraced(plan: &TradePlan, window: &[Candle]) -> Option<DateTime<Utc>> {
        let bar = window.last().unwrap();
        let state = PlanState::seed(Phase::AwaitEntry, bar.time + Duration::days(1));
        evaluate_plan(
            plan,
            &state,
            &[*bar],
            window,
            bar.time + Duration::hours(1),
            bar.time + Duration::days(1),
        )
        .new_state
        .retrace_seen_at
    }

    #[test]
    fn replay_fetches_old_retrace_anchors_across_weekends_for_live_parity() {
        let p = plan();
        let start = ts("2026-06-15T12:00:00Z");
        let from = ts("2026-06-01T00:00:00Z");
        let window: Vec<_> = (0..=(start - from).num_hours())
            .map(|hour| from + Duration::hours(hour))
            .filter(|time| time.weekday().num_days_from_monday() < 5)
            .map(|time| Candle {
                time,
                o: 1.12,
                h: 1.12,
                l: 1.12,
                c: 1.12,
            })
            .collect();
        let requested = warmup_bars(&p, start, 200).unwrap();
        let since = start - Duration::hours(requested as i64);
        let fetched: Vec<_> = window
            .iter()
            .filter(|bar| bar.time >= since)
            .copied()
            .collect();
        assert_eq!(
            retraced(&p, &window),
            None,
            "full live history does not satisfy the line"
        );
        assert_eq!(
            retraced(&p, &fetched),
            retraced(&p, &window),
            "replay must count the same traded bars as live, not extrapolate omitted anchors"
        );
        assert!(since.timestamp() < p.entry_conditions.retrace.unwrap().a.at_epoch);
    }
}
