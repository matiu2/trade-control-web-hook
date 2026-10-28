//! Broker volume limits change size, never the strategy's stop or target.
use serde::{Deserialize, Serialize};

/// Action when a cTrader entry exceeds the symbol's maximum order volume.
/// Other broker adapters retain their own sizing rules.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OversizePolicy {
    /// Use the largest valid broker volume and accept lower cash risk.
    #[default]
    CapVolume,
    /// Refuse the entry rather than reduce its volume.
    Reject,
}

impl OversizePolicy {
    pub fn is_default(&self) -> bool {
        *self == Self::CapVolume
    }
}
