//! Explicit operator actions share the journal's configured destination and key.
use chrono::Utc;
use color_eyre::eyre::{Result, eyre};
use trade_control_cli::build_plan_admin_intent;
use trade_control_core::intent::Action;

use super::{JournalClient, request_suffix, validate_trade_id};
use crate::plan_admin::PlanAction;

impl JournalClient {
    pub fn administer(&self, trade_id: &str, action: &PlanAction) -> Result<String> {
        validate_trade_id(trade_id)?;
        let (kind, account, prefix) = match action {
            PlanAction::Paper { account } => {
                (Action::PlanPaper, Some(account.clone()), "plan-paper")
            }
            PlanAction::SwapAccount { account } => (
                Action::PlanSwapAccount,
                Some(account.clone()),
                "plan-swapped",
            ),
            PlanAction::ClosedManually => {
                (Action::PlanClosedManually, None, "plan-closed-manually")
            }
        };
        if account.as_ref().is_some_and(|name| name.trim().is_empty()) {
            return Err(eyre!("choose a target account"));
        }
        let intent = build_plan_admin_intent(
            kind,
            trade_id,
            account,
            true,
            Utc::now(),
            &request_suffix()?,
        );
        let response = self.post(&intent)?;
        let identity = format!("{prefix}: {trade_id}");
        if !response
            .strip_prefix(&identity)
            .is_some_and(|tail| tail.starts_with(';') || tail.starts_with(' '))
        {
            return Err(eyre!(
                "invalid plan administration acknowledgement: {response}"
            ));
        }
        Ok(response)
    }
}
