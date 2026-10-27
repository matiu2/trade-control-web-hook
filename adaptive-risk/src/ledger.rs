use crate::{Decimal, RiskState, add};
use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Normalised incremental event; net_pnl includes commissions, swaps and other charges.
/// Signed volume delta is positive for opening fills, negative for closing fills, zero for charges.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Execution {
    pub event_id: String,
    pub setup_id: String,
    pub position_id: String,
    pub occurred_at: DateTime<Utc>,
    pub volume_delta: Decimal,
    pub net_pnl: Decimal,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetupOutcome {
    pub setup_id: String,
    pub closed_at: DateTime<Utc>,
    pub net_pnl: Decimal,
}
impl RiskState {
    pub fn on_execution(&mut self, event: Execution) -> Result<()> {
        let unseen = !self.executions.contains_key(&event.event_id);
        let event_time = event.occurred_at;
        let result = self.record_execution(event);
        if unseen {
            self.needs_reconciliation = true;
            self.invalidated_after = Some(
                self.invalidated_after
                    .into_iter()
                    .chain(self.last_reconciled_at)
                    .chain([event_time])
                    .max()
                    .unwrap_or(event_time),
            );
        }
        if result.is_err() {
            self.needs_reconciliation = true;
        }
        result
    }
    fn record_execution(&mut self, event: Execution) -> Result<()> {
        ensure!(
            !event.event_id.is_empty()
                && !event.setup_id.is_empty()
                && !event.position_id.is_empty(),
            "execution identity missing"
        );
        if let Some(previous) = self.executions.get(&event.event_id) {
            ensure!(previous == &event, "conflicting duplicate execution");
            return Ok(());
        }
        ensure!(
            !self.outcomes.contains_key(&event.setup_id),
            "late execution for completed setup; rebuild reconciliation"
        );
        self.executions.insert(event.event_id.clone(), event);
        Ok(())
    }
    /// Broker adapter must prove all setup positions closed and pending orders terminal.
    /// Events can arrive out of order; streak is replayed by (closed_at, setup_id).
    pub fn on_setup_closed(
        &mut self,
        setup_id: &str,
        closed_at: DateTime<Utc>,
        all_members_terminal: bool,
    ) -> Result<()> {
        let result = self.close_setup(setup_id, closed_at, all_members_terminal);
        if result.is_err() {
            self.needs_reconciliation = true;
        }
        result
    }
    fn close_setup(
        &mut self,
        setup_id: &str,
        closed_at: DateTime<Utc>,
        terminal: bool,
    ) -> Result<()> {
        ensure!(terminal, "setup still has live members");
        let events: Vec<_> = self
            .executions
            .values()
            .filter(|e| e.setup_id == setup_id)
            .collect();
        ensure!(
            !events.is_empty() && events.iter().all(|e| e.occurred_at <= closed_at),
            "incomplete setup history"
        );
        let mut volumes = std::collections::BTreeMap::<&str, Decimal>::new();
        let mut net = Decimal::ZERO;
        for e in events {
            let previous = volumes
                .get(e.position_id.as_str())
                .copied()
                .unwrap_or_default();
            volumes.insert(&e.position_id, add(previous, e.volume_delta)?);
            net = add(net, e.net_pnl)?;
        }
        ensure!(
            volumes.values().all(Decimal::is_zero),
            "setup positions not fully closed"
        );
        let outcome = SetupOutcome {
            setup_id: setup_id.into(),
            closed_at,
            net_pnl: net,
        };
        if let Some(previous) = self.outcomes.get(setup_id) {
            ensure!(previous == &outcome, "conflicting setup outcome");
            return Ok(());
        }
        self.outcomes.insert(setup_id.into(), outcome);
        self.rebuild_streak();
        Ok(())
    }
    pub fn rebuild_streak(&mut self) {
        let mut outcomes: Vec<_> = self.outcomes.values().collect();
        outcomes.sort_by(|a, b| (a.closed_at, &a.setup_id).cmp(&(b.closed_at, &b.setup_id)));
        self.consecutive_losses = outcomes.into_iter().fold(0u32, |losses, outcome| {
            if outcome.net_pnl < Decimal::ZERO {
                losses.saturating_add(1)
            } else if (outcome.net_pnl > Decimal::ZERO && self.policy.streak_backoff.reset_on_win)
                || (outcome.net_pnl.is_zero() && self.policy.streak_backoff.reset_on_break_even)
            {
                0
            } else {
                losses
            }
        });
    }
}
