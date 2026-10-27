use super::*;
use crate::Decimal;
use color_eyre::Result;
fn number(v: Decimal) -> Result<f64> {
    Ok(v.to_string().parse()?)
}
fn mean_interval(values: &[f64]) -> Interval {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = if values.len() < 2 {
        0.0
    } else {
        values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)
    };
    let margin = 1.96 * (variance / n).sqrt();
    Interval {
        mean,
        lower: mean - margin,
        upper: mean + margin,
    }
}
pub(super) fn aggregate(
    strategy: Strategy,
    c: &MonteCarloConfig,
    dataset: Dataset,
    runs: &[ReplayReport],
) -> Result<MonteCarloReport> {
    let collect = |f: fn(&ReplayReport) -> Result<f64>| -> Result<Interval> {
        Ok(mean_interval(
            &runs.iter().map(f).collect::<Result<Vec<_>>>()?,
        ))
    };
    let n = runs.len() as f64;
    let fraction = runs.iter().filter(|r| r.funded).count() as f64 / n;
    let z = 1.96;
    let center = (fraction + z * z / (2.0 * n)) / (1.0 + z * z / n);
    let margin =
        z * ((fraction * (1.0 - fraction) + z * z / (4.0 * n)) / n).sqrt() / (1.0 + z * z / n);
    let targets: Vec<f64> = runs
        .iter()
        .filter_map(|r| r.trades_to_target.map(|n| n as f64))
        .collect();
    Ok(MonteCarloReport {
        strategy,
        dataset,
        simulations: runs.len(),
        seed: c.seed,
        funding_probability: Interval {
            mean: fraction,
            lower: (center - margin).max(0.0),
            upper: (center + margin).min(1.0),
        },
        net_return_pct: collect(|r| number(r.net_return_pct))?,
        max_drawdown_pct: collect(|r| number(r.max_drawdown_pct))?,
        average_risk_pct: collect(|r| number(r.average_risk_pct))?,
        average_reserved_risk_pct: collect(|r| number(r.average_reserved_risk_pct))?,
        longest_losing_streak: collect(|r| Ok(f64::from(r.longest_losing_streak)))?,
        skipped_trades: collect(|r| Ok(r.skipped_trades as f64))?,
        daily_lockouts: collect(|r| Ok(r.daily_lockouts as f64))?,
        account_limit_breaches: collect(|r| Ok(r.account_limit_breaches as f64))?,
        daily_firm_limit_breaches: collect(|r| Ok(r.daily_firm_limit_breaches as f64))?,
        trades_to_target: (!targets.is_empty()).then(|| mean_interval(&targets)),
    })
}
