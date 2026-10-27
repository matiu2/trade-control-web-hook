//! Operator actions for a plan and the account picker used by the journal.

use color_eyre::eyre::{Result, eyre};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    Paper { account: String },
    SwapAccount { account: String },
    ClosedManually,
}

impl PlanAction {
    pub fn title(&self) -> &'static str {
        match self {
            Self::Paper { .. } => "Paper mode — entries logged",
            Self::SwapAccount { .. } => "Swap account",
            Self::ClosedManually => "Record manual closure",
        }
    }

    pub fn explanation(&self, trade_id: &str) -> String {
        match self {
            Self::Paper { account } => format!(
                "Switch {trade_id} to paper mode using market data from {account}? Entries are logged without broker orders or simulated fills/P&L. Existing broker positions and orders are detached from management; close or cancel them separately if needed."
            ),
            Self::SwapAccount { account } => format!(
                "Move future execution of {trade_id} to {account}? The current broker / paper mode is preserved. Existing positions and orders stay at the old broker and are detached from management. They are not moved or closed."
            ),
            Self::ClosedManually => format!(
                "Confirm you already closed {trade_id} and cancelled its pending broker orders. Your assertion stops management immediately. Broker history is checked afterward when available; this is not broker-confirmed closure. No broker-close request is sent."
            ),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AccountChoice {
    pub name: String,
    pub broker: String,
    pub kind: String,
    #[serde(default)]
    pub capabilities: Option<AccountCapabilities>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AccountCapabilities {
    pub quotes: bool,
    pub execution: bool,
    pub history: bool,
    pub readiness: String,
    pub reason: String,
}

impl AccountChoice {
    pub fn unavailable_reason(&self, purpose: AccountPurpose) -> Option<&str> {
        match &self.capabilities {
            Some(caps)
                if !caps.history || (purpose == AccountPurpose::SwapAccount && !caps.execution) =>
            {
                Some(&caps.reason)
            }
            None if self.broker == "ctrader" => {
                Some("cTrader capability unknown; refresh account configuration")
            }
            _ => None,
        }
    }
}

pub fn parse_accounts(json: &str) -> Result<Vec<AccountChoice>> {
    let mut accounts: Vec<AccountChoice> =
        serde_json::from_str(json).map_err(|e| eyre!("parse account list: {e}"))?;
    accounts.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(accounts)
}

#[derive(Debug, Clone)]
pub struct AccountPicker {
    pub trade_id: String,
    pub purpose: AccountPurpose,
    pub accounts: Vec<AccountChoice>,
    pub selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountPurpose {
    Paper,
    SwapAccount,
}

impl AccountPurpose {
    pub fn title(self) -> &'static str {
        match self {
            Self::Paper => "Paper mode — choose market-data account",
            Self::SwapAccount => "Swap account",
        }
    }

    pub fn action(self, account: String) -> PlanAction {
        match self {
            Self::Paper => PlanAction::Paper { account },
            Self::SwapAccount => PlanAction::SwapAccount { account },
        }
    }
}

impl AccountPicker {
    pub fn move_selection(&mut self, delta: i32) {
        if !self.accounts.is_empty() {
            self.selected =
                (self.selected as i32 + delta).rem_euclid(self.accounts.len() as i32) as usize;
        }
    }

    pub fn current(&self) -> Option<&AccountChoice> {
        self.accounts.get(self.selected)
    }
}

#[derive(Debug, Clone)]
pub struct ActionConfirmation {
    pub trade_id: String,
    pub action: PlanAction,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_choices_are_loaded_from_metadata_and_sorted() {
        let choices = parse_accounts(
            r#"[{"name":"the5ers-competition","broker":"mt5","kind":"demo"},{"name":"reversals","broker":"tradenation","kind":"demo"}]"#,
        )
        .unwrap();
        assert_eq!(choices[0].name, "reversals");
        assert_eq!(choices[0].broker, "tradenation");
        let mut picker = AccountPicker {
            trade_id: "trade-1".into(),
            purpose: AccountPurpose::SwapAccount,
            accounts: choices,
            selected: 0,
        };
        picker.move_selection(-1);
        assert_eq!(picker.current().unwrap().name, "the5ers-competition");
    }

    #[test]
    fn account_switch_warning_explains_detached_exposure() {
        let text = PlanAction::SwapAccount {
            account: "reversals".into(),
        }
        .explanation("trade-1");
        assert!(text.contains("reversals"));
        assert!(text.contains("old broker"));
        assert!(text.contains("detached"));
    }

    #[test]
    fn manual_close_is_an_operator_assertion() {
        let text = PlanAction::ClosedManually.explanation("trade-1");
        assert!(text.contains("already closed"));
        assert!(text.contains("No broker-close request"));
    }

    #[test]
    fn quote_only_and_unknown_ctrader_are_unavailable_for_both_plan_actions() {
        let choices = parse_accounts(r#"[{"name":"fix","broker":"ctrader","kind":"demo","capabilities":{"quotes":true,"execution":false,"history":false,"readiness":"quotes-only","reason":"Open API needed"}},{"name":"unknown","broker":"ctrader","kind":"demo"}]"#).unwrap();
        for purpose in [AccountPurpose::Paper, AccountPurpose::SwapAccount] {
            assert!(
                choices
                    .iter()
                    .all(|account| account.unavailable_reason(purpose).is_some())
            );
        }
    }
}
