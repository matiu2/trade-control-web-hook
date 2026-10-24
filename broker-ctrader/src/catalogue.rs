//! Audited non-stock underlyings for native cTrader price/R replay.
//!
//! This is an underlying lookup, not an order-sizing model or a replacement
//! for the caller's live account/symbol identity and precision validation.
use color_eyre::{Result, eyre::eyre};
use instrument_lookup::Asset;
use std::{collections::BTreeMap, sync::LazyLock};

mod build;
mod facts;

static ASSETS: LazyLock<Result<BTreeMap<String, Asset>>> = LazyLock::new(|| {
    let snapshot = facts::snapshot()?;
    facts::validate(&snapshot)?;
    build::assets(&snapshot.records)
});

fn assets() -> Result<&'static BTreeMap<String, Asset>> {
    ASSETS.as_ref().map_err(|error| eyre!("{error:#}"))
}

/// Resolve only an audited native non-stock name, never another broker's alias.
/// The caller must still validate the selected account's current native symbol.
pub fn replay_asset(symbol: &str) -> Result<&'static Asset> {
    let asset = assets()?.get(symbol).ok_or_else(|| {
        eyre!(
            "cTrader native symbol {symbol:?} has no audited non-stock underlying; \
             stocks, ETFs and unverified aliases are not onboarded for replay"
        )
    })?;
    tracing::debug!(
        symbol,
        underlying = asset.id,
        "resolved audited cTrader native underlying"
    );
    Ok(asset)
}

/// Native broker names, including spaces/punctuation, suitable for explicit reads.
pub fn non_stock_symbols() -> Result<Vec<&'static str>> {
    Ok(assets()?.keys().map(String::as_str).collect())
}

#[cfg(test)]
mod tests;
