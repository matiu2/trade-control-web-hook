//! Which broker's candle feed to replay through the engine.

use clap::ValueEnum;

/// The candle data source. The live cron engine pulls from TradeNation, so
/// that's the default — it reproduces what the engine actually saw. OANDA is
/// offered because candle-cache caches it to disk and it needs no TradeNation
/// session; its mid prices differ slightly from TradeNation's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum CandleSource {
    /// TradeNation chart OHLCV (matches the live engine).
    TradeNation,
    /// OANDA v20 candles via candle-cache (disk-cached).
    Oanda,
}
