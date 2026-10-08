//! Read directions missing from older workers' plan summaries without blocking
//! the TUI. One thread fetches plans sequentially, posting each result as it lands.

use std::sync::mpsc::Sender;

use crate::cli;
use crate::jobs::{JobKind, JobOutcome, JobResult};
use crate::plan::parse_plan_export;

pub fn spawn(tx: Sender<JobResult>, trade_ids: Vec<String>) {
    if trade_ids.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for trade_id in trade_ids {
            let outcome = cli::plan_export_json(&trade_id)
                .and_then(|json| parse_plan_export(&json))
                .map(|detail| JobOutcome::Direction(detail.direction))
                .unwrap_or_else(|error| JobOutcome::Failed(error.to_string()));
            if tx
                .send(JobResult {
                    trade_id,
                    kind: JobKind::Direction,
                    outcome,
                })
                .is_err()
            {
                break;
            }
        }
    });
}
