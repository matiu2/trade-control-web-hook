//! Tunables for the cron jobs that moved into this shared crate. One file so
//! changes are localised. The wasm worker's own `src/cron/constants.rs` keeps
//! the constants that *its* still-in-worker jobs need (`STALE_AFTER`); the
//! values shared by the moved blackout cluster live here so the worker and the
//! native runtime can't drift.

/// Spread-blackout backstop, in seconds (~3h). Single source of truth: the
/// global window-marker TTL (apply), each per-trade record's TTL, and the
/// recovery watcher's "clear regardless of spread" backstop (watch) all derive
/// from this one constant so they can never drift apart. The post-NY-close
/// liquidity trough is ~1h; 3h is a generous safety ceiling after which a
/// still-`applied` record is force-cleared.
pub const BLACKOUT_BACKSTOP_SECONDS: u64 = 3 * 60 * 60;

/// Forex pip-size fallback used by the blackout re-drive when neither the baked
/// intent `pip_size` nor the per-trade record's pip is usable. Mirrors the
/// worker's `DEFAULT_PIP_SIZE` (which is `&Env`-side and not linkable here);
/// only ever resolves absolute prices for the cheap fill-side pre-check.
pub const DEFAULT_PIP_SIZE: f64 = 0.0001;
