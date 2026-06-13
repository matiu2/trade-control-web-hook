//! Tunables for the cron handler. One file so changes are localised.
//! Promote to wrangler.toml `[vars]` if operational tuning becomes
//! frequent.

use chrono::Duration;

/// A cached TN session older than this is force-refreshed on the
/// next cron tick. Hint, not a correctness boundary — the existing
/// re-login-on-rejection path stays the safety net.
pub const STALE_AFTER: Duration = Duration::hours(12);

/// Spread-blackout backstop, in seconds (~3h). Single source of truth:
/// the global window-marker TTL (Cron 1), each per-trade record's TTL,
/// and the recovery watcher's "clear regardless of spread" backstop
/// (Cron 2) all derive from this one constant so they can never drift
/// apart. The post-NY-close liquidity trough is ~1h; 3h is a generous
/// safety ceiling after which a still-`applied` record is force-cleared.
pub const BLACKOUT_BACKSTOP_SECONDS: u64 = 3 * 60 * 60;
