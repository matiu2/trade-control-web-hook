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

use instrument_lookup::{Asset, AssetClass, Instrument, pip_size_from};
use tracing::warn;
use trading_view::symbol_info::SymbolInfo;

/// The catalog's own view of an instrument's precision — the fallback layer
/// beneath live TradingView. Sourced from the native per-broker
/// [`Instrument`] (preferred) or the legacy [`Asset`] (single-tick), so
/// `resolve_effective_precision` doesn't care which structure supplied it.
#[derive(Debug, Clone, Copy)]
pub struct CatalogPrecision {
    pub tick_size: f64,
    pub pip_size: f64,
    pub decimal_places: u8,
    pub class: AssetClass,
    /// Contract multiplier for a futures series (money per 1.0 of price), or
    /// `None` for a spot/CFD instrument, which is sized in units.
    ///
    /// ⚠️ **Deliberately not taken from TradingView.** Unlike tick, where the
    /// live chart is the source of truth, the multiplier comes only from the
    /// catalog: TV's `point_value` is a CFD per-point value for the *chart's*
    /// instrument, not the exchange contract size, and letting it win would
    /// mis-size a futures order by whatever the chart happened to report.
    pub contract_multiplier: Option<f64>,
}

impl CatalogPrecision {
    /// Per-broker precision from the native instrument-primary catalog row —
    /// the correct source (OANDA and TradeNation legs can genuinely differ).
    pub fn from_instrument(i: &Instrument) -> Self {
        Self {
            tick_size: i.tick_size,
            pip_size: i.pip_size,
            decimal_places: i.decimal_places,
            class: i.class,
            // `point_value` is the multiplier on a futures leg (the catalog
            // populates it from the series' `FuturesSpec`) and `None` on an
            // un-audited spot leg. Both map straight onto our `Option`.
            contract_multiplier: i.point_value,
        }
    }

    /// Legacy single-tick precision from an [`Asset`]. Used only when the
    /// native catalog has no row for this (broker, symbol).
    pub fn from_asset(a: &Asset) -> Self {
        Self {
            tick_size: a.tick_size,
            pip_size: a.pip_size,
            decimal_places: a.decimal_places,
            class: a.class,
            // `a.futures` is `Some` only on the four futures series, so a spot
            // asset yields `None` and nothing is baked onto its intent. This
            // is the path futures actually take: they have no native
            // instrument row (no per-month TradingView key exists), so
            // `precision_for` falls back here.
            contract_multiplier: a.futures.map(|f| f.multiplier),
        }
    }
}

/// The pip and tick tv-arm will bake onto the intent, plus where each came
/// from (for logging / tests).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectivePrecision {
    pub pip_size: f64,
    pub tick_size: f64,
    /// True when the tick was taken from live TradingView (vs the catalog).
    pub tick_from_tv: bool,
    /// Contract multiplier, passed through from the catalog unchanged — see
    /// [`CatalogPrecision::contract_multiplier`] for why TradingView never
    /// overrides it. `None` for spot/CFD.
    pub contract_multiplier: Option<f64>,
}

impl EffectivePrecision {
    /// The catalog's precision, used as-is — the answer whenever live
    /// TradingView Symbol-info is unavailable (no chart on a `--spec-in`
    /// re-arm, or tv-mcp unreachable).
    ///
    /// A constructor rather than three hand-written struct literals: every
    /// caller previously had to remember each field, so a new one (the
    /// multiplier) would have been silently dropped on the fallback paths —
    /// and futures *always* take a fallback path, since they have no native
    /// instrument row. Adding a field here is now a single edit.
    pub fn from_catalog(cat: CatalogPrecision) -> Self {
        Self {
            pip_size: cat.pip_size,
            tick_size: cat.tick_size,
            tick_from_tv: false,
            contract_multiplier: cat.contract_multiplier,
        }
    }
}

