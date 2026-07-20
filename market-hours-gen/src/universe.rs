//! The instrument universe — which `(venue, symbol)` pairs to profile, sourced
//! from `instrument-lookup`'s catalog.
//!
//! Each catalog `Asset` carries its OANDA and TradeNation symbols (or `None`
//! when a venue doesn't list it) and its class. We emit one work item per
//! `(venue, symbol)` the venue actually lists, FX/metals/indices first so the
//! instruments we trade surface first in the run log.

use instrument_lookup::{Asset, AssetClass, Broker as IlBroker};

use crate::Venue;

/// One instrument to profile: which venue, its venue-native symbol, its class
/// (for ordering + reporting), and a display name.
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub venue: Venue,
    pub symbol: String,
    pub class: AssetClass,
    pub display_name: String,
}

/// Ordering rank for a class — lower runs first (FX/metals/indices we trade,
/// then bonds/commodities/crypto, stocks last).
fn class_rank(class: AssetClass) -> u8 {
    match class {
        AssetClass::Forex => 0,
        AssetClass::Gold => 1,
        AssetClass::Commodity => 2,
        AssetClass::Index => 3,
        AssetClass::Bond => 4,
        AssetClass::Crypto => 5,
        AssetClass::Stock => 6,
    }
}

/// Build the ordered work list for the requested venues from the catalog.
///
/// `include_stocks` gates the `Stock` class (part-time exchanges, gappy candles
/// — off by default). An asset contributes one work item per venue that lists
/// it; a venue with no symbol for the asset is skipped (no canonical sharing).
pub fn work_items(assets: &[Asset], venues: &[Venue], include_stocks: bool) -> Vec<WorkItem> {
    let mut items = Vec::new();
    for asset in assets {
        if asset.class == AssetClass::Stock && !include_stocks {
            continue;
        }
        for &venue in venues {
            let il_broker = match venue {
                Venue::Oanda => IlBroker::Oanda,
                Venue::TradeNation => IlBroker::TradeNation,
            };
            let Some(sym) = asset.symbol_for(il_broker) else {
                continue;
            };
            if sym.is_empty() {
                continue;
            }
            items.push(WorkItem {
                venue,
                symbol: sym.to_string(),
                class: asset.class,
                display_name: asset.display_name.clone(),
            });
        }
    }
    items.sort_by(|a, b| {
        class_rank(a.class)
            .cmp(&class_rank(b.class))
            .then_with(|| a.venue.as_str().cmp(b.venue.as_str()))
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
