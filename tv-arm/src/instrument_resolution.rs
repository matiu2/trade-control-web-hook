//! Resolve a chart's TradingView symbol into the canonical broker-
//! specific symbol via the `instrument-lookup` catalog, and synthesize
//! a `trade-calendar-maker` `Instrument` for the calendar-bars planner.
//!
//! This module is the single seam where `tv-arm` consults the shared
//! instrument catalog. It replaces ad-hoc symbol parsing (the old
//! `cli::parse_instrument` FX-only path) and the silent
//! TV-symbol → broker-symbol redirect that `cli::validate_instrument`
//! used to do behind our back.
//!
//! Two failure modes both surface as hard errors:
//!
//! 1. The chart's symbol isn't in the catalog at all. Error includes
//!    the user-overlay path so the operator can add an `[[asset]]`
//!    entry without rebuilding.
//! 2. The asset is in the catalog but isn't listed on the broker the
//!    operator chose (e.g. `--broker tradenation` on a chart for an
//!    OANDA-only gilt). Error lists which brokers DO carry it.

use color_eyre::eyre::{Result, eyre};
use instrument_lookup::{Asset, AssetClass, Broker as IlBroker};
use trade_calendar_maker::types::{Instrument as TcmInstrument, InstrumentType};
use trade_control_conventions::Broker as ConvBroker;

use crate::precision::CatalogPrecision;

/// One chart-symbol → broker-canonical-symbol resolution result.
///
/// Holds a static reference back into the catalog (the catalog is
/// loaded once into a `LazyLock` inside `instrument-lookup`) so all
/// downstream code can read news currencies, class, etc. without
/// re-querying.
#[derive(Debug, Clone)]
pub struct ResolvedInstrument {
    /// The catalog entry the chart's symbol resolved to.
    pub asset: &'static Asset,
    /// The asset's symbol on the chosen broker — what gets passed to
    /// `cli::build_trade_from_spec` (e.g. `"EUR/USD"` for TradeNation,
    /// `"EUR_USD"` for OANDA).
    pub broker_symbol: String,
    /// The **per-broker** catalog precision for the chosen broker's leg,
    /// from the native instrument-primary catalog. This is the correct
    /// fallback beneath live TradingView: OANDA and TradeNation legs of the
    /// same underlying can genuinely tick differently (AU200 OANDA 0.1 vs TN
    /// 1.0). Falls back to the legacy single-tick `Asset` precision only when
    /// the native catalog has no row for this (broker, symbol).
    pub precision: CatalogPrecision,
}

/// Resolve a TV-form symbol (e.g. `"TRADENATION:EURUSD"`, `"OANDA:EUR_USD"`,
/// or a bare `"EURUSD"`) against the chosen broker.
///
/// Uses `instrument_lookup::resolve` so operator typos on the chart's
/// symbol field (slash form, underscore form, display name) all
/// converge to the same `Asset`.
pub fn resolve_for_broker(tv_symbol: &str, broker: ConvBroker) -> Result<ResolvedInstrument> {
    let bare = strip_exchange(tv_symbol);
    let asset = instrument_lookup::resolve(bare)?.ok_or_else(|| {
        let hint = instrument_lookup::user_config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.config/instrument-lookup/mappings.toml".to_string());
        eyre!(
            "chart symbol {tv_symbol:?} is not in the instrument-lookup catalog. \
             Add an `[[asset]]` entry to {hint} to teach tv-arm about it.",
        )
    })?;

    let il_broker = to_il_broker(broker);
    let broker_symbol = asset.symbol_for(il_broker).ok_or_else(|| {
        let carriers = brokers_carrying(asset);
        let listed = if carriers.is_empty() {
            "no broker in the catalog lists it".to_string()
        } else {
            format!("listed on: {}", carriers.join(", "))
        };
        eyre!(
            "asset {} is not listed on {} ({})",
            asset.id,
            broker.as_str(),
            listed,
        )
    })?;

    let precision = precision_for(il_broker, broker_symbol, asset);

    Ok(ResolvedInstrument {
        asset,
        broker_symbol: broker_symbol.to_string(),
        precision,
    })
}

