use super::*;
use adaptive_risk::{DailyReference, DrawdownReference, RiskPolicy};
use broker_ctrader::protection::Costs;
use std::collections::BTreeMap;
pub(super) fn policy(o: &Options) -> Result<RiskPolicy> {
    let mut p = if let Some(file) = &o.risk_policy_file {
        toml::from_str::<RiskPolicy>(&std::fs::read_to_string(file)?)?
    } else {
        ensure!(
            o.max_account_loss_pct.is_some()
                && o.daily_loss_limit_pct.is_some()
                && o.drawdown_reference.is_some()
                && o.daily_reference.is_some(),
            "prop enrollment requires explicit firm limits and reference formulas, or --risk-policy-file"
        );
        RiskPolicy::default()
    };
    if let Some(v) = o.base_risk_pct {
        p.base_risk_pct = v;
    }
    if let Some(v) = o.max_account_loss_pct {
        p.max_account_loss_pct = v;
    }
    if let Some(v) = o.internal_account_stop_pct {
        p.internal_account_stop_pct = v;
    }
    if let Some(v) = o.daily_loss_limit_pct {
        p.daily_loss_limit_pct = v;
    }
    if let Some(v) = o.internal_daily_stop_pct {
        p.internal_daily_stop_pct = v;
    }
    if let Some(v) = o.safety_buffer_pct {
        p.safety_buffer_pct = v;
    }
    if let Some(v) = &o.drawdown_reference {
        p.drawdown_reference = match v.as_str() {
            "static-initial-balance" => DrawdownReference::StaticInitialBalance,
            "trailing-balance" => DrawdownReference::TrailingBalance,
            "trailing-equity" => DrawdownReference::TrailingEquity,
            _ => return Err(eyre!("unsupported drawdown reference")),
        };
    }
    if let Some(v) = &o.daily_reference {
        p.daily_reference = match v.as_str() {
            "balance" => DailyReference::Balance,
            "equity" => DailyReference::Equity,
            "higher-of-balance-and-equity" => DailyReference::HigherOfBalanceAndEquity,
            _ => return Err(eyre!("unsupported daily reference")),
        };
    }
    if let Some(v) = &o.daily_reset {
        p.calendar.reset_time = chrono::NaiveTime::parse_from_str(v, "%H:%M:%S")
            .or_else(|_| chrono::NaiveTime::parse_from_str(v, "%H:%M"))?;
    }
    if let Some(v) = &o.risk_timezone {
        p.calendar.timezone = v.parse().map_err(|_| eyre!("invalid IANA risk timezone"))?;
    }
    if let Some(v) = &o.streak_levels {
        p.streak_backoff.levels = pairs(v)?;
    }
    if let Some(v) = &o.drawdown_levels {
        p.drawdown_backoff.levels = pairs(v)?;
    }
    if o.no_streak_backoff {
        p.streak_backoff.enabled = false;
    }
    if o.no_drawdown_backoff {
        p.drawdown_backoff.enabled = false;
    }
    if let Some(v) = o.slippage_buffer_pct_of_risk {
        p.execution.slippage_buffer_pct_of_risk = v;
    }
    if let Some(v) = o.max_data_age_seconds {
        p.execution.max_data_age_seconds = v;
    }
    p.validate()?;
    Ok(p)
}
fn pairs<T: std::str::FromStr>(s: &str) -> Result<Vec<(T, adaptive_risk::Decimal)>> {
    s.split(',')
        .map(|entry| {
            let (threshold, value) = entry
                .split_once(':')
                .ok_or_else(|| eyre!("risk tiers need threshold:value pairs"))?;
            Ok((
                threshold
                    .trim()
                    .parse()
                    .map_err(|_| eyre!("invalid risk tier threshold"))?,
                value
                    .trim()
                    .parse()
                    .map_err(|_| eyre!("invalid risk tier value"))?,
            ))
        })
        .collect()
}
pub(super) fn costs(o: &Options) -> Result<BTreeMap<String, Costs>> {
    let mut costs = if let Some(path) = &o.risk_costs_file {
        toml::from_str::<BTreeMap<String, Costs>>(&std::fs::read_to_string(path)?)?
    } else {
        BTreeMap::new()
    };
    for specification in &o.costs {
        let parts: Vec<_> = specification.split(':').collect();
        ensure!(
            (4..=5).contains(&parts.len()),
            "risk costs need SYMBOL:OPEN:CLOSE:FUTURE_SWAP[:FIXED]"
        );
        let symbol = parts[0];
        ensure!(
            !symbol.is_empty() && symbol.len() <= 128 && !symbol.chars().any(char::is_control),
            "invalid native instrument name"
        );
        let parse = |s: &str| s.parse().map_err(|_| eyre!("invalid risk cost amount"));
        let value = Costs {
            opening_per_unit: parse(parts[1])?,
            closing_per_unit: parse(parts[2])?,
            future_swap_per_unit: parse(parts[3])?,
            fixed_per_order: parse(parts.get(4).copied().unwrap_or("0"))?,
            overlapping: false,
        };
        ensure!(
            costs.insert(symbol.to_owned(), value).is_none(),
            "duplicate risk cost instrument"
        );
    }
    ensure!(
        !costs.is_empty(),
        "prop enrollment requires --risk-cost per native instrument or --risk-costs-file"
    );
    Ok(costs)
}
