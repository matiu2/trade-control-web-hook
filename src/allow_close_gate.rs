//! The `allow_close` gate.
//!
//! The pure decision logic ([`evaluate`], [`AllowCloseOutcome`]) moved to
//! [`trade_control_core::allow_close_gate`] so the live worker (`run_close`
//! in `src/lib.rs`) and the offline replay share one implementation and
//! can't drift (`[[strategy_changes_in_both_replayer_and_worker]]`): a
//! blocked `allow_close` keeps a position OPEN, and without the shared
//! decision the replay would close it and diverge. This file re-exports it
//! verbatim so every call site (`allow_close_gate::evaluate`,
//! `allow_close_gate::AllowCloseOutcome::*`) is byte-unchanged.
//!
//! Note: core's copy inlines the `needs_golden` / `needs_confirmed`
//! candle-quality check (identical to the worker's [`crate::candle_gate`]
//! two-field predicate) so it can live in `core` without depending on the
//! worker-only `candle_gate` module. The entry path still uses
//! `candle_gate` directly; both must stay in lockstep.

pub use trade_control_core::allow_close_gate::*;
