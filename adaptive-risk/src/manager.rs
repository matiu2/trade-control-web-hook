use crate::{
    store::{FileStore, LockedStore},
    *,
};
use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, ensure, eyre};

pub struct RiskManager {
    pub store: FileStore,
    account_id: String,
    policy: RiskPolicy,
}
/// Account-wide interprocess gate. Hold across fresh broker reads, authorization,
/// immediate revalidation and submission. Dropping it never releases persisted risk.
pub struct RiskSession {
    store: LockedStore,
    pub(crate) state: RiskState,
}
#[derive(Clone, Debug)]
pub struct Authorization {
    account_id: String,
    policy: RiskPolicy,
    request: ProposedTrade,
    decision: RiskDecision,
}
impl Authorization {
    pub fn decision(&self) -> &RiskDecision {
        &self.decision
    }
    pub fn request_id(&self) -> &str {
        &self.request.request_id
    }
}
impl RiskManager {
    pub fn new(store: FileStore, account_id: String, policy: RiskPolicy) -> Result<Self> {
        policy.validate()?;
        Ok(Self {
            store,
            account_id,
            policy,
        })
    }
    pub fn begin(&self) -> Result<RiskSession> {
        let store = self.store.lock()?;
        let state = store.load()?;
        ensure!(
            state.account_id == self.account_id && state.policy == self.policy,
            "risk account identity or policy changed; explicit migration required"
        );
        Ok(RiskSession { store, state })
    }
}
impl RiskSession {
    fn persist(&mut self) -> Result<()> {
        if let Err(error) = self.store.save(&self.state) {
            self.state.needs_reconciliation = true;
            return Err(error);
        }
        Ok(())
    }
    pub fn state(&self) -> &RiskState {
        &self.state
    }
    pub fn latch_stops(&mut self, account: bool, daily: bool) -> Result<()> {
        if account && !self.state.account_locked {
            self.state.metrics.circuit_breaker_activations = self
                .state
                .metrics
                .circuit_breaker_activations
                .saturating_add(1);
        }
        if daily && !self.state.daily_locked {
            self.state.metrics.circuit_breaker_activations = self
                .state
                .metrics
                .circuit_breaker_activations
                .saturating_add(1);
        }
        self.state.account_locked |= account;
        self.state.daily_locked |= daily;
        self.persist()
    }
    pub fn on_execution(&mut self, execution: ledger::Execution) -> Result<()> {
        let result = self.state.on_execution(execution);
        self.persist()?;
        result
    }
    pub fn on_setup_closed(
        &mut self,
        setup: &str,
        at: DateTime<Utc>,
        terminal: bool,
    ) -> Result<()> {
        let result = self.state.on_setup_closed(setup, at, terminal);
        self.persist()?;
        result
    }
    pub fn disconnect(&mut self) -> Result<()> {
        self.state.invalidated_after = self
            .state
            .invalidated_after
            .max(self.state.last_reconciled_at);
        self.state.needs_reconciliation = true;
        self.persist()
    }
    pub fn reconcile(
        &mut self,
        snapshot: &AccountSnapshot,
        rule: &dyn LossRule,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.state.needs_reconciliation = true;
        self.persist()?;
        self.apply_reconciliation(snapshot, rule, now)?;
        self.state.needs_reconciliation = false;
        self.track_reduced(snapshot, rule, now);
        self.persist()
    }
    fn apply_reconciliation(
        &mut self,
        a: &AccountSnapshot,
        rule: &dyn LossRule,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let s = &mut self.state;
        ensure!(a.connected, "broker disconnected");
        decision::validate_snapshot(&s.policy, s, a, now)?;
        ensure!(
            a.day_reference.day.start >= s.day.day.start,
            "trading day moved backwards"
        );
        if a.day_reference.day == s.day.day {
            ensure!(
                a.day_reference == s.day,
                "daily reference changed within trading day"
            );
        }
        ensure!(
            s.reservations
                .iter()
                .all(|(id, reservation)| !reservation.submitted
                    || a.resolved_requests.contains(id)
                    || a.exposures
                        .iter()
                        .any(|e| e.request_id.as_ref() == Some(id))),
            "unresolved request; broker history reconciliation required"
        );
        decision::reserved_risk(&s.policy, s, a)?;
        if a.day_reference.day != s.day.day {
            s.day = a.day_reference.clone();
            s.daily_locked = false;
        }
        s.reservations
            .retain(|id, _| !a.resolved_requests.contains(id));
        s.balance_high = s.balance_high.max(a.balance_high);
        s.equity_high = s.equity_high.max(a.equity_high);
        let floors = rule.thresholds(&s.policy, s, a)?;
        if a.equity <= floors.internal_daily_floor && !s.daily_locked {
            s.daily_locked = true;
            s.metrics.circuit_breaker_activations =
                s.metrics.circuit_breaker_activations.saturating_add(1);
        }
        if a.equity <= floors.internal_account_floor && !s.account_locked {
            s.account_locked = true;
            s.metrics.circuit_breaker_activations =
                s.metrics.circuit_breaker_activations.saturating_add(1);
        }
        s.last_sequence = Some(a.sequence);
        s.last_reconciled_at = Some(a.observed_at);
        Ok(())
    }
    pub fn authorize(
        &mut self,
        a: &AccountSnapshot,
        trade: ProposedTrade,
        rule: &dyn LossRule,
        now: DateTime<Utc>,
    ) -> Result<Authorization> {
        let decision = evaluate_trade(&self.state.policy, a, &trade, &self.state, rule, now);
        if !decision.approved {
            self.state.metrics.blocked_trades = self.state.metrics.blocked_trades.saturating_add(1);
        }
        self.observe(a, &trade, &decision, rule, now);
        if !decision.approved {
            self.persist()?;
            return Err(eyre!("risk blocked: {:?}", decision.reasons));
        }
        let reservation = Reservation {
            request_id: trade.request_id.clone(),
            setup_id: trade.setup_id.clone(),
            risk: decision.allowed_risk_amount,
            volume: decision.max_volume,
            created_at: now,
            submitted: false,
        };
        self.state
            .attempted_requests
            .insert(trade.request_id.clone());
        self.state
            .reservations
            .insert(trade.request_id.clone(), reservation);
        self.persist()?;
        Ok(Authorization {
            account_id: self.state.account_id.clone(),
            policy: self.state.policy.clone(),
            request: trade,
            decision,
        })
    }
    /// Recalculate with a fresh quote/account snapshot immediately before transport.
    /// The original authorization is a ceiling; volume/risk may never increase.
    pub fn before_submit(
        &mut self,
        authorization: &Authorization,
        refreshed: &ProposedTrade,
        a: &AccountSnapshot,
        rule: &dyn LossRule,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.validate_token(authorization)?;
        let mut expected = authorization.request.clone();
        expected.estimate.price = refreshed.estimate.price;
        expected.estimate.conversion = refreshed.estimate.conversion;
        expected.estimate.costs_per_unit = refreshed.estimate.costs_per_unit;
        expected.estimate.fixed_costs = refreshed.estimate.fixed_costs;
        ensure!(&expected == refreshed, "immutable order terms changed");
        self.reconcile(a, rule, now)?;
        let mut check = self.state.clone();
        check.reservations.remove(authorization.request_id());
        check.attempted_requests.remove(authorization.request_id());
        let result = evaluate_trade(&check.policy, a, refreshed, &check, rule, now);
        let multiplier = if refreshed.overlapping {
            check.policy.execution.overlap_multiplier
        } else {
            Decimal::ONE
        };
        let fresh_risk = mul(
            refreshed.estimate.loss(
                authorization.decision.max_volume,
                check.policy.execution.slippage_buffer_pct_of_risk,
            )?,
            multiplier,
        )?;
        ensure!(
            result.approved
                && result.max_volume >= authorization.decision.max_volume
                && fresh_risk <= authorization.decision.allowed_risk_amount,
            "authorization expired or risk increased"
        );
        let reservation = self
            .state
            .reservations
            .get_mut(authorization.request_id())
            .ok_or_else(|| eyre!("authorization missing"))?;
        ensure!(!reservation.submitted, "request already submitted");
        reservation.submitted = true;
        self.state.needs_reconciliation = true;
        self.persist()
    }
    fn validate_token(&self, authorization: &Authorization) -> Result<()> {
        ensure!(
            authorization.account_id == self.state.account_id
                && authorization.policy == self.state.policy,
            "authorization belongs to another account/policy"
        );
        let reservation = self
            .state
            .reservations
            .get(authorization.request_id())
            .ok_or_else(|| eyre!("authorization missing"))?;
        ensure!(
            reservation.setup_id == authorization.request.setup_id
                && reservation.risk == authorization.decision.allowed_risk_amount
                && reservation.volume == authorization.decision.max_volume,
            "reservation does not match authorization"
        );
        Ok(())
    }
    /// Only a definitive NotSent/Rejected response proving zero fills can release here.
    /// Accepted/partial/unknown results retain risk until broker reconciliation.
    pub fn rejected_without_fill(&mut self, authorization: &Authorization) -> Result<()> {
        self.validate_token(authorization)?;
        self.state.reservations.remove(authorization.request_id());
        self.state.needs_reconciliation = true;
        self.persist()
    }
    fn track_reduced(&mut self, a: &AccountSnapshot, rule: &dyn LossRule, now: DateTime<Utc>) {
        if let Ok(metrics) = crate::metrics::snapshot(&self.state, a, rule, now) {
            if metrics.risk_multiplier < Decimal::ONE {
                self.state.metrics.reduced_since.get_or_insert(now);
            } else if let Some(start) = self.state.metrics.reduced_since.take() {
                self.state.metrics.reduced_risk_seconds = self
                    .state
                    .metrics
                    .reduced_risk_seconds
                    .saturating_add((now - start).num_seconds().max(0));
            }
        }
    }
    fn observe(
        &mut self,
        a: &AccountSnapshot,
        t: &ProposedTrade,
        d: &RiskDecision,
        rule: &dyn LossRule,
        now: DateTime<Utc>,
    ) {
        self.track_reduced(a, rule, now);
        let p = &self.state.policy;
        let multiplier = p.streak_multiplier(self.state.consecutive_losses);
        let telemetry = crate::metrics::snapshot(&self.state, a, rule, now);
        let drawdown = telemetry
            .as_ref()
            .map_or(d.drawdown_pct, |m| m.account_drawdown_pct);
        let daily_loss = telemetry
            .as_ref()
            .map_or(d.daily_loss_pct, |m| m.daily_loss_pct);
        let reserved = telemetry
            .as_ref()
            .map_or(d.reserved_exposure, |m| m.total_reserved_exposure);
        let daily_remaining = telemetry
            .as_ref()
            .map_or(d.daily_remaining, |m| m.daily_budget_remaining);
        let reduced_seconds = telemetry
            .as_ref()
            .map_or(self.state.metrics.reduced_risk_seconds, |m| {
                m.reduced_risk_seconds
            });
        match telemetry {
            Ok(metrics) => {
                tracing::info!(account_id=%a.account_id,metrics=?metrics,"account risk metrics")
            }
            Err(error) => {
                tracing::warn!(account_id=%a.account_id,%error,"account risk telemetry unavailable")
            }
        }
        tracing::info!(timestamp=%now,account_id=%a.account_id,strategy_id=%t.strategy_id,setup_id=%t.setup_id,account_equity=%a.equity,account_drawdown_pct=%drawdown,daily_loss_pct=%daily_loss,consecutive_losses=self.state.consecutive_losses,base_risk_pct=%p.base_risk_pct,streak_risk_pct=%(p.base_risk_pct*multiplier),drawdown_risk_pct=%p.drawdown_risk_pct(drawdown),allowed_risk_pct=%d.allowed_risk_pct,reserved_exposure=%reserved,decision=d.approved,limiting_policy=?d.limiting_policy,risk_multiplier=%multiplier,daily_budget_remaining=%daily_remaining,blocked_trades=self.state.metrics.blocked_trades,circuit_breaker_activations=self.state.metrics.circuit_breaker_activations,reduced_risk_seconds=reduced_seconds,"account risk decision");
    }
}
