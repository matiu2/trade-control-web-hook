//! System 1 of the DST-aware spread blackout — worker re-export shim.
//!
//! The pure decision, the per-instrument threshold lookup, the baked
//! `spread-sampler-cron` baseline, and the elevated/recovered cutoff
//! constants now live in [`trade_control_core::spread_blackout`] so the
//! offline replay (`engine::simulate_fill`, which links `core` but not this
//! worker `cdylib`) applies the *same* spread-blackout reject the live
//! worker does (`[[strategy_changes_in_both_replayer_and_worker]]`). The
//! build.rs that bakes the baseline moved to `core/build.rs` with it.
//!
//! What stays in the worker is only the I/O wrapper around the pure
//! decision: the KV `spread-blackout:window` read + the live broker quote
//! sample in `run_enter` (`src/lib.rs`), and the recovery watcher in
//! `src/cron/blackout_watch.rs`. Both reach the shared items through this
//! re-export, so existing `crate::spread_blackout::*` call sites are
//! unchanged.

// Only the items the worker's wrappers actually reach through this shim are
// re-exported (the recovery watcher reads `SPREAD_BLACKOUT_RECOVERED_PIPS`;
// `run_enter` / `blackout_cancel` use the decision + lookups). The full
// public surface — `SPREAD_REJECT_MULTIPLE`, `SPREAD_BLACKOUT_ELEVATED_PIPS`
// — lives in `trade_control_core::spread_blackout`; reach for it there.
pub use trade_control_core::spread_blackout::{
    SPREAD_BLACKOUT_RECOVERED_PIPS, elevated_threshold_pips,
};
