//! Read-only contract resolution keeps execution and spread-coverage gates distinct.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResolutionPurpose {
    Trading,
    Replay,
}

impl ResolutionPurpose {
    pub(crate) fn for_args(args: &crate::args::Args) -> Self {
        if args.replay() && args.position_entry_mode().is_none() {
            Self::Replay
        } else {
            Self::Trading
        }
    }
}

pub(crate) fn resolve_for_purpose(
    tv_symbol: &str,
    broker: ConvBroker,
    account: Option<&str>,
    purpose: ResolutionPurpose,
) -> Result<ResolvedInstrument> {
    if purpose != ResolutionPurpose::Replay || broker != ConvBroker::Ctrader {
        return resolve_for_account(tv_symbol, broker, account);
    }
    let resolved = crate::ctrader_instrument::resolve_replay(strip_exchange(tv_symbol), account)?;
    let account = account.ok_or_else(|| eyre!("cTrader requires an explicit named account"))?;
    if resolved.asset.class == AssetClass::Forex {
        require_spread_coverage(&format!("ctrader:{account}:{}", resolved.broker_symbol))?;
    } else {
        require_calibrated_replay(account, &resolved.broker_symbol)?;
        tracing::warn!(
            account,
            instrument = resolved.broker_symbol,
            "read-only commodity replay reports price/R outcomes; broker order size, margin and commissions are not simulated, and live cTrader commodity trading remains unavailable"
        );
    }
    Ok(resolved)
}

fn require_calibrated_replay(account: &str, symbol: &str) -> Result<()> {
    let key = trade_control_core::spread_blackout::ctrader_spread_key(account, symbol);
    let coverage = trade_control_core::spread_blackout::coverage(&key);
    color_eyre::eyre::ensure!(
        coverage.is_covered(),
        "cTrader commodity replay requires a calibrated account spread profile: {key:?} is {}. Collect and bake this instrument's paired history before replay; another broker's profile or legacy flat thresholds cannot establish coverage",
        coverage.reason()
    );
    Ok(())
}

#[cfg(test)]
mod tests;
