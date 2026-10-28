//! Keep timeline inspection from recursively recording its own response.

use super::{Action, ControlResult, DispatchOutcome, control_to_parts};

impl DispatchOutcome {
    pub(super) fn from_control(result: ControlResult, action: Action) -> Self {
        let (status, body) = control_to_parts(result);
        // The response already contains older request records. Persisting it
        // verbatim would embed every earlier read in the next timeline read.
        let record = if matches!(action, Action::PlanTimeline) && status.is_success() {
            "plan-timeline".to_string()
        } else {
            body.clone()
        };
        Self {
            status,
            body,
            record,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Action, ControlResult, DispatchOutcome, StatusCode};
    use tracing_subscriber::prelude::*;

    fn init_tracing() {
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    }

    #[test]
    fn successful_timeline_read_records_a_summary_and_returns_full_evidence() {
        init_tracing();
        let body = format!(
            "records:\n{}",
            "- outcome: previous timeline response\n".repeat(1000)
        );
        let outcome =
            DispatchOutcome::from_control(ControlResult::ok(body.clone()), Action::PlanTimeline);

        assert_eq!(outcome.status, StatusCode::OK);
        assert_eq!(outcome.body, body);
        assert_eq!(outcome.record, "plan-timeline");
    }

    #[test]
    fn failed_timeline_read_keeps_the_failure_reason() {
        init_tracing();
        let outcome = DispatchOutcome::from_control(
            ControlResult::error("no recorded events for trade_id missing", 404),
            Action::PlanTimeline,
        );

        assert_eq!(outcome.status, StatusCode::NOT_FOUND);
        assert_eq!(outcome.record, "no recorded events for trade_id missing");
        assert_eq!(outcome.body, outcome.record);
    }

    #[test]
    fn other_control_actions_keep_their_recorded_response() {
        init_tracing();
        let outcome = DispatchOutcome::from_control(
            ControlResult::ok("stored plan evidence"),
            Action::PlanShow,
        );

        assert_eq!(outcome.status, StatusCode::OK);
        assert_eq!(outcome.body, "stored plan evidence");
        assert_eq!(outcome.record, outcome.body);
    }
}
