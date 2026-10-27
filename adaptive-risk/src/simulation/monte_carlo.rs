use super::statistics::aggregate;
use super::*;
use crate::calendar::TradingCalendar;
use crate::*;
use chrono::Duration;
use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, ensure};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonteCarloConfig {
    pub simulations: usize,
    pub trades: usize,
    pub seed: u64,
    /// Explicit empirical/modelled distribution; sampled once per day (zero-trade days supported).
    pub trades_per_day: Vec<u32>,
    pub block_size: usize,
    pub initial_balance: Decimal,
    pub target_pct: Decimal,
    pub start: DateTime<Utc>,
    pub win_probability: Decimal,
    pub average_winner_r: Decimal,
    pub average_loser_r: Decimal,
}
impl Default for MonteCarloConfig {
    fn default() -> Self {
        Self {
            simulations: 10000,
            trades: 200,
            seed: 42,
            trades_per_day: vec![1, 2, 3],
            block_size: 5,
            initial_balance: Decimal::from(10000),
            target_pct: Decimal::from(10),
            start: DateTime::from_timestamp(1767650400, 0).unwrap_or_default(),
            win_probability: Decimal::new(58, 2),
            average_winner_r: Decimal::new(213, 2),
            average_loser_r: Decimal::NEGATIVE_ONE,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct Interval {
    pub mean: f64,
    pub lower: f64,
    pub upper: f64,
}
#[derive(Clone, Debug, Serialize)]
pub struct MonteCarloReport {
    pub strategy: Strategy,
    pub dataset: Dataset,
    pub simulations: usize,
    pub seed: u64,
    pub funding_probability: Interval,
    pub net_return_pct: Interval,
    pub max_drawdown_pct: Interval,
    pub average_risk_pct: Interval,
    pub average_reserved_risk_pct: Interval,
    pub longest_losing_streak: Interval,
    pub skipped_trades: Interval,
    pub daily_lockouts: Interval,
    pub account_limit_breaches: Interval,
    pub daily_firm_limit_breaches: Interval,
    /// Conditional on reaching target; absent when no run reaches it.
    pub trades_to_target: Option<Interval>,
}
pub fn monte_carlo(
    p: &RiskPolicy,
    c: &MonteCarloConfig,
    actual: Option<(&[TradeRecord], Dataset)>,
) -> Result<Vec<MonteCarloReport>> {
    p.validate()?;
    ensure!(
        c.simulations > 0
            && c.trades > 0
            && c.block_size > 0
            && !c.trades_per_day.is_empty()
            && c.trades_per_day.iter().any(|n| *n > 0),
        "invalid simulation dimensions or trades/day"
    );
    ensure!(
        c.win_probability >= Decimal::ZERO
            && c.win_probability <= Decimal::ONE
            && c.average_winner_r > Decimal::ZERO
            && c.average_loser_r < Decimal::ZERO,
        "invalid provisional statistics"
    );
    if let Some((records, _)) = actual {
        ensure!(
            !records.is_empty()
                && c.block_size <= records.len()
                && records.windows(2).all(|w| w[0].at <= w[1].at),
            "historical bootstrap requires chronological outcomes and block_size <= record count"
        );
    }
    let mut rng = ChaCha8Rng::seed_from_u64(c.seed);
    let mut results: [Vec<ReplayReport>; 4] =
        std::array::from_fn(|_| Vec::with_capacity(c.simulations));
    for _ in 0..c.simulations {
        let trades = sample(c, p, &mut rng, actual.map(|(records, _)| records))?;
        for (slot, result) in results.iter_mut().zip(compare(
            p,
            &trades,
            actual.map_or(Dataset::SyntheticProvisional, |(_, dataset)| dataset),
            c.initial_balance,
            c.target_pct,
        )?) {
            slot.push(result.report);
        }
    }
    [
        Strategy::Fixed,
        Strategy::StreakOnly,
        Strategy::DrawdownOnly,
        Strategy::Combined,
    ]
    .into_iter()
    .zip(results)
    .map(|(strategy, runs)| {
        aggregate(
            strategy,
            c,
            actual.map_or(Dataset::SyntheticProvisional, |(_, d)| d),
            &runs,
        )
    })
    .collect()
}
fn sample(
    c: &MonteCarloConfig,
    p: &RiskPolicy,
    rng: &mut ChaCha8Rng,
    actual: Option<&[TradeRecord]>,
) -> Result<Vec<TradeRecord>> {
    let mut outcomes = Vec::with_capacity(c.trades);
    while outcomes.len() < c.trades {
        if let Some(records) = actual {
            let start = rng.random_range(0..records.len());
            outcomes.extend(
                (0..c.block_size).map(|offset| records[(start + offset) % records.len()].net_r),
            );
        } else {
            let draw = Decimal::from(rng.random_range(0..1000000u32)) / Decimal::from(1000000);
            outcomes.push(if draw < c.win_probability {
                c.average_winner_r
            } else {
                c.average_loser_r
            });
        }
    }
    outcomes.truncate(c.trades);
    timestamps(c, &p.calendar, rng)?
        .into_iter()
        .zip(outcomes)
        .map(|(at, net_r)| Ok(TradeRecord { at, net_r }))
        .collect()
}
fn timestamps(
    c: &MonteCarloConfig,
    calendar: &TradingCalendar,
    rng: &mut ChaCha8Rng,
) -> Result<Vec<DateTime<Utc>>> {
    let mut result = Vec::with_capacity(c.trades);
    let mut day = calendar.day_at(c.start)?;
    let mut empty_days = 0;
    while result.len() < c.trades {
        let count = c.trades_per_day[rng.random_range(0..c.trades_per_day.len())];
        if count == 0 {
            empty_days += 1;
            ensure!(
                empty_days < 100000,
                "trade timestamp distribution produced too many empty days"
            );
        } else {
            empty_days = 0;
        }
        for n in 0..count {
            let millis = (day.end - day.start).num_milliseconds() * i64::from(n + 1)
                / (i64::from(count) + 1);
            result.push(day.start + Duration::milliseconds(millis));
            if result.len() == c.trades {
                break;
            }
        }
        day = calendar.day_at(day.end)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
