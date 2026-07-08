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

/// What `tv info` returns. The identity fields are always present; the
/// numeric fields (`tick_size`, `point_value`, `session`, ...) come from the
/// extended `tv info` (which reads `symbolInfoWV()`) and are `Option` so an
/// older payload or an unresolved symbol still parses. Truly unused fields
/// (`typespecs`, `resolution`, `chart_type`) are dropped on deserialization.
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
#[derive(Debug, Clone, Default, Deserialize)]
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

    // --- Numeric symbol facts (from the extended `tv info`, which reads
    // TradingView's `symbolInfoWV()`). All optional: a payload from the
    // older `symbolExt()`-only build, or a symbol TV couldn't resolve,
    // simply leaves these `None` and the caller falls back to the catalog.
    /// `"EXCHANGE:SYMBOL"` as TradingView reports it internally. This is the
    /// authoritative per-broker key (`"OANDA:AU200AUD"`), which can differ
    /// from a catalog-derived key.
    #[serde(default)]
    pub pro_name: Option<String>,
    /// Minimum price increment = `minmov / pricescale` (e.g. `0.00001` for a
    /// 5-dp FX major, `0.1` for the OANDA AU200 index). Computed JS-side.
    #[serde(default)]
    pub tick_size: Option<f64>,
    /// Number of quoted decimal places (`round(log10(pricescale))`).
    #[serde(default)]
    pub decimal_places: Option<u8>,
    /// TradingView `pointvalue` — per-point contract value.
    #[serde(default)]
    pub point_value: Option<f64>,
    /// Quote currency (`"USD"`, `"NZD"`, ...).
    #[serde(default)]
    pub currency_code: Option<String>,
    /// Session-hours string (`"1700-1700"`).
    #[serde(default)]
    pub session: Option<String>,
    /// Exchange timezone (`"America/New_York"`).
    #[serde(default)]
    pub timezone: Option<String>,
    /// Listing exchange TradingView reports (`"OANDA"`, `"TRADENATION"`).
    #[serde(default)]
    pub listed_exchange: Option<String>,
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
        // Minimal (old-build) payload → numeric fields absent, not an error.
        assert_eq!(info.tick_size, None);
        assert_eq!(info.point_value, None);
        assert_eq!(info.session, None);
    }

    #[test]
    fn parses_extended_numeric_payload() {
        // The extended `tv info` (reads symbolInfoWV()) — real AU200 shape.
        let raw = r#"{
            "success": true,
            "symbol": "AU200AUD",
            "full_name": "OANDA:AU200AUD",
            "exchange": "OANDA",
            "description": "Australia 200",
            "type": "index",
            "pro_name": "OANDA:AU200AUD",
            "pricescale": 10,
            "minmov": 1,
            "tick_size": 0.1,
            "decimal_places": 1,
            "point_value": 1,
            "currency_code": "AUD",
            "session": "1700-1700",
            "timezone": "America/New_York",
            "listed_exchange": "OANDA"
        }"#;
        let info: SymbolInfo = serde_json::from_str(raw).expect("parses");
        assert_eq!(info.pro_name.as_deref(), Some("OANDA:AU200AUD"));
        assert_eq!(info.tick_size, Some(0.1));
        assert_eq!(info.decimal_places, Some(1));
        assert_eq!(info.point_value, Some(1.0));
        assert_eq!(info.currency_code.as_deref(), Some("AUD"));
        assert_eq!(info.session.as_deref(), Some("1700-1700"));
        assert_eq!(info.listed_exchange.as_deref(), Some("OANDA"));
    }
}
