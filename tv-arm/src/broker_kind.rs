//! Converting between the conventions crate's `Broker` and the cli crate's
//! `BrokerKind`.
//!
//! Two enums for one concept, because the crates don't depend on each other in
//! the direction that would let one own it: `trade-control-conventions` is the
//! low-level vocabulary and `trade-control-cli` is a consumer of it, but
//! `BrokerKind` is part of the cli's *signed wire* types. Rather than couple the
//! wire format to the vocabulary crate, tv-arm converts at the boundary.
//!
//! Exhaustive `match` on both sides on purpose — a new broker fails to compile
//! here, which is the cheapest possible place to be told about it.

use trade_control_cli as cli;
use trade_control_conventions::Broker;

/// Vocabulary → wire.
pub fn broker_to_kind(b: Broker) -> cli::BrokerKind {
    match b {
        Broker::Oanda => cli::BrokerKind::Oanda,
        Broker::TradeNation => cli::BrokerKind::TradeNation,
    }
}

/// Wire → vocabulary.
pub fn kind_to_broker(k: cli::BrokerKind) -> Broker {
    match k {
        cli::BrokerKind::Oanda => Broker::Oanda,
        cli::BrokerKind::TradeNation => Broker::TradeNation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips both ways for every variant. A mapping that crossed the wires
    /// would arm a TradeNation trade against OANDA — the conversion is one line
    /// per broker, which is exactly the kind of thing that gets typo'd and never
    /// noticed.
    #[test]
    fn every_broker_round_trips() {
        for b in [Broker::Oanda, Broker::TradeNation] {
            assert_eq!(kind_to_broker(broker_to_kind(b)), b);
        }
        for k in [cli::BrokerKind::Oanda, cli::BrokerKind::TradeNation] {
            assert_eq!(broker_to_kind(kind_to_broker(k)), k);
        }
    }

    /// …and that the mapping is the identity on names, not just a bijection. A
    /// swapped pair round-trips perfectly and is still wrong.
    #[test]
    fn the_mapping_is_not_merely_a_bijection() {
        assert_eq!(broker_to_kind(Broker::Oanda), cli::BrokerKind::Oanda);
        assert_eq!(
            broker_to_kind(Broker::TradeNation),
            cli::BrokerKind::TradeNation
        );
    }
}
