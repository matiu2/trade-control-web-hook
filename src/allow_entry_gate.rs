//! The `allow_entry` gate — relocated to `core`.
//!
//! The pure decision (the `allow_entry` Rhai script + the AND-composed
//! candle-quality gate) moved to [`trade_control_core::allow_entry_gate`]
//! so that **both** the live worker and the offline replay
//! (`engine::simulator`) apply the same gate and can't drift (rule
//! `[[strategy_changes_in_both_replayer_and_worker]]`). Rhai compiles
//! off-wasm, so the replay can run the script identically.
//!
//! This worker module is now a thin re-export so its call sites in
//! `src/lib.rs` (`allow_entry_gate::evaluate`,
//! `allow_entry_gate::AllowEntryOutcome::*`) are byte-unchanged.

pub use trade_control_core::allow_entry_gate::*;
