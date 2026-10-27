use super::*;
use std::io::{self, IsTerminal, Write};
pub(super) fn complete(o: &Options) -> Result<Options> {
    let mut o = o.clone();
    if !o.prop_firm || !io::stdin().is_terminal() {
        return Ok(o);
    }
    if o.risk_policy_file.is_none() {
        amount(&mut o.daily_loss_limit_pct, "Firm daily loss limit (%)")?;
        amount(&mut o.max_account_loss_pct, "Firm overall loss limit (%)")?;
        field(
            &mut o.daily_reference,
            "Firm daily reference (balance/equity/higher-of-balance-and-equity)",
            None,
        )?;
        field(
            &mut o.drawdown_reference,
            "Overall reference (static-initial-balance/trailing-balance/trailing-equity)",
            None,
        )?;
        field(
            &mut o.daily_reset,
            "Firm daily reset local time",
            Some("17:00"),
        )?;
        field(
            &mut o.risk_timezone,
            "Firm daily reset IANA timezone",
            Some("America/New_York"),
        )?;
    }
    amount(&mut o.initial_balance, "Verified initial account balance")?;
    if !o.new_flat_account && o.day_start_balance.is_none() && o.day_start_equity.is_none() {
        o.new_flat_account = yes(
            "Is this an unused, flat account whose day-start balance/equity equal its initial balance?",
        )?;
    }
    if !o.new_flat_account {
        amount(&mut o.day_start_balance, "Verified firm day-start balance")?;
        amount(&mut o.day_start_equity, "Verified firm day-start equity")?;
    }
    if o.costs.is_empty() && o.risk_costs_file.is_none() {
        o.costs = input(
            "Native symbol costs SYMBOL:OPEN:CLOSE:FUTURE_SWAP[:FIXED], comma separated",
            None,
        )?
        .split(',')
        .map(|s| s.trim().to_owned())
        .collect();
    }
    let p = super::policy::policy(&o)?;
    if (!matches!(p.daily_reference, adaptive_risk::DailyReference::Balance)
        || matches!(
            p.drawdown_reference,
            adaptive_risk::DrawdownReference::TrailingEquity
        ))
        && o.firm_evidence_file.is_none()
    {
        o.firm_evidence_file = Some(std::path::PathBuf::from(input(
            "Absolute authoritative firm/broker evidence file path",
            None,
        )?));
    }
    if !o.rules_verified {
        o.rules_verified = yes(
            "Have you verified these rules and reset semantics against your purchased program?",
        )?;
    }
    if !o.fees_verified {
        o.fees_verified = yes(
            "Have you verified the fee signs, commission accounting and conservative cost estimates?",
        )?;
    }
    Ok(o)
}
fn amount(target: &mut Option<adaptive_risk::Decimal>, label: &str) -> Result<()> {
    if target.is_none() {
        *target = Some(
            input(label, None)?
                .parse()
                .map_err(|_| eyre!("invalid decimal amount"))?,
        );
    }
    Ok(())
}
fn field(target: &mut Option<String>, label: &str, default: Option<&str>) -> Result<()> {
    if target.is_none() {
        *target = Some(input(label, default)?);
    }
    Ok(())
}
fn yes(label: &str) -> Result<bool> {
    Ok(matches!(
        input(label, Some("no"))?.to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
fn input(label: &str, default: Option<&str>) -> Result<String> {
    if let Some(default) = default {
        print!("{label} [{default}]: ");
    } else {
        print!("{label}: ");
    }
    io::stdout().flush()?;
    let mut line = String::new();
    ensure!(
        io::stdin().read_line(&mut line)? > 0,
        "account setup input ended"
    );
    let line = line.trim();
    if line.is_empty() {
        return default
            .map(str::to_owned)
            .ok_or_else(|| eyre!("account setting required"));
    }
    Ok(line.to_owned())
}
