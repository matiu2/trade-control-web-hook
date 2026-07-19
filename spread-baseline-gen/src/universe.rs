//! The instrument universe — which (broker, symbol) pairs to profile, in what
//! order, sourced from `instrument-lookup`'s catalog.
//!
//! Each catalog `Asset` carries its OANDA and TradeNation symbols (or `None`
//! when a broker doesn't list it) and its class. We emit one work item per
//! (broker, symbol) the broker actually lists, ordered so the instruments we
//! trade — FX, metals, 24h indices — come first and thinly-traded stocks last
//! (their part-time-exchange candles may have gaps; Stage 1 does them last or
//! defers them).

use instrument_lookup::{Asset, AssetClass, Broker as IlBroker};

use crate::Broker;

/// One instrument to profile: which broker, its broker-native symbol, its
/// class (for ordering + reporting), and a display name.
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub broker: Broker,
    pub symbol: String,
    pub class: AssetClass,
    pub display_name: String,
    /// The asset's `spread_schedule` FK name (e.g. `"ny"`, `"asx"`, `"none"`).
    /// Generate.rs resolves this to an IANA tz for local-hour bucketing and
    /// emits it as a table column so Stage 3 can bake it.
    pub spread_schedule: String,
    /// The IANA tz id for this asset's schedule, or `None` for `none`/unknown.
    /// `None` ⇒ the asset has no spread hour and is skipped for profiling.
    pub spread_schedule_tz: Option<String>,
    /// The asset's pip size (from instrument-lookup). Used to convert each
    /// minute's dimensionless `spread_frac` into the pips magnitude the live
    /// reject-gate baseline wants. `0.0` when unavailable ⇒ the profile emits
    /// 0.0 pips and the gate falls back to its flat cutoff.
    pub pip_size: f64,
}

/// Ordering rank for a class — lower runs first. FX/metals/24h-indices are what
/// we trade and have clean round-the-clock candles; stocks last (part-time
/// exchanges, gappy candles). Bonds/commodities/crypto in the middle.
fn class_rank(class: AssetClass) -> u8 {
    match class {
        AssetClass::Forex => 0,
        AssetClass::Gold => 1,
        AssetClass::Commodity => 2, // includes spot metals (silver)
        AssetClass::Index => 3,
        AssetClass::Crypto => 4,
        AssetClass::Bond => 5,
        AssetClass::Stock => 6,
    }
}

/// Build the ordered work list for the requested brokers from the catalog.
///
/// `include_stocks` gates the `Stock` class (Stage 1 defers them by default).
/// An asset contributes a work item per broker that lists it; an asset with no
/// symbol for a broker is simply skipped for that broker (no canonical
/// sharing).
pub fn work_items(assets: &[Asset], brokers: &[Broker], include_stocks: bool) -> Vec<WorkItem> {
    let mut items = Vec::new();
    for asset in assets {
        if asset.class == AssetClass::Stock && !include_stocks {
            continue;
        }
        for &broker in brokers {
            let il_broker = match broker {
                Broker::Oanda => IlBroker::Oanda,
                Broker::TradeNation => IlBroker::TradeNation,
            };
            let Some(sym) = asset.symbol_for(il_broker) else {
                continue;
            };
            if sym.is_empty() {
                continue;
            }
            items.push(WorkItem {
                broker,
                symbol: sym.to_string(),
                class: asset.class,
                display_name: asset.display_name.clone(),
                spread_schedule: asset.spread_schedule.clone(),
                spread_schedule_tz: asset.spread_schedule_tz(),
                pip_size: asset.pip_size,
            });
        }
    }
    items.sort_by(|a, b| {
        class_rank(a.class)
            .cmp(&class_rank(b.class))
            .then_with(|| a.broker.as_str().cmp(b.broker.as_str()))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_rank_orders_fx_before_stock() {
        assert!(class_rank(AssetClass::Forex) < class_rank(AssetClass::Stock));
        assert!(class_rank(AssetClass::Gold) < class_rank(AssetClass::Index));
    }
}
