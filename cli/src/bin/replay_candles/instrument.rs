//! Resolve a user-supplied instrument string into the per-broker symbol the
//! chosen candle source needs.
//!
//! The engine plan stores a canonical/OANDA-ish symbol (`EUR_CAD`), but the two
//! candle feeds key differently: candle-cache over OANDA wants `EUR_CAD`, while
//! `TradeNationClient` resolves candles by its MarketName (`EUR/CAD`). We lean
//! on `instrument-lookup` (the single source of truth) rather than rolling a
//! map, mirroring the existing `cli::calendar_bars::parse_instrument` pattern.

use color_eyre::eyre::{Result, eyre};
use instrument_lookup::{Broker, by_broker_symbol, resolve};

use trade_control_cli::replay_args::CandleSource;

/// Resolve `raw` (e.g. `eur/cad`, `EUR_CAD`, `EURCAD`) to the broker symbol the
/// `source` feed expects. Tries a direct broker-symbol hit first, then the
/// general catalog resolver, exactly like `parse_instrument`.
pub fn resolve_for(raw: &str, source: CandleSource) -> Result<String> {
    let asset = by_broker_symbol(broker_of(source), raw)
        .map_err(|e| eyre!("instrument-lookup overlay error resolving {raw:?}: {e}"))?
        .or(resolve(raw)
            .map_err(|e| eyre!("instrument-lookup overlay error resolving {raw:?}: {e}"))?)
        .ok_or_else(|| {
            eyre!(
                "unsupported instrument {raw:?}: not in the instrument-lookup catalog. \
                 Add an `[[asset]]` entry to ~/.config/instrument-lookup/mappings.toml."
            )
        })?;

    let broker = broker_of(source);
    asset.symbol_for(broker).map(str::to_string).ok_or_else(|| {
        eyre!(
            "instrument {raw:?} ({}) is not listed on {broker} — pick the other --source \
             or add the symbol to the catalog",
            asset.id
        )
    })
}

fn broker_of(source: CandleSource) -> Broker {
    match source {
        CandleSource::Oanda => Broker::Oanda,
        CandleSource::TradeNation => Broker::TradeNation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_eur_cad_per_source() {
        assert_eq!(
            resolve_for("eur/cad", CandleSource::Oanda).unwrap(),
            "EUR_CAD"
        );
        assert_eq!(
            resolve_for("EUR_CAD", CandleSource::TradeNation).unwrap(),
            "EUR/CAD"
        );
    }

    #[test]
    fn unknown_instrument_is_an_error() {
        assert!(resolve_for("not-a-real-pair", CandleSource::Oanda).is_err());
    }
}
