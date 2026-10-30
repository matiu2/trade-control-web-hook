//! Retain a spread-blocked signal and judge recovery from observed quotes only.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::broker::Quote;
use crate::intent::Direction;

/// Shared live polling cadence; recovery replay defaults to M1 quote samples.
pub const POLL_SECONDS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryTiming {
    Immediate,
    Adaptive,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpreadRetry {
    pub timing: RetryTiming,
    pub max_age_secs: u32,
    pub max_chase_r: f64,
    pub lookahead_secs: u32,
    pub min_improvement_r: f64,
}

impl SpreadRetry {
    pub fn valid(self) -> bool {
        (1..=86_400).contains(&self.max_age_secs)
            && (1..=3600).contains(&self.lookahead_secs)
            && self.max_chase_r.is_finite()
            && (0.0..=2.0).contains(&self.max_chase_r)
            && self.min_improvement_r.is_finite()
            && (0.0..=2.0).contains(&self.min_improvement_r)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Recovery {
    pub policy: SpreadRetry,
    pub direction: Direction,
    pub ideal_entry: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub boundary: f64,
    pub min_r: f64,
    pub started_at: DateTime<Utc>,
    pub sampled_at: DateTime<Utc>,
    pub last_entry: f64,
    pub last_spread: f64,
    pub spread_decay: f64,
    pub adverse_speed: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    Wait(&'static str),
    Ready,
    Drop(&'static str),
}

impl Recovery {
    pub fn risk(self) -> f64 {
        (self.ideal_entry - self.stop_loss).abs()
    }

    pub fn executable(self, quote: &Quote) -> f64 {
        match self.direction {
            Direction::Long => quote.ask,
            Direction::Short => quote.bid,
        }
    }

    fn sign(self) -> f64 {
        match self.direction {
            Direction::Long => 1.0,
            Direction::Short => -1.0,
        }
    }

    pub fn chase_r(self, entry: f64) -> f64 {
        ((entry - self.ideal_entry) * self.sign()).max(0.0) / self.risk()
    }

    fn reward_r(self, entry: f64, spread: f64) -> Option<f64> {
        let risk = (entry - self.stop_loss) * self.sign();
        let reward = (self.take_profit - entry) * self.sign();
        let stop_distance = risk.max(10.0 * spread);
        let boundary_distance = (entry - self.boundary) * self.sign();
        (risk > 0.0 && reward > 0.0 && stop_distance < boundary_distance)
            .then_some(reward / stop_distance)
    }

    /// Update exponentially smoothed rates using elapsed time, never future bars.
    fn observe(&mut self, quote: &Quote, now: DateTime<Utc>) {
        let seconds = (now - self.sampled_at).num_milliseconds() as f64 / 1000.0;
        if seconds <= 0.0 {
            return;
        }
        let entry = self.executable(quote);
        let alpha = 1.0 - (-seconds / f64::from(self.policy.lookahead_secs)).exp();
        let decay = (self.last_spread - quote.spread()) / seconds;
        let speed = (entry - self.last_entry) * self.sign() / seconds;
        self.spread_decay += alpha * (decay - self.spread_decay);
        self.adverse_speed += alpha * (speed - self.adverse_speed);
        self.last_entry = entry;
        self.last_spread = quote.spread();
        self.sampled_at = now;
    }

    pub fn decide(&mut self, quote: &Quote, now: DateTime<Utc>) -> Decision {
        if !self.policy.valid()
            || !self.risk().is_finite()
            || self.risk() <= 0.0
            || ![
                self.ideal_entry,
                self.stop_loss,
                self.take_profit,
                self.boundary,
                self.min_r,
                self.spread_decay,
                self.adverse_speed,
                self.last_entry,
                self.last_spread,
            ]
            .into_iter()
            .all(f64::is_finite)
            || self.min_r <= 0.0
        {
            return Decision::Drop("invalid-recovery-geometry");
        }
        if now >= self.started_at + chrono::Duration::seconds(i64::from(self.policy.max_age_secs)) {
            return Decision::Drop("spread-retry-expired");
        }
        if ![quote.bid, quote.ask].into_iter().all(f64::is_finite) || quote.ask < quote.bid {
            return Decision::Wait("no-valid-quote");
        }
        self.observe(quote, now);
        let entry = self.executable(quote);
        let exit = match self.direction {
            Direction::Long => quote.bid,
            Direction::Short => quote.ask,
        };
        if (exit - self.stop_loss) * self.sign() <= 0.0
            || (self.take_profit - exit) * self.sign() <= 0.0
        {
            return Decision::Drop("spread-retry-bracket-consumed");
        }
        if (entry - self.ideal_entry) * self.sign() < 0.0 {
            return Decision::Wait("waiting-for-original-entry");
        }
        if self.chase_r(entry) > self.policy.max_chase_r {
            return Decision::Drop("spread-retry-price-ran-away");
        }
        if (entry - self.stop_loss) * self.sign() <= 0.0
            || (self.take_profit - entry) * self.sign() <= 0.0
        {
            return Decision::Drop("spread-retry-bracket-consumed");
        }
        let Some(current_r) = self.reward_r(entry, quote.spread()) else {
            return Decision::Wait("spread-still-past-veto");
        };
        if current_r < self.min_r {
            return Decision::Wait("spread-still-below-min-r");
        }
        if self.policy.timing == RetryTiming::Immediate || self.spread_decay <= 0.0 {
            return Decision::Ready;
        }
        let remaining = (self.started_at
            + chrono::Duration::seconds(i64::from(self.policy.max_age_secs))
            - now)
            .num_seconds() as f64;
        let horizon = f64::from(self.policy.lookahead_secs).min(remaining);
        let predicted_entry = entry + self.sign() * self.adverse_speed * horizon;
        let predicted_spread = (quote.spread() - self.spread_decay * horizon).max(0.0);
        if self.chase_r(predicted_entry) <= self.policy.max_chase_r
            && self
                .reward_r(predicted_entry, predicted_spread)
                .is_some_and(|r| r > current_r + self.policy.min_improvement_r)
        {
            Decision::Wait("spread-decaying-faster-than-price-cost")
        } else {
            Decision::Ready
        }
    }
}

#[cfg(test)]
mod tests;