/// Resolve the per-broker precision for this leg from the native
/// instrument-primary catalog, falling back to the legacy `Asset`'s
/// single-tick precision when the native catalog has no matching row (a
/// build predating the data-fill, or a symbol only in the legacy overlay).
fn precision_for(broker: IlBroker, broker_symbol: &str, asset: &Asset) -> CatalogPrecision {
    match instrument_lookup::resolve_for(broker, broker_symbol) {
        Ok(Some(inst)) => CatalogPrecision::from_instrument(inst),
        _ => CatalogPrecision::from_asset(asset),
    }
}

/// Build a `trade-calendar-maker::Instrument` from an `Asset` so the
/// calendar-bars planner (which takes a tcm `Instrument`) can read
/// `affected_currencies` without parsing the symbol itself.
///
/// The `instrument_type` field is cosmetic for our hot path
/// (`plan_calendar_bars` only reads `affected_currencies`) but we map
/// it accurately for any future consumer.
pub fn synthesize_calendar_instrument(asset: &Asset) -> TcmInstrument {
    // Prefer OANDA's symbol as the carried-around name (tcm was
    // designed around OANDA symbols); fall back to the canonical id
    // when the asset isn't listed on OANDA.
    let symbol = asset
        .symbols
        .oanda
        .clone()
        .unwrap_or_else(|| asset.id.clone());
    let instrument_type = map_class(asset.class);
    TcmInstrument::new(symbol, instrument_type, asset.news_currencies.clone())
}

fn strip_exchange(tv_symbol: &str) -> &str {
    match tv_symbol.split_once(':') {
        Some((_, sym)) => sym,
        None => tv_symbol,
    }
}

fn to_il_broker(broker: ConvBroker) -> IlBroker {
    match broker {
        ConvBroker::Oanda => IlBroker::Oanda,
        ConvBroker::TradeNation => IlBroker::TradeNation,
    }
}

fn brokers_carrying(asset: &Asset) -> Vec<&'static str> {
    [
        IlBroker::Oanda,
        IlBroker::TradeNation,
        IlBroker::TradingView,
    ]
    .into_iter()
    .filter(|b| asset.symbol_for(*b).is_some())
    .map(broker_label)
    .collect()
}

fn broker_label(b: IlBroker) -> &'static str {
    match b {
        IlBroker::Oanda => "oanda",
        IlBroker::TradeNation => "tradenation",
        IlBroker::TradingView => "tradingview",
    }
}

