//! Resolve an instrument's market-hours no-entry windows for the replay.
//!
//! The live worker rejects/sweeps a resting order caught inside an instrument's
//! daily close→open session gap. Live, those windows are written to KV daily by
//! the `blackout_hours` cron, which calls TradeNation `market_info` and feeds the
//! Brisbane session ranges through the pure `core::windows_from_session` deriver.
//! The replay feeds the resulting windows to [`engine::sweep_reason`] so a
//! market-hours-blackout sweep is reconstructed the way the live worker would.
//!
//! # Status: seam ready, source pending
//!
//! The engine side is **done and source-agnostic** — `sweep_reason` already
//! takes the windows and lights up its `Blackout` branch via the shared
//! `core::market_blackout_due`. What's not yet wired is *where the windows come
//! from* offline: a forthcoming shared market-hours source (being produced
//! out-of-band, to be folded into the engine/core) will supply them. Until then
//! this returns an **empty** set with a `WARN`, so the replay behaves exactly as
//! before (the order is still reported `NEVER FILLED`, just without the blackout
//! label). When the source lands, only the body of [`resolve_blackout_windows`]
//! changes — the call site and the engine seam stay put.
//!
//! **Fail-soft, always.** Market hours are a post-mortem annotation, never a
//! fill/exit decision, so a missing source must never abort a replay — empty
//! windows ⇒ `sweep_reason`'s blackout branch never fires (its fail-open).

use trade_control_core::intent::NoEntryWindow;

use super::source::CandleSource;

/// Resolve `instrument`'s market-hours no-entry windows, or an empty set (with a
/// `WARN`) when no source is wired yet.
///
/// TODO(replay-parity-item-3): feed this from the forthcoming shared
/// market-hours source (to be added to the engine/core). Live the worker uses
/// TradeNation `market_info` → `core::windows_from_session`; the replay must use
/// the **same** windows to stay in lockstep (TradingView's charted-exchange
/// hours would diverge from the broker's). OANDA stays empty either way — the
/// worker's `blackout_hours` cron skips OANDA (no `market_info` equivalent).
pub async fn resolve_blackout_windows(
    source: CandleSource,
    instrument: &str,
) -> Vec<NoEntryWindow> {
    let _ = source;
    tracing::warn!(
        "market-hours: no offline window source wired yet for {instrument}; blackout sweeps \
         won't be reconstructed (the order still reports NEVER FILLED, just without the \
         blackout label). See TODO(replay-parity-item-3)."
    );
    Vec::new()
}
