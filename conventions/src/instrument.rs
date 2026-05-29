//! Symbol formatting between TradingView's display form and the
//! broker's canonical form.
//!
//! Conventions (from `tv_arm_hs.py:379-388`):
//! - 6-letter alpha symbol (`EURUSD`) → `EUR_USD` (OANDA) or
//!   `EUR/USD` (TradeNation).
//! - Anything else passes through with `/` ↔ `_` swap per broker.

use alloc::string::String;
use alloc::string::ToString;

use crate::broker::Broker;

/// Split a `EXCHANGE:SYMBOL` pair. When there's no colon (e.g.
/// `EUR_USD`), returns `(None, "EUR_USD")`.
pub fn split_symbol(symbol: &str) -> (Option<&str>, &str) {
    match symbol.split_once(':') {
        Some((exchange, sym)) if !sym.is_empty() => (Some(exchange), sym),
        _ => (None, symbol),
    }
}

/// Format `raw_sym` into the broker's canonical instrument code.
///
/// Behavior matches `tv_arm_hs.py::instrument_for`:
/// - 6-letter all-alpha symbol (`EURUSD`, `eurusd`) is uppercased
///   and split into a 3+3 currency pair: `EUR_USD` / `EUR/USD`.
/// - Any other shape passes through with `/` ↔ `_` swap.
pub fn instrument_for(broker: Broker, raw_sym: &str) -> String {
    if raw_sym.len() == 6 && raw_sym.chars().all(|c| c.is_ascii_alphabetic()) {
        let a = raw_sym[..3].to_ascii_uppercase();
        let b = raw_sym[3..].to_ascii_uppercase();
        return match broker {
            Broker::TradeNation => alloc::format!("{a}/{b}"),
            Broker::Oanda => alloc::format!("{a}_{b}"),
        };
    }
    match broker {
        Broker::TradeNation => raw_sym.replace('_', "/"),
        Broker::Oanda => raw_sym.replace('/', "_"),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_with_exchange() {
        assert_eq!(split_symbol("OANDA:EUR_USD"), (Some("OANDA"), "EUR_USD"));
    }

    #[test]
    fn split_without_exchange() {
        assert_eq!(split_symbol("EUR_USD"), (None, "EUR_USD"));
    }

    #[test]
    fn split_with_trailing_colon_collapses_to_just_sym() {
        // `tv_arm_hs.py` falls back to (sym, "") shape — we model that
        // with `(None, "EUR_USD")`.
        assert_eq!(split_symbol("EUR_USD:"), (None, "EUR_USD:"));
    }

    #[test]
    fn six_letter_alpha_to_oanda() {
        assert_eq!(instrument_for(Broker::Oanda, "EURUSD"), "EUR_USD");
        assert_eq!(instrument_for(Broker::Oanda, "eurusd"), "EUR_USD");
    }

    #[test]
    fn six_letter_alpha_to_tradenation() {
        assert_eq!(instrument_for(Broker::TradeNation, "EURUSD"), "EUR/USD");
    }

    #[test]
    fn passthrough_oanda_swaps_slash_to_underscore() {
        assert_eq!(instrument_for(Broker::Oanda, "EUR/USD"), "EUR_USD");
    }

    #[test]
    fn passthrough_tradenation_swaps_underscore_to_slash() {
        assert_eq!(instrument_for(Broker::TradeNation, "EUR_USD"), "EUR/USD");
    }

    #[test]
    fn non_fx_passthrough() {
        // Indices/commodities with non-6-letter names should pass
        // through unmodified except for separator swap.
        assert_eq!(instrument_for(Broker::Oanda, "DE40"), "DE40");
        assert_eq!(
            instrument_for(Broker::TradeNation, "Spot Silver"),
            "Spot Silver"
        );
    }
}
