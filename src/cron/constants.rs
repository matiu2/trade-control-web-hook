//! Tunables for the cron handler. One file so changes are localised.
//! Promote to wrangler.toml `[vars]` if operational tuning becomes
//! frequent.

use chrono::Duration;

/// A cached TN session older than this is force-refreshed on the
/// next cron tick. Hint, not a correctness boundary — the existing
/// re-login-on-rejection path stays the safety net.
pub const STALE_AFTER: Duration = Duration::hours(12);