fn map_class(class: AssetClass) -> InstrumentType {
    match class {
        AssetClass::Forex => InstrumentType::Forex,
        AssetClass::Index => InstrumentType::Index,
        AssetClass::Gold => InstrumentType::Gold,
        AssetClass::Bond => InstrumentType::Bond,
        AssetClass::Commodity => InstrumentType::Commodity,
        // tcm doesn't model crypto or single-stock; fall back to
        // Commodity since the field isn't read in our path. If a
        // future consumer reads it, revisit then.
        AssetClass::Crypto | AssetClass::Stock => InstrumentType::Commodity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_eurusd_to_oanda_canonical() {
        let r = resolve_for_broker("OANDA:EURUSD", ConvBroker::Oanda).expect("resolves");
        assert_eq!(r.asset.id, "EURUSD");
        assert_eq!(r.broker_symbol, "EUR_USD");
    }

    #[test]
    fn resolves_eurusd_to_tn_canonical() {
        let r =
            resolve_for_broker("TRADENATION:EURUSD", ConvBroker::TradeNation).expect("resolves");
        assert_eq!(r.asset.id, "EURUSD");
        assert_eq!(r.broker_symbol, "EUR/USD");
    }

    #[test]
    fn au200_oanda_leg_carries_native_per_broker_tick() {
        // The PRICE_PRECISION_EXCEEDED instrument, and the whole reason for
        // this migration. The legacy `resolve()` lands on a *duplicate*
        // `AU200AUD` Asset row that carries the WRONG class-default tick 1.0
        // (that's what sent OANDA a 5-decimal price it rejected). The
        // per-broker precision must instead come from the native instrument
        // catalog, keyed off the broker order symbol — tick 0.1.
        let r = resolve_for_broker("OANDA:AU200AUD", ConvBroker::Oanda).expect("resolves");
        assert_eq!(r.broker_symbol, "AU200_AUD");
        // Precision is the native per-broker value, NOT the legacy Asset's.
        assert_eq!(r.precision.tick_size, 0.1, "OANDA AU200 ticks in 0.1");
        assert_eq!(r.precision.pip_size, 1.0, "index sizes on a whole point");
        // Prove the native lookup actually corrected a wrong legacy tick:
        // the Asset it resolved to still carries the stale value.
        assert_ne!(
            r.asset.tick_size, r.precision.tick_size,
            "native precision must override the legacy Asset's wrong tick"
        );
    }

    #[test]
    fn resolves_smi_to_tn_canonical() {
        let r = resolve_for_broker("TRADENATION:SMI", ConvBroker::TradeNation).expect("resolves");
        assert_eq!(r.asset.id, "CH20");
        assert_eq!(r.broker_symbol, "Switzerland 20");
        assert!(r.asset.is_affected_by("CHF"));
        assert!(r.asset.is_affected_by("EUR"));
    }

    #[test]
    fn resolves_underscore_form_typo() {
        // Operator typed EUR_USD into a TN chart's symbol field
        // — `resolve()` finds it via the OANDA-symbol column anyway.
        let r =
            resolve_for_broker("TRADENATION:EUR_USD", ConvBroker::TradeNation).expect("resolves");
        assert_eq!(r.asset.id, "EURUSD");
        assert_eq!(r.broker_symbol, "EUR/USD");
    }

    #[test]
    fn resolves_bare_symbol_without_exchange() {
        let r = resolve_for_broker("EURUSD", ConvBroker::Oanda).expect("resolves");
        assert_eq!(r.asset.id, "EURUSD");
    }

    #[test]
    fn unknown_symbol_hard_errors_with_overlay_hint() {
        let err = resolve_for_broker("TRADENATION:NOPE_XYZ_NOTREAL", ConvBroker::TradeNation)
            .expect_err("must error");
        let msg = format!("{err}");
        assert!(
            msg.contains("not in the instrument-lookup catalog"),
            "msg = {msg}"
        );
        assert!(msg.contains("mappings.toml"), "msg = {msg}");
    }

    #[test]
    fn asset_not_listed_on_broker_hard_errors() {
        // UK10YB is OANDA-only in the baseline.
        let err = resolve_for_broker("UK10YB", ConvBroker::TradeNation).expect_err("must error");
        let msg = format!("{err}");
        assert!(msg.contains("not listed on tradenation"), "msg = {msg}");
        assert!(msg.contains("oanda"), "msg should name OANDA: {msg}");
    }

    #[test]
    fn synthesize_smi_has_chf_and_eur() {
        let asset = instrument_lookup::resolve("SMI")
            .expect("ok")
            .expect("found");
        let inst = synthesize_calendar_instrument(asset);
        assert!(inst.is_affected_by("CHF"));
        assert!(inst.is_affected_by("EUR"));
        assert!(!inst.is_affected_by("JPY"));
    }

    #[test]
    fn synthesize_eurusd_has_both_legs() {
        let asset = instrument_lookup::resolve("EURUSD")
            .expect("ok")
            .expect("found");
        let inst = synthesize_calendar_instrument(asset);
        assert!(inst.is_affected_by("EUR"));
        assert!(inst.is_affected_by("USD"));
        assert!(!inst.is_affected_by("JPY"));
    }

    #[test]
    fn synthesize_xauusd_has_xau_and_usd() {
        let asset = instrument_lookup::resolve("XAUUSD")
            .expect("ok")
            .expect("found");
        let inst = synthesize_calendar_instrument(asset);
        assert!(inst.is_affected_by("XAU"));
        assert!(inst.is_affected_by("USD"));
    }

    #[test]
    fn strip_exchange_handles_no_prefix() {
        assert_eq!(strip_exchange("EURUSD"), "EURUSD");
        assert_eq!(strip_exchange("TRADENATION:EURUSD"), "EURUSD");
        assert_eq!(strip_exchange("OANDA:EUR_USD"), "EUR_USD");
    }
}
