//! Tunables for the cron jobs that moved into this shared crate. One file so
//! changes are localised. The wasm worker's own `src/cron/constants.rs` keeps
//! the constants that *its* still-in-worker jobs need (`STALE_AFTER`); the
//! values shared by the moved blackout cluster live here so the worker and the
//! native runtime can't drift.

/// Spread-blackout backstop, in seconds (~3h). Re-exported from
/// [`trade_control_core::spread_blackout::BLACKOUT_BACKSTOP_SECONDS`] — the value
/// moved into `core` so the offline replay's transient-widen reconstruction
/// computes the same backstop as the live recovery watcher without depending on
/// this crate. Kept re-exported here (rather than fixing up every call site) so
/// the apply/watch/record TTLs still read it from `constants` unchanged.
pub use trade_control_core::spread_blackout::BLACKOUT_BACKSTOP_SECONDS;

/// Forex pip-size fallback used by the blackout re-drive when neither the baked
/// intent `pip_size` nor the per-trade record's pip is usable. Mirrors the
/// worker's `DEFAULT_PIP_SIZE` (which is `&Env`-side and not linkable here);
/// only ever resolves absolute prices for the cheap fill-side pre-check.
pub const DEFAULT_PIP_SIZE: f64 = 0.0001;
