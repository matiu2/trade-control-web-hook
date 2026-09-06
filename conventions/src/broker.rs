//! Broker enum + exchange / default-account mappings.

/// The brokers the worker knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Broker {
    /// OANDA v20 — uses `USD_CAD`-style instrument codes.
    Oanda,
    /// TradeNation — uses `USD/CAD`-style instrument codes.
    TradeNation,
    /// Interactive Brokers — futures only, `GCZ6`-style contract symbols.
    /// No broker implementation exists yet; the variant is here so the
    /// compiler enumerates the places that will need one.
    Ibkr,
}

impl Broker {
    /// Every variant, in menu order. See [`BrokerKind::ALL`] in `core` — same
    /// role, and the two lists must agree because `tv-arm` maps between them.
    ///
    /// [`BrokerKind::ALL`]: https://docs.rs/trade-control-core
    pub const ALL: &'static [Broker] = &[Broker::Oanda, Broker::TradeNation, Broker::Ibkr];

    /// Look up a broker from a TradingView exchange tag (the prefix
    /// before the colon in `OANDA:EUR_USD`). Case-insensitive.
    /// Returns `None` when the exchange isn't one of the known
    /// broker prefixes — callers usually fall back to [`Broker::Oanda`].
    pub fn from_exchange(exchange: &str) -> Option<Self> {
        match exchange.trim().to_ascii_uppercase().as_str() {
            "TRADENATION" => Some(Self::TradeNation),
            "OANDA" => Some(Self::Oanda),
            // Deliberately absent: IBKR has no TradingView exchange prefix we
            // arm from. A futures chart is read under its own exchange tag
            // (COMEX/CME), and mapping those here would silently arm a futures
            // contract off a chart the operator did not pick a broker for.
            _ => None,
        }
    }

    /// Parse the lower-case wire form (`"oanda"` / `"tradenation"`).
    pub fn from_wire(s: &str) -> Option<Self> {
        match s.trim() {
            "oanda" => Some(Self::Oanda),
            "tradenation" => Some(Self::TradeNation),
            "ibkr" => Some(Self::Ibkr),
            _ => None,
        }
    }

    /// Lower-case wire form — matches the YAML `broker:` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oanda => "oanda",
            Self::TradeNation => "tradenation",
            Self::Ibkr => "ibkr",
        }
    }

    /// Default operator account index for this broker. Used when the
    /// caller hasn't passed `--account-id` or set `TRADE_CONTROL_ACCOUNT`.
    ///
    /// `None` when the broker has no default — the caller must then require an
    /// explicit account rather than substituting one. IBKR is that case today:
    /// no IBKR account exists, so any placeholder returned here would name an
    /// account that isn't in the store and fail far from the cause.
    pub fn default_account_index(self) -> Option<&'static str> {
        match self {
            Self::Oanda => Some("m-and-w"),
            Self::TradeNation => Some("reversals"),
            Self::Ibkr => None,
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
        assert_eq!(Broker::Oanda.default_account_index(), Some("m-and-w"));
        assert_eq!(
            Broker::TradeNation.default_account_index(),
            Some("reversals")
        );
    }

    /// IBKR has no account in the store, so it must report "no default" rather
    /// than a placeholder. An empty-string default would flow into a plan as a
    /// blank account name and fail at dispatch, far from the cause.
    #[test]
    fn ibkr_has_no_default_account_and_says_so() {
        assert_eq!(Broker::Ibkr.default_account_index(), None);
    }

    /// `ALL` must actually list every variant. A `from_wire`/`as_str`
    /// round-trip driven off a stale `ALL` would silently stop covering the
    /// variant that was left out, so this walks `ALL` and also pins its length.
    #[test]
    fn all_covers_every_variant_and_round_trips() {
        assert_eq!(Broker::ALL.len(), 3, "a new broker must be added to ALL");
        for &b in Broker::ALL {
            assert_eq!(Broker::from_wire(b.as_str()), Some(b), "{b:?}");
        }
    }

    #[test]
    fn ibkr_parses_from_the_wire_form() {
        assert_eq!(Broker::from_wire("ibkr"), Some(Broker::Ibkr));
        assert_eq!(Broker::Ibkr.as_str(), "ibkr");
    }

    /// IBKR is deliberately absent from the TradingView exchange mapping: a
    /// COMEX/CME chart must not silently resolve to a broker the operator
    /// never picked.
    #[test]
    fn ibkr_is_not_reachable_from_a_tradingview_exchange_tag() {
        assert_eq!(Broker::from_exchange("IBKR"), None);
        assert_eq!(Broker::from_exchange("COMEX"), None);
        assert_eq!(Broker::from_exchange("CME"), None);
    }
}
