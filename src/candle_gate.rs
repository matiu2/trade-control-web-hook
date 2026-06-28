//! Candle-quality gate — relocated to `core`.
//!
//! The pure decision (`needs_golden` / `needs_confirmed` shell checks)
//! moved to [`trade_control_core::candle_gate`] so that **both** the live
//! worker and the offline replay (`engine::simulator`) apply the same
//! gate and can't drift (rule `[[strategy_changes_in_both_replayer_and_worker]]`).
//!
//! This worker module is now a thin re-export so its call sites in
//! `src/lib.rs` (`candle_gate::evaluate`, `candle_gate::CandleGateOutcome`)
//! are byte-unchanged.

pub use trade_control_core::candle_gate::*;
