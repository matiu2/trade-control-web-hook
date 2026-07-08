//! Effective pip/tick resolution at arm time.
//!
//! The instrument-lookup catalog is the fallback, but the **live TradingView
//! Symbol-info** is the source of truth for the instrument you're actually
//! arming: it's the same tick/pip the chart shows you, read straight from
//! `symbolInfoWV()`. So tv-arm prefers the live value and treats the catalog
//! as a cross-check — warning loudly when they disagree so a stale catalog
//! entry surfaces the moment you trade that instrument, without blocking the
//! trade.
//!
//! Precedence (highest first): explicit `--pip-size` / `--tick-size` flag >
//! live TradingView Symbol-info > instrument-lookup catalog. The flags stay
//! the operator's manual escape hatch; this module only decides the
//! TV-vs-catalog layer beneath them.

use instrument_lookup::{Asset, pip_size_from};
use tracing::warn;
use trading_view::symbol_info::SymbolInfo;

/// The pip and tick tv-arm will bake onto the intent, plus where each came
/// from (for logging / tests).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectivePrecision {
    pub pip_size: f64,
    pub tick_size: f64,
    /// True when the tick was taken from live TradingView (vs the catalog).
    pub tick_from_tv: bool,
}

/// Decide the effective pip/tick from the catalog asset and the live chart
/// Symbol-info. TV wins when it supplied a usable tick; otherwise the
/// catalog value stands. A mismatch between the two is logged at WARN.
///
/// Pip is derived from TV's tick via the catalog's own `pip_size_from` rule
/// (keyed on the asset class), so a live tick still yields a class-correct
/// pip — TV's popup exposes tick, not the sizing pip, and re-deriving keeps
/// the fractional-pip FX / index-point conventions intact.
pub fn resolve_effective_precision(asset: &Asset, tv: &SymbolInfo) -> EffectivePrecision {
    let catalog_tick = asset.tick_size;
    let catalog_pip = asset.pip_size;

    match tv.tick_size {
        Some(tv_tick) if tv_tick.is_finite() && tv_tick > 0.0 => {
            if !ticks_match(tv_tick, catalog_tick) {
                warn!(
                    asset = %asset.id,
                    catalog_tick,
                    tv_tick,
                    tv_key = tv.pro_name.as_deref().unwrap_or(&tv.full_name),
                    "tick mismatch: catalog disagrees with live TradingView; \
                     using the live TV value (catalog may be stale)"
                );
            }
            // Re-derive pip from the live tick using the class rule, so a
            // corrected tick also corrects the sizing pip.
            let tv_dp = tv.decimal_places.unwrap_or(asset.decimal_places);
            let tv_pip = pip_size_from(asset.class, tv_dp, tv_tick);
            EffectivePrecision {
                pip_size: tv_pip,
                tick_size: tv_tick,
                tick_from_tv: true,
            }
        }
        _ => {
            // TV gave no usable numeric tick (older build, or a symbol it
            // couldn't fully resolve) — fall back to the catalog.
            EffectivePrecision {
                pip_size: catalog_pip,
                tick_size: catalog_tick,
                tick_from_tv: false,
            }
        }
    }
}

/// Ticks are floats; treat them equal within a tiny relative epsilon so
/// `0.00001` from two sources doesn't spuriously "mismatch".
fn ticks_match(a: f64, b: f64) -> bool {
    let scale = a.abs().max(b.abs()).max(1e-12);
    (a - b).abs() <= scale * 1e-9
}

#[cfg(test)]
mod tests {
    use super::*;
    use instrument_lookup::{AssetClass, AssetSymbols};

    fn asset(class: AssetClass, tick: f64, dp: u8, pip: f64) -> Asset {
        Asset {
            id: "TEST".into(),
            class,
            display_name: "Test".into(),
            description: "Test asset".into(),
            news_currencies: vec!["USD".into()],
            tick_size: tick,
            decimal_places: dp,
            pip_size: pip,
            symbols: AssetSymbols {
                oanda: Some("TEST".into()),
                tradenation: None,
                tradingview: Some("TEST".into()),
            },
        }
    }

    fn tv_info(tick: Option<f64>, dp: Option<u8>) -> SymbolInfo {
        SymbolInfo {
            symbol: "TEST".into(),
            full_name: "OANDA:TEST".into(),
            exchange: "OANDA".into(),
            description: "Test".into(),
            asset_type: "forex".into(),
            pro_name: Some("OANDA:TEST".into()),
            tick_size: tick,
            decimal_places: dp,
            point_value: Some(1.0),
            currency_code: Some("USD".into()),
            session: None,
            timezone: None,
            listed_exchange: Some("OANDA".into()),
        }
    }

    #[test]
    fn tv_tick_wins_when_present() {
        // Catalog says index tick 1.0 (the AU200 bug); TV says 0.1.
        let a = asset(AssetClass::Index, 1.0, 0, 1.0);
        let tv = tv_info(Some(0.1), Some(1));
        let eff = resolve_effective_precision(&a, &tv);
        assert_eq!(eff.tick_size, 0.1);
        assert!(eff.tick_from_tv);
        // Index pip is always 1.0 regardless of tick.
        assert_eq!(eff.pip_size, 1.0);
    }

    #[test]
    fn falls_back_to_catalog_when_tv_has_no_tick() {
        let a = asset(AssetClass::Forex, 0.00001, 5, 0.0001);
        let tv = tv_info(None, None); // old-build payload
        let eff = resolve_effective_precision(&a, &tv);
        assert_eq!(eff.tick_size, 0.00001);
        assert_eq!(eff.pip_size, 0.0001);
        assert!(!eff.tick_from_tv);
    }

    #[test]
    fn matching_ticks_still_take_tv_but_no_warn_semantics() {
        // Agreement: TV tick == catalog tick; TV still wins (source of
        // truth), pip re-derived identically.
        let a = asset(AssetClass::Forex, 0.00001, 5, 0.0001);
        let tv = tv_info(Some(0.00001), Some(5));
        let eff = resolve_effective_precision(&a, &tv);
        assert_eq!(eff.tick_size, 0.00001);
        assert_eq!(eff.pip_size, 0.0001);
        assert!(eff.tick_from_tv);
    }

    #[test]
    fn fractional_pip_fx_pip_rederived_from_tv_tick() {
        // A 3-dp JPY pair: TV tick 0.001 → pip 0.01 (10x), even if the
        // catalog pip were wrong.
        let a = asset(
            AssetClass::Forex,
            0.001,
            3,
            0.5, /* wrong catalog pip */
        );
        let tv = tv_info(Some(0.001), Some(3));
        let eff = resolve_effective_precision(&a, &tv);
        assert_eq!(eff.tick_size, 0.001);
        assert_eq!(eff.pip_size, 0.01); // corrected, not the wrong 0.5
    }

    #[test]
    fn zero_or_nonfinite_tv_tick_falls_back() {
        let a = asset(AssetClass::Forex, 0.00001, 5, 0.0001);
        for bad in [Some(0.0), Some(f64::NAN), Some(-0.1)] {
            let tv = tv_info(bad, Some(5));
            let eff = resolve_effective_precision(&a, &tv);
            assert_eq!(eff.tick_size, 0.00001, "bad tv tick {bad:?} → catalog");
            assert!(!eff.tick_from_tv);
        }
    }
}
