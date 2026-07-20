//! Candle-derived, per-venue **market-hours** blackout generator.
//!
//! Offline operator tool: fetches H1 candles from OANDA and TradeNation,
//! measures each instrument's ATR-relative close→open price gaps
//! ([`compute`]), and renders a committed `market_hours_baked.rs` table of
//! `(venue, symbol) → weekend + optional daily-close` blocks. The `core` gate
//! turns each row into a weekday-aware [`WeekMask`](trade_control_core) at read
//! time.
//!
//! # Why candle-derived, not session-string-derived
//!
//! The predecessor read each broker's `market_info` session string and
//! inflated it with buffers — but that string is day-of-week-blind, so a
//! 5-minute daily housekeeping gap became a phantom multi-hour blackout applied
//! every weekday, wrongly rejecting mid-week entries. Measuring the ACTUAL
//! reopen price gaps from candles, and separating the universal weekend gap
//! from a genuine mid-week daily close, is what fixes that. See the
//! `market-hours-blackout-weekly-gap-bug` memory.
//!
//! # Per-venue principle
//!
//! OANDA `EUR_USD` and TradeNation `EUR/USD` are different instruments with
//! different session structures (different index hours, OANDA lists bonds TN
//! doesn't). Each venue gets its own row from its own candles — no canonical
//! sharing. The gate keys on `(venue, symbol)`.

pub mod compute;
pub mod fetch;
pub mod render;
pub mod universe;

pub use compute::{MarketHoursProfile, profile_from_bars};
pub use render::render_table;

/// Which venue a computed row belongs to. Tags each table row so the gate can
/// disambiguate the two venues' symbol namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Venue {
    Oanda,
    TradeNation,
}

impl Venue {
    pub fn as_str(self) -> &'static str {
        match self {
            Venue::Oanda => "oanda",
            Venue::TradeNation => "tradenation",
        }
    }
}

/// A single computed row: venue + the instrument's venue-native symbol + its
/// profile. The generated table is a slice of these, sorted for lookup.
#[derive(Debug, Clone)]
pub struct MarketHoursRow {
    pub venue: Venue,
    /// The venue-native symbol string (`"EUR_USD"` OANDA / `"EUR/USD"` TN).
    pub symbol: String,
    /// Human display name (validation report only).
    pub display_name: String,
    pub profile: MarketHoursProfile,
    /// Set when the instrument produced too few gap events or errored — the row
    /// is still emitted (weekend block only) but flagged unreviewed.
    pub error: Option<String>,
}
