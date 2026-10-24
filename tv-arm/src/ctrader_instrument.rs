//! Resolve account-native cTrader precision without broadening live execution.
use color_eyre::eyre::{Result, ensure, eyre};
use instrument_lookup::AssetClass;

use crate::instrument_resolution::ResolvedInstrument;

mod metadata;

#[derive(Clone, Copy)]
enum Mode {
    Trading,
    Replay,
}

impl Mode {
    fn asset(self, symbol: &str) -> Result<&'static instrument_lookup::Asset> {
        let asset = match self {
            Self::Trading => instrument_lookup::resolve(symbol)?
                .ok_or_else(|| eyre!("no catalogued underlying for cTrader symbol {symbol:?}"))?,
            Self::Replay => broker_ctrader::catalogue::replay_asset(symbol).map_err(|error| {
                eyre!(
                    "cTrader native symbol {symbol:?} is not ready for replay metadata; only audited native non-stock underlyings are onboarded: {error:#}"
                )
            })?,
        };
        self.validate_class(asset.class)?;
        Ok(asset)
    }

    fn validate_class(self, class: AssetClass) -> Result<()> {
        match self {
            Self::Trading => ensure!(
                class == AssetClass::Forex,
                "cTrader initially supports catalogued FX only"
            ),
            // Class alone never establishes readiness: the replay catalogue
            // audits native non-stock metadata before this policy is checked.
            Self::Replay => {}
        }
        Ok(())
    }
}

pub(super) fn resolve(raw: &str, account: Option<&str>) -> Result<ResolvedInstrument> {
    resolve_with_mode(raw, account, Mode::Trading)
}

/// Resolve audited native non-stock precision for a price/R replay.
///
/// Asset-class permission does not onboard every broker symbol. Stocks and
/// unverified aliases remain blocked; no executable contract or volume is inferred.
pub(super) fn resolve_replay(raw: &str, account: Option<&str>) -> Result<ResolvedInstrument> {
    resolve_with_mode(raw, account, Mode::Replay)
}

fn resolve_with_mode(raw: &str, account: Option<&str>, mode: Mode) -> Result<ResolvedInstrument> {
    let account = account
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| eyre!("cTrader requires an explicit named account"))?;
    let symbol =
        trade_control_conventions::instrument_for(trade_control_conventions::Broker::Ctrader, raw);
    let asset = mode.asset(&symbol)?;
    trade_control_cli::blocking::run_blocking(async {
        let broker = broker_ctrader::CtraderBroker::connect(account).await?;
        let native = broker.instrument_metadata(&symbol).await?;
        metadata::resolve(asset, &symbol, &metadata::NativeMetadata::from(&native))
    })
}

#[cfg(test)]
mod tests;
