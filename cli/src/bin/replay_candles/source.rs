//! Which broker candle-cache pulls (and caches) candles from.

use clap::ValueEnum;

/// Which broker candle-cache pulls (and caches) candles from. **Both** sources
/// always go through candle-cache, so either choice fills the on-disk cache and
/// reduces future broker calls — `--source` only selects the broker, never
/// whether the cache is used. The live cron engine pulls from TradeNation, so
/// that's the default: it reproduces what the engine actually saw. OANDA is
/// offered because it needs no TradeNation session; its mid prices differ
/// slightly from TradeNation's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum CandleSource {
    /// TradeNation candles via candle-cache (matches the live engine).
    TradeNation,
    /// OANDA v20 candles via candle-cache.
    Oanda,
}
