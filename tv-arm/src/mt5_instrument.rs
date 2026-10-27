//! Account-specific MT5 order symbols and precision, with shared news metadata.
use color_eyre::eyre::{Result, WrapErr, ensure, eyre};
use instrument_lookup::{Asset, pip_size_from};
use mt5_data_source::{account_config::AccountConfig, web::Symbol, web::trading::SymbolInfo};

use crate::{instrument_resolution::ResolvedInstrument, precision::CatalogPrecision};

/// MT5 spot/CFD aliases refer to shared underlyings, never futures series.
/// The live account catalogue still decides whether a name is actually listed.
pub(super) fn catalog_symbol(raw: &str) -> String {
    let compact = trade_control_conventions::instrument_for(
        trade_control_conventions::Broker::Mt5,
        raw.trim(),
    );
    let root = compact
        .split_once('.')
        .map_or(compact.as_str(), |(root, _)| root);
    match root.to_ascii_uppercase().as_str() {
        "SP500" => "SPX500".into(),
        "DAX40" => "DE30".into(),
        "JPN225" => "JP225".into(),
        "XBRUSD" | "BCOUSD" => "BCO".into(),
        "XTIUSD" | "WTICOUSD" => "WTICO".into(),
        _ => root.to_string(),
    }
}

pub(super) fn resolve(
    raw: &str,
    asset: &'static Asset,
    account: Option<&str>,
) -> Result<ResolvedInstrument> {
    let account = account
        .filter(|name| !name.is_empty())
        .ok_or_else(|| eyre!("MT5 requires an explicit named account"))?;
    let config = AccountConfig::load(account)?;
    trade_control_cli::blocking::run_blocking(async {
        let trader = config.trader().await?;
        let listed = trader.symbols().await?;
        let name = select_symbol(raw, asset, &listed)?;
        let symbol = trader
            .symbol(&name)
            .await
            .wrap_err_with(|| format!("reading MT5 contract {name:?} on account {account:?}"))?;
        from_metadata(asset, &symbol)
    })
}

fn select_symbol(raw: &str, asset: &Asset, listed: &[Symbol]) -> Result<String> {
    let normalized = trade_control_conventions::instrument_for(
        trade_control_conventions::Broker::Mt5,
        raw.trim(),
    );
    if let Some(symbol) = listed
        .iter()
        .find(|symbol| symbol.name.eq_ignore_ascii_case(&normalized))
    {
        return Ok(symbol.name.clone());
    }
    ensure!(
        !normalized.contains('.'),
        "MT5 account does not list exact symbol {raw:?}"
    );
    let candidates = listed
        .iter()
        .map(|symbol| {
            let matched = instrument_lookup::resolve(&catalog_symbol(&symbol.name))?
                .is_some_and(|candidate| same_underlying(candidate, asset));
            Ok(matched.then_some(symbol.name.as_str()))
        })
        .collect::<Result<Vec<_>>>()?;
    let candidates: Vec<_> = candidates.into_iter().flatten().collect();
    match candidates.as_slice() {
        [name] => Ok((*name).to_string()),
        [] => Err(eyre!(
            "MT5 account does not list {raw:?} (asset {}); select its native broker symbol",
            asset.id
        )),
        _ => Err(eyre!(
            "MT5 symbol {raw:?} is ambiguous: {candidates:?}; select an exact native broker symbol"
        )),
    }
}

/// Legacy catalog rows may have distinct IDs for one broker instrument (for
/// example SPX500 and SPX500USD). Compare actual shared broker legs as well;
/// differently denominated contracts retain their distinct symbols.
fn same_underlying(left: &Asset, right: &Asset) -> bool {
    catalog_symbol(&left.id) == catalog_symbol(&right.id)
        || [
            instrument_lookup::Broker::Oanda,
            instrument_lookup::Broker::TradeNation,
        ]
        .into_iter()
        .any(|broker| {
            left.symbol_for(broker)
                .filter(|symbol| !symbol.is_empty())
                .is_some_and(|symbol| right.symbol_for(broker) == Some(symbol))
        })
}

fn from_metadata(asset: &'static Asset, symbol: &SymbolInfo) -> Result<ResolvedInstrument> {
    broker_mt5::contract::validate_calc_mode(symbol.calc_mode).map_err(|error| {
        eyre!(
            "MT5 symbol {:?} has unsupported profit calculation mode {}: {error:?}",
            symbol.name,
            symbol.calc_mode
        )
    })?;
    ensure!(
        symbol.tick_size.is_finite() && symbol.tick_size >= 0.0,
        "invalid MT5 tick size for {:?}",
        symbol.name
    );
    let decimal_places = u8::try_from(symbol.digits)?;
    let tick_size = if symbol.tick_size > 0.0 {
        symbol.tick_size
    } else {
        symbol.point
    };
    ensure!(
        decimal_places <= 12 && tick_size.is_finite() && tick_size > 0.0,
        "invalid MT5 precision for {:?}",
        symbol.name
    );
    Ok(ResolvedInstrument {
        asset,
        broker_symbol: symbol.name.clone(),
        precision: CatalogPrecision {
            tick_size,
            pip_size: pip_size_from(asset.class, decimal_places, tick_size),
            decimal_places,
            class: asset.class,
            // MT5 sizes in lots from live contract metadata, not futures multipliers.
            contract_multiplier: None,
        },
    })
}

#[cfg(test)]
mod tests;
