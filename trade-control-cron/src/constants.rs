//! Tunables for the cron jobs that moved into this shared crate. One file so
//! changes are localised. The wasm worker's own `src/cron/constants.rs` keeps
//! the constants that *its* still-in-worker jobs need (`STALE_AFTER`); the
//! values shared by the moved blackout cluster live here so the worker and the
//! native runtime can't drift.

/// The spread-blackout backstop's two split concerns, re-exported from `core`
/// (2026-07). Historically a single `BLACKOUT_BACKSTOP_SECONDS = 3h` drove both
/// the per-record TTL and the safety force-restore ceiling; that broke for
/// multi-hour blocks (an 8h AUD/CHF record expired at 3h before its block-lift
/// restore, and the 3h backstop force-restored back into the active trough).
/// Now:
/// - [`spread_block_ttl_seconds`] — per-instrument record TTL = block length + grace.
/// - [`SAFETY_FORCE_RESTORE_SECONDS`] — global last-resort force-restore ceiling.
///
/// Both live in `core` so the offline replay computes identical values without
/// depending on this crate.
pub use trade_control_core::spread_blackout::{
    SAFETY_FORCE_RESTORE_SECONDS, spread_block_ttl_seconds,
};

/// Forex pip-size fallback used by the blackout re-drive when neither the baked
/// intent `pip_size` nor the per-trade record's pip is usable. Mirrors the
/// worker's `DEFAULT_PIP_SIZE` (which is `&Env`-side and not linkable here);
/// only ever resolves absolute prices for the cheap fill-side pre-check.
pub const DEFAULT_PIP_SIZE: f64 = 0.0001;
