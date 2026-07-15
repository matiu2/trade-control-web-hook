//! Candle-derived, per-broker spread-baseline generator.
//!
//! Offline operator tool: fetches H1 bid/ask candles from OANDA and
//! TradeNation, computes a per-(broker, instrument) spread-hour mask via the
//! med3 rule ([`compute`]), and renders a committed `spread_baseline.rs` table.
//!
//! Supersedes the `spread-sampler-cron` → `core/build.rs` sampler pipeline as
//! the data source for the spread-hour gate. See
//! `SCOPING-candle-derived-spread-baseline.md`.
//!
//! **Per-broker principle (locked):** OANDA `EUR_USD` and TradeNation
//! `EUR/USD` are different instruments — each gets its own mask from its own
//! broker's candles. No canonical sharing. The gate already keys on the exact
//! broker symbol string, so both rows coexist as distinct keys.

pub mod compute;
pub mod fetch;
pub mod render;
pub mod universe;

pub use compute::{
    Bar, MinuteBar, ReviewStatus, SpreadProfile, profile_for_instrument, profile_from_minutes,
};
pub use render::render_table;

/// Which broker a computed profile belongs to. Tags each table row so the
/// generated key is unambiguous (and, in principle, to let a future gate
/// disambiguate two identical symbol strings — though today the broker symbol
/// forms already differ).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Broker {
    Oanda,
    TradeNation,
}

impl Broker {
    pub fn as_str(self) -> &'static str {
        match self {
            Broker::Oanda => "oanda",
            Broker::TradeNation => "tradenation",
        }
    }
}

/// A single computed row: broker + the instrument's broker-native symbol + its
/// profile. The generated table is a slice of these, sorted for binary search.
#[derive(Debug, Clone)]
pub struct BaselineRow {
    pub broker: Broker,
    /// The broker-native symbol string (`"EUR_USD"` OANDA / `"EUR/USD"` TN).
    pub symbol: String,
    /// Human display name (for the validation report only).
    pub display_name: String,
    pub profile: SpreadProfile,
}
