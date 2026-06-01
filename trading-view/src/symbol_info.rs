//! Serde shape for `tv info` (tv-mcp's `chart.symbolExt()` wrapper).
//!
//! Used by tv-arm to recover when the chart's bare symbol isn't in the
//! instrument-lookup catalog: the `description` field carries the
//! broker's own name for the asset (e.g. `"ALPHABET"` for the chart
//! `TRADENATION:GOOGL`), which is what the TradeNation column of the
//! catalog uses. Looking up by `description` finds the asset; the
//! original chart symbol can then be saved into the overlay's
//! `tradingview` field so future runs resolve directly.

use serde::Deserialize;

/// What `tv info` returns. Only the fields tv-arm actually reads are
/// modelled; everything else (`pro_name`, `typespecs`, `resolution`,
/// `chart_type`) is dropped on deserialization.
///
/// Example payload (real, captured from a TradeNation Google chart):
/// ```json
/// {
///   "success": true,
///   "symbol": "GOOGL",
///   "full_name": "TRADENATION:GOOGL",
///   "exchange": "Trade Nation",
///   "description": "ALPHABET",
///   "type": "stock"
/// }
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct SymbolInfo {
    /// Bare TV symbol — same as the suffix of `full_name` (`"GOOGL"`).
    pub symbol: String,
    /// `"EXCHANGE:SYMBOL"` form (`"TRADENATION:GOOGL"`).
    pub full_name: String,
    /// Human-readable exchange name (`"Trade Nation"`, `"OANDA"`).
    pub exchange: String,
    /// The broker's own name for the asset — this is what the
    /// TradeNation / OANDA columns of the catalog typically match.
    pub description: String,
    /// `"stock"`, `"forex"`, `"index"`, `"commodity"`, ...
    #[serde(rename = "type")]
    pub asset_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_googl_payload() {
        let raw = r#"{
            "success": true,
            "symbol": "GOOGL",
            "full_name": "TRADENATION:GOOGL",
            "exchange": "Trade Nation",
            "description": "ALPHABET",
            "type": "stock",
            "pro_name": "TRADENATION:GOOGL",
            "typespecs": ["cfd"],
            "resolution": "60",
            "chart_type": 1
        }"#;
        let info: SymbolInfo = serde_json::from_str(raw).expect("parses");
        assert_eq!(info.symbol, "GOOGL");
        assert_eq!(info.full_name, "TRADENATION:GOOGL");
        assert_eq!(info.exchange, "Trade Nation");
        assert_eq!(info.description, "ALPHABET");
        assert_eq!(info.asset_type, "stock");
    }

    #[test]
    fn ignores_unknown_fields_and_minimal_payload() {
        let raw = r#"{
            "symbol": "EURUSD",
            "full_name": "OANDA:EURUSD",
            "exchange": "OANDA",
            "description": "EUR / US Dollar",
            "type": "forex"
        }"#;
        let info: SymbolInfo = serde_json::from_str(raw).expect("parses");
        assert_eq!(info.symbol, "EURUSD");
        assert_eq!(info.asset_type, "forex");
    }
}
