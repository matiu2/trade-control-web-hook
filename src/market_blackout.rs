//! Market-hours entry blackout — System 1 (the reject gate). Reject a
//! brand-new entry that fires inside a per-instrument close→open gap, so a
//! resting stop order can never be left to trigger on the reopen liquidity
//! gap (the incident this feature fixes).
//!
//! The per-instrument no-entry windows (UTC minute-of-day ranges) are
//! derived daily by the 06:00 UTC cron (`src/cron/blackout_hours.rs`) from
//! the broker's session hours and stored in KV. The pure derivation +
//! `is_inside_any` predicate live in `trade_control_core::intent::blackout`
//! and are unit-tested there. This module holds only the worker-side glue:
//! turning `now` into a UTC minute-of-day, which the gate in `run_enter`
//! (src/lib.rs) feeds to `is_inside_any`.
//!
//! Reject, not delay — exactly like the spread blackout. The next signal
//! bar refires and re-checks; once the market has reopened the same entry
//! passes. Returning `ActionResult::Rejected` is a `Skip` in `seen_decision`
//! (no `mark_seen`), so this reject never poisons the intent id. See
//! CLAUDE.md "Replay protection scope".

// `now_utc_minute_of_day` moved to `trade_control_core::sweep_gate` so the
// offline replay (which can't depend on this worker `cdylib`) shares one
// definition with the worker — the `[[strategy_changes_in_both_replayer_and_worker]]`
// rule. Re-exported here so the `run_enter` gate call site
// (`market_blackout::now_utc_minute_of_day`) stays byte-unchanged. The
// predicate's unit tests live alongside it in `core::sweep_gate`.
pub use trade_control_core::sweep_gate::now_utc_minute_of_day;
