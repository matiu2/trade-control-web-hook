//! Tunables for the cron jobs that moved into this shared crate. One file so
//! changes are localised. The wasm worker's own `src/cron/constants.rs` keeps
//! the constants that *its* still-in-worker jobs need (`STALE_AFTER`); the
//! values shared by the moved blackout cluster live here so the worker and the
//! native runtime can't drift.

/// Per-instrument spread-blackout record TTL = block length + grace, re-exported
/// from `core` (2026-07 backstop split). Used by System 2's widen record in
/// `blackout_apply` so its record outlives the block. Lives in `core` so the
/// offline replay computes the identical value.
///
/// The other split concern, `SAFETY_FORCE_RESTORE_SECONDS`, and the resting-order
/// re-drive's `DEFAULT_PIP_SIZE` fallback are now consumed only inside
/// `core::pending_lifecycle` (the shared fn the cron delegates System 3 to, PR 2),
/// so they no longer need re-exporting here.
pub use trade_control_core::spread_blackout::spread_block_ttl_seconds;
