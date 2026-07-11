//! Broker enum + exchange / default-account mappings.

/// The two brokers the worker currently supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Broker {
    /// OANDA v20 — uses `USD_CAD`-style instrument codes.
    Oanda,
    /// TradeNation — uses `USD/CAD`-style instrument codes.
    TradeNation,
}

impl Broker {
    /// Look up a broker from a TradingView exchange tag (the prefix
    /// before the colon in `OANDA:EUR_USD`). Case-insensitive.
    /// Returns `None` when the exchange isn't one of the known
    /// broker prefixes — callers usually fall back to [`Broker::Oanda`].
    pub fn from_exchange(exchange: &str) -> Option<Self> {
        match exchange.trim().to_ascii_uppercase().as_str() {
            "TRADENATION" => Some(Self::TradeNation),
            "OANDA" => Some(Self::Oanda),
            _ => None,
        }
    }

    /// Parse the lower-case wire form (`"oanda"` / `"tradenation"`).
    pub fn from_wire(s: &str) -> Option<Self> {
        match s.trim() {
            "oanda" => Some(Self::Oanda),
            "tradenation" => Some(Self::TradeNation),
            _ => None,
        }
    }

    /// Lower-case wire form — matches the YAML `broker:` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oanda => "oanda",
            Self::TradeNation => "tradenation",
        }
    }

    /// Default operator account index for this broker. Used when the
    /// caller hasn't passed `--account-id` or set `TRADE_CONTROL_ACCOUNT`.
    pub fn default_account_index(self) -> &'static str {
        match self {
            Self::Oanda => "m-and-w",
            Self::TradeNation => "reversals",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_exchange_known() {
        assert_eq!(Broker::from_exchange("OANDA"), Some(Broker::Oanda));
        assert_eq!(Broker::from_exchange("oanda"), Some(Broker::Oanda));
        assert_eq!(
            Broker::from_exchange("TRADENATION"),
            Some(Broker::TradeNation)
        );
    }

    #[test]
    fn from_exchange_unknown_is_none() {
        assert_eq!(Broker::from_exchange("FX:NYSE"), None);
        assert_eq!(Broker::from_exchange(""), None);
    }

    #[test]
    fn from_wire_round_trip() {
        for b in [Broker::Oanda, Broker::TradeNation] {
            assert_eq!(Broker::from_wire(b.as_str()), Some(b));
        }
    }

    #[test]
    fn default_account_indices() {
        assert_eq!(Broker::Oanda.default_account_index(), "m-and-w");
        assert_eq!(Broker::TradeNation.default_account_index(), "reversals");
    }
}
