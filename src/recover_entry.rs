//! Broker-side stop-entry recovery (`recover_entry`, `#19-10` path).
//!
//! The **pure** decision logic ([`recover_entry_plan`],
//! [`outcome_for_entry_error`], [`RecoverEntryPlan`]) moved to
//! [`trade_control_core::recover_entry`] so the live worker and the
//! offline replay share one implementation and can't drift
//! (`[[strategy_changes_in_both_replayer_and_worker]]`). This file
//! re-exports it verbatim so every call site
//! (`recover_entry::recover_entry_plan`, `recover_entry::RecoverEntryPlan`,
//! `recover_entry::outcome_for_entry_error`) in `run_enter` (`src/lib.rs`)
//! is byte-unchanged. The broker re-place and KV bookkeeping stay in
//! `run_enter`.

pub use trade_control_core::recover_entry::*;