/// Decide the effective pip/tick from the catalog precision and the live
/// chart Symbol-info. TV wins when it supplied a usable tick; otherwise the
/// catalog value stands. A mismatch between the two is logged at WARN.
///
/// Pip is derived from TV's tick via the catalog's own `pip_size_from` rule
/// (keyed on the class), so a live tick still yields a class-correct pip —
/// TV's popup exposes tick, not the sizing pip, and re-deriving keeps the
/// fractional-pip FX / index-point conventions intact.
pub fn resolve_effective_precision(cat: CatalogPrecision, tv: &SymbolInfo) -> EffectivePrecision {
    let catalog_tick = cat.tick_size;

    match tv.tick_size {
        Some(tv_tick) if tv_tick.is_finite() && tv_tick > 0.0 => {
            if !ticks_match(tv_tick, catalog_tick) {
                warn!(
                    catalog_tick,
                    tv_tick,
                    tv_key = tv.pro_name.as_deref().unwrap_or(&tv.full_name),
                    "tick mismatch: catalog disagrees with live TradingView; \
                     using the live TV value (catalog may be stale)"
                );
            }
            // Re-derive pip from the live tick using the class rule, so a
            // corrected tick also corrects the sizing pip.
            let tv_dp = tv.decimal_places.unwrap_or(cat.decimal_places);
            let tv_pip = pip_size_from(cat.class, tv_dp, tv_tick);
            EffectivePrecision {
                pip_size: tv_pip,
                tick_size: tv_tick,
                tick_from_tv: true,
                contract_multiplier: cat.contract_multiplier,
            }
        }
        _ => {
            // TV gave no usable numeric tick (older build, or a symbol it
            // couldn't fully resolve) — fall back to the catalog.
            EffectivePrecision::from_catalog(cat)
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
            spread_schedule: "none".into(),
            futures: None,
            symbols: AssetSymbols {
                oanda: Some("TEST".into()),
                tradenation: None,
                tradingview: Some("TEST".into()),
                ibkr: None,
            },
        }
    }

    /// A futures series asset, shaped like the catalog's ES row.
    fn futures_asset(multiplier: f64) -> Asset {
        let mut a = asset(AssetClass::Index, 0.25, 2, 1.0);
        a.id = "ES".into();
        a.futures = Some(instrument_lookup::FuturesSpec { multiplier });
        a.symbols.ibkr = Some("ES".into());
        a
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
        let eff = resolve_effective_precision(CatalogPrecision::from_asset(&a), &tv);
        assert_eq!(eff.tick_size, 0.1);
        assert!(eff.tick_from_tv);
        // Index pip is always 1.0 regardless of tick.
        assert_eq!(eff.pip_size, 1.0);
    }

    #[test]
    fn falls_back_to_catalog_when_tv_has_no_tick() {
        let a = asset(AssetClass::Forex, 0.00001, 5, 0.0001);
        let tv = tv_info(None, None); // old-build payload
        let eff = resolve_effective_precision(CatalogPrecision::from_asset(&a), &tv);
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
        let eff = resolve_effective_precision(CatalogPrecision::from_asset(&a), &tv);
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
        let eff = resolve_effective_precision(CatalogPrecision::from_asset(&a), &tv);
        assert_eq!(eff.tick_size, 0.001);
        assert_eq!(eff.pip_size, 0.01); // corrected, not the wrong 0.5
    }

    #[test]
    fn a_spot_asset_carries_no_multiplier() {
        let a = asset(AssetClass::Forex, 0.00001, 5, 0.0001);
        let cat = CatalogPrecision::from_asset(&a);
        assert_eq!(cat.contract_multiplier, None);
        let eff = resolve_effective_precision(cat, &tv_info(Some(0.00001), Some(5)));
        assert_eq!(eff.contract_multiplier, None);
    }

    #[test]
    fn a_futures_asset_carries_its_catalog_multiplier() {
        let a = futures_asset(50.0);
        let cat = CatalogPrecision::from_asset(&a);
        assert_eq!(cat.contract_multiplier, Some(50.0));
        let eff = resolve_effective_precision(cat, &tv_info(Some(0.25), Some(2)));
        assert_eq!(eff.contract_multiplier, Some(50.0));
    }

    #[test]
    fn tradingview_never_overrides_the_multiplier() {
        // TV reports `point_value: 1.0` for this chart (see `tv_info`), which
        // is a CFD per-point value and wrong for an exchange contract. The
        // catalog multiplier must survive it — TV wins on tick, never here.
        let a = futures_asset(50.0);
        let tv = tv_info(Some(0.25), Some(2));
        assert_eq!(tv.point_value, Some(1.0), "fixture must exercise the clash");
        let eff = resolve_effective_precision(CatalogPrecision::from_asset(&a), &tv);
        assert!(eff.tick_from_tv, "tick still comes from TV");
        assert_eq!(
            eff.contract_multiplier,
            Some(50.0),
            "TV point_value must not override the catalog multiplier"
        );
    }

    #[test]
    fn the_multiplier_survives_the_catalog_fallback_path() {
        // No usable TV tick: the whole precision falls back to the catalog.
        // The multiplier must come through that arm too, since futures have no
        // native instrument row and always take a fallback path.
        let a = futures_asset(100.0);
        let eff =
            resolve_effective_precision(CatalogPrecision::from_asset(&a), &tv_info(None, None));
        assert!(!eff.tick_from_tv);
        assert_eq!(eff.contract_multiplier, Some(100.0));
    }

    #[test]
    fn zero_or_nonfinite_tv_tick_falls_back() {
        let a = asset(AssetClass::Forex, 0.00001, 5, 0.0001);
        for bad in [Some(0.0), Some(f64::NAN), Some(-0.1)] {
            let tv = tv_info(bad, Some(5));
            let eff = resolve_effective_precision(CatalogPrecision::from_asset(&a), &tv);
            assert_eq!(eff.tick_size, 0.00001, "bad tv tick {bad:?} → catalog");
            assert!(!eff.tick_from_tv);
        }
    }
}
