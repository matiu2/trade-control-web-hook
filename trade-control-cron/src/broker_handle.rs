//! [`BrokerHandle`] — the broker enum the cron engine matches on.
//!
//! Moved here from the wasm worker's `src/cron/sweep.rs` so the engine tick
//! (now in [`crate::engine`]) and both runtimes (the wasm worker + the native
//! VM scheduler) share one definition. It holds the same `OandaBroker` /
//! `TradeNationAdapter` types on both runtimes, so it is shared verbatim.

/// One enum so the dispatcher can return either broker type without boxing
/// across the async boundary (which `impl Trait` precludes). Shared with the
/// spread-recovery watcher (still in the wasm worker's sweep), which calls
/// `get_quote` through the same per-broker match.
pub enum BrokerHandle {
    Oanda(broker_oanda::OandaBroker),
    TradeNation(broker_tradenation_adapter::TradeNationAdapter),
}
