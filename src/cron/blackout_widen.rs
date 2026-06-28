//! System 2 (spread-blackout stop widening) pure math.
//!
//! The decision logic moved to [`trade_control_core::blackout_widen`] so
//! the live worker and the offline replay share one implementation and
//! can't drift (`[[strategy_changes_in_both_replayer_and_worker]]`). This
//! file re-exports it verbatim so every call site
//! (`super::blackout_widen::{clamp_widen, widened_stop}`) is unchanged.

pub use trade_control_core::blackout_widen::*;
