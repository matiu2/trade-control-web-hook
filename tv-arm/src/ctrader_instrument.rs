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
    fn validate_class(self, class: AssetClass) -> Result<()> {
        match self {
            Self::Trading => ensure!(
                class == AssetClass::Forex,
                "cTrader initially supports catalogued FX only"
            ),
            Self::Replay => ensure!(
                matches!(class, AssetClass::Forex | AssetClass::Commodity),
                "cTrader replay supports catalogued FX and commodity instruments only"
            ),
        }
        Ok(())
    }
}

pub(super) fn resolve(raw: &str, account: Option<&str>) -> Result<ResolvedInstrument> {
    resolve_with_mode(raw, account, Mode::Trading)
}

/// Resolve precision for a price/R replay, not an executable contract or volume.
pub(super) fn resolve_replay(raw: &str, account: Option<&str>) -> Result<ResolvedInstrument> {
    resolve_with_mode(raw, account, Mode::Replay)
}

fn resolve_with_mode(raw: &str, account: Option<&str>, mode: Mode) -> Result<ResolvedInstrument> {
    let account = account
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| eyre!("cTrader requires an explicit named account"))?;
    let symbol =
        trade_control_conventions::instrument_for(trade_control_conventions::Broker::Ctrader, raw);
    let asset = instrument_lookup::resolve(&symbol)?
        .ok_or_else(|| eyre!("no catalogued underlying for cTrader symbol {raw:?}"))?;
    mode.validate_class(asset.class)?;
    trade_control_cli::blocking::run_blocking(async {
        let broker = broker_ctrader::CtraderBroker::connect(account).await?;
        let native = broker.instrument_metadata(&symbol).await?;
        metadata::resolve(asset, &symbol, &metadata::NativeMetadata::from(&native))
    })
}

#[cfg(test)]
mod tests;
