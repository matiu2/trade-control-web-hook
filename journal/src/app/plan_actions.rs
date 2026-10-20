//! Confirmed operator actions and account selection, isolated from chart jobs.

use super::{App, Status};
use crate::jobs::{self, JobKind};
use crate::plan::PlanRow;
use crate::plan_admin::{
    AccountChoice, AccountPicker, AccountPurpose, ActionConfirmation, PlanAction,
};
use crate::screen::Screen;

impl App {
    pub fn request_plan_action(&mut self, action: PlanAction) {
        let Some(trade_id) = self.management_trade(action == PlanAction::ClosedManually) else {
            return;
        };
        self.action_confirm = Some(ActionConfirmation { trade_id, action });
    }

    pub fn request_swap_account(&mut self) {
        self.request_account_picker(AccountPurpose::SwapAccount);
    }

    pub fn request_paper_mode(&mut self) {
        self.request_account_picker(AccountPurpose::Paper);
    }

    fn request_account_picker(&mut self, purpose: AccountPurpose) {
        let Some(trade_id) = self.management_trade(false) else {
            return;
        };
        if self.mark_in_flight(&trade_id, JobKind::Accounts) {
            self.account_picker = Some(AccountPicker {
                trade_id: trade_id.clone(),
                purpose,
                accounts: Vec::new(),
                selected: 0,
            });
            jobs::spawn_accounts(self.job_tx.clone(), trade_id);
        }
    }

    fn management_trade(&mut self, allow_archived: bool) -> Option<String> {
        if self.screen == Screen::List {
            return None;
        }
        let plan = self.current_plan()?;
        if plan.is_archived() && !allow_archived {
            self.status = Status::error("this plan is already archived");
            return None;
        }
        let trade_id = plan.trade_id.clone();
        if [JobKind::PlanAction, JobKind::Timeline, JobKind::PlanList]
            .iter()
            .any(|kind| self.in_flight.contains(&(trade_id.clone(), *kind)))
        {
            self.status = Status::error("wait for the current plan update to finish");
            return None;
        }
        Some(trade_id)
    }

    pub(super) fn receive_accounts(&mut self, trade_id: String, accounts: Vec<AccountChoice>) {
        let Some(picker) = self
            .account_picker
            .as_mut()
            .filter(|p| p.trade_id == trade_id)
        else {
            return;
        };
        let current = self
            .plans
            .iter()
            .find(|p| p.trade_id == trade_id)
            .map(|p| p.account.as_str());
        picker.accounts = accounts
            .into_iter()
            .filter(|a| Some(a.name.as_str()) != current)
            .collect();
        picker.selected = 0;
        if picker.accounts.is_empty() {
            self.status = Status::error("no alternative accounts are configured");
        }
    }

    pub fn move_account(&mut self, delta: i32) {
        if let Some(picker) = self.account_picker.as_mut() {
            picker.move_selection(delta);
        }
    }

    pub fn accept_account(&mut self) {
        let Some(picker) = self.account_picker.as_ref() else {
            return;
        };
        let Some(account) = picker.current() else {
            return;
        };
        self.action_confirm = Some(ActionConfirmation {
            trade_id: picker.trade_id.clone(),
            action: picker.purpose.action(account.name.clone()),
        });
        self.account_picker = None;
    }

    pub fn cancel_plan_action(&mut self) {
        self.action_confirm = None;
        self.account_picker = None;
        self.status = Status::info("plan action cancelled");
    }

    pub fn confirm_plan_action(&mut self) {
        let Some(confirm) = self.action_confirm.take() else {
            return;
        };
        if self.mark_in_flight(&confirm.trade_id, JobKind::PlanAction) {
            jobs::spawn_plan_action(self.job_tx.clone(), confirm.trade_id, confirm.action);
        }
    }

    pub(super) fn refresh_plan_rows(&mut self, mut plans: Vec<PlanRow>) {
        let keep = self.current_plan().map(|p| p.trade_id.clone());
        plans.iter_mut().for_each(|row| {
            if row.direction.is_none() {
                row.direction = self
                    .plans
                    .iter()
                    .find(|old| old.trade_id == row.trade_id)
                    .and_then(|old| old.direction.clone());
            }
        });
        self.plans = plans;
        if let Some(index) =
            keep.and_then(|id| self.visible_plans().iter().position(|p| p.trade_id == id))
        {
            self.selected = index;
        }
        self.clamp_selection();
    }
}
