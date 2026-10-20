//! Resolve cTrader FX symbols using the selected account's contract metadata.
use color_eyre::eyre::{Result, ensure, eyre};
use instrument_lookup::AssetClass;

use crate::{instrument_resolution::ResolvedInstrument, precision::CatalogPrecision};

pub(super) fn resolve(raw: &str, account: Option<&str>) -> Result<ResolvedInstrument> {
    let account = account
        .filter(|name| !name.is_empty())
        .ok_or_else(|| eyre!("cTrader requires an explicit named account"))?;
    let symbol =
        trade_control_conventions::instrument_for(trade_control_conventions::Broker::Ctrader, raw);
    let asset = instrument_lookup::resolve(&symbol)?
        .ok_or_else(|| eyre!("no catalogued underlying for cTrader symbol {raw:?}"))?;
    ensure!(
        asset.class == AssetClass::Forex,
        "cTrader initially supports catalogued FX only"
    );
    trade_control_cli::blocking::run_blocking(async {
        let broker = broker_ctrader::CtraderBroker::connect(account).await?;
        let metadata = broker.instrument_metadata(&symbol).await?;
        let decimal_places = u8::try_from(metadata.digits)?;
        ensure!(decimal_places <= 12, "unsupported cTrader precision");
        let tick_size = 10_f64.powi(-i32::from(decimal_places));
        ensure!(
            metadata.pip_size.is_finite() && metadata.pip_size > 0.0,
            "invalid cTrader pip size"
        );
        Ok(ResolvedInstrument {
            asset,
            broker_symbol: metadata.name,
            precision: CatalogPrecision {
                tick_size,
                pip_size: metadata.pip_size,
                decimal_places,
                class: asset.class,
                contract_multiplier: None,
            },
        })
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn named_account_required_before_network() {
        let error = super::resolve("EURUSD", None).unwrap_err();
        assert!(error.to_string().contains("explicit named account"));
    }
}
