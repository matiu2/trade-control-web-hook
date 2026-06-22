//! Continuous **at-entry level vetos** (Bug #12).
//!
//! The legacy TradingView model fired an *external* `too-low` / `too-high`
//! alert that **wrote a KV veto** which then persisted ~44 h; a later
//! confirmed `enter` found it via `is_vetoed` and was rejected. The engine
//! migration (TV alerts retired 2026-06-22) re-modelled those as one-shot
//! cross-event guard rules: the KV veto is only written when price *crosses*
//! the level on a closed candle the engine evaluates. A gap past the level,
//! a level already breached when the plan armed, or a cross during a phase
//! where the guard is disarmed produces **no** KV veto — so the enter
//! confirmed and an order was placed that the veto existed to prevent
//! (NZD/CAD, −110.53 GBP, 10–11 Jun 2026; see bug-012).
//!
//! `too-low` is really a *continuous* predicate — "is the entry price already
//! past the pcl-exhausted level (most of the reward gone)?" — not a one-shot
//! cross. We restore the continuous protection by **baking the level onto the
//! enter intent** and re-checking it in `run_enter` against the resolved
//! entry price, independent of whether any cross-event guard fired. This is
//! pure data + a truth-table; the worker gate is the thin wrapper.

use serde::{Deserialize, Serialize};

/// Which side of the level counts as "already past" — i.e. the entry is too
/// far into the move for the trade to be worth taking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VetoSide {
    /// Reject when the entry price is **at or below** the level.
    Below,
    /// Reject when the entry price is **at or above** the level.
    Above,
}

/// A continuous at-entry level veto baked onto the enter intent. Names a
/// veto, a price level, and which side counts as "past".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryLevelVeto {
    /// The veto name, reused verbatim in the worker's reject outcome
    /// (`rejected: veto-active (<name>)`) — e.g. `"too-low"` / `"too-high"`.
    pub name: String,
    /// The price level the entry is checked against.
    pub level: f64,
    /// Which side of `level` is "past" (reject).
    pub past: VetoSide,
}

impl EntryLevelVeto {
    /// Is the resolved entry price already past the level? "Past" is
    /// **inclusive** at the level, mirroring the legacy Intrabar
    /// `CrossDir::Either` straddle (`low <= level <= high`).
    pub fn is_past(&self, entry_price: f64) -> bool {
        match self.past {
            VetoSide::Below => entry_price <= self.level,
            VetoSide::Above => entry_price >= self.level,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn veto(past: VetoSide) -> EntryLevelVeto {
        EntryLevelVeto {
            name: "too-low".into(),
            level: 1.2000,
            past,
        }
    }

    #[test]
    fn below_fires_at_or_under_level() {
        let v = veto(VetoSide::Below);
        assert!(v.is_past(1.1999), "below the level is past");
        assert!(v.is_past(1.2000), "at the level is past (inclusive)");
        assert!(!v.is_past(1.2001), "above the level is not past");
    }

    #[test]
    fn above_fires_at_or_over_level() {
        let v = veto(VetoSide::Above);
        assert!(v.is_past(1.2001), "above the level is past");
        assert!(v.is_past(1.2000), "at the level is past (inclusive)");
        assert!(!v.is_past(1.1999), "below the level is not past");
    }

    #[test]
    fn round_trips_through_json() {
        let v = veto(VetoSide::Above);
        let json = serde_json::to_string(&v).expect("serialise");
        let back: EntryLevelVeto = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(v, back);
    }
}
