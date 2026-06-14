//! Per-tick TN session pre-warm. Iterates the operator's known TN
//! accounts (via `MetadataStore::list()`) and, for any whose cached
//! session is older than [`STALE_AFTER`](crate::cron::constants),
//! forces a re-login via the existing `acquire_tn_broker` helper.
//! Re-login itself writes the fresh `cached_at` via `cache_and_open`,
//! so this module only decides "is it time to refresh?"

use chrono::{DateTime, Duration, Utc};
use worker::Env;

/// Walk every TN account in the operator's metadata store; force a
/// re-login for any whose cached session is older than `threshold`.
/// Single-account errors are logged and skipped — never abort the loop.
pub async fn refresh_stale_sessions(env: &Env, now: DateTime<Utc>, threshold: Duration) {
    #[cfg(target_arch = "wasm32")]
    {
        refresh_stale_sessions_wasm(env, now, threshold).await;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // Native test builds can't run worker::Fetch login flows; mirror
        // the rest of the wasm-only TN paths in `lib.rs` and no-op.
        let _ = (env, now, threshold);
    }
}

#[cfg(target_arch = "wasm32")]
async fn refresh_stale_sessions_wasm(env: &Env, now: DateTime<Utc>, threshold: Duration) {
    use trade_control_core::account::MetadataStore;
    use trade_control_core::intent::BrokerKind;

    let kv = match env.kv(crate::KV_NAMESPACE) {
        Ok(kv) => kv,
        Err(err) => {
            rlog_err!("cron session_refresh: KV binding missing: {err:?}");
            return;
        }
    };
    let metadata = crate::accounts::KvMetadataStore::new(kv.clone());
    let accounts = match metadata.list().await {
        Ok(v) => v,
        Err(err) => {
            rlog_err!("cron session_refresh: metadata list: {err}");
            return;
        }
    };

    let tn_accounts: Vec<_> = accounts
        .into_iter()
        .filter(|m| m.broker == BrokerKind::TradeNation)
        .collect();
    rlog!(
        "cron session_refresh: {} TN accounts to consider",
        tn_accounts.len()
    );

    for meta in tn_accounts {
        let name = &meta.name;
        let meta_key = super::session_meta::key(name);
        let cached_at = match kv.get(&meta_key).text().await {
            Ok(Some(json)) => match serde_json::from_str::<super::session_meta::SessionMeta>(&json)
            {
                Ok(m) => Some(m.cached_at),
                Err(err) => {
                    rlog_err!(
                        "cron session_refresh[{name}]: parse session_meta: {err} — treating as stale"
                    );
                    None
                }
            },
            Ok(None) => None,
            Err(err) => {
                rlog_err!(
                    "cron session_refresh[{name}]: KV get session_meta: {err:?} — treating as stale"
                );
                None
            }
        };

        let stale = match cached_at {
            Some(ts) => is_stale(ts, now, threshold),
            None => true,
        };
        if !stale {
            rlog!("cron session_refresh[{name}]: fresh");
            continue;
        }

        match crate::acquire_tn_broker(env, Some(name)).await {
            Some(_) => rlog!("cron session_refresh[{name}]: refreshed"),
            None => rlog_err!("cron session_refresh[{name}]: error (see prior logs)"),
        }
    }
}

/// Pure staleness predicate. A future `cached_at` (clock skew) is
/// treated as fresh — we don't want to thrash on log-in retries just
/// because the cron worker's clock briefly lags the KV-write worker.
///
/// `dead_code` allowed for the same reason as [`super::session_meta`]:
/// the live consumer is wasm-only; native builds only call this from
/// tests, which don't count as live use sites for rustc.
#[allow(dead_code)]
pub fn is_stale(cached_at: DateTime<Utc>, now: DateTime<Utc>, threshold: Duration) -> bool {
    now - cached_at > threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, 0, 0).unwrap()
    }

    #[test]
    fn just_now_is_fresh() {
        let now = t(2026, 5, 28, 12);
        assert!(!is_stale(now, now, Duration::hours(12)));
    }

    #[test]
    fn exactly_at_threshold_is_fresh() {
        // The predicate uses `>`, so equality on the boundary is fresh.
        let now = t(2026, 5, 28, 12);
        let cached = t(2026, 5, 28, 0);
        assert!(!is_stale(cached, now, Duration::hours(12)));
    }

    #[test]
    fn one_second_past_threshold_is_stale() {
        let now = t(2026, 5, 28, 12) + Duration::seconds(1);
        let cached = t(2026, 5, 28, 0);
        assert!(is_stale(cached, now, Duration::hours(12)));
    }

    #[test]
    fn well_past_threshold_is_stale() {
        let now = t(2026, 5, 30, 0);
        let cached = t(2026, 5, 28, 0);
        assert!(is_stale(cached, now, Duration::hours(12)));
    }

    #[test]
    fn future_cached_at_is_not_stale() {
        // Clock skew between workers shouldn't cause unnecessary
        // re-logins. Negative `(now - cached_at)` is not `> threshold`.
        let now = t(2026, 5, 28, 0);
        let cached = t(2026, 5, 28, 1);
        assert!(!is_stale(cached, now, Duration::hours(12)));
    }

    #[test]
    fn schema_compat_meta_round_trip_via_predicate() {
        // SessionMeta JSON round-trips and the resulting timestamp can
        // be fed into `is_stale` for a fresh result.
        let cached_at = t(2026, 5, 28, 11);
        let now = t(2026, 5, 28, 12);
        let meta = super::super::session_meta::SessionMeta { cached_at };
        let json = serde_json::to_string(&meta).unwrap();
        let back: super::super::session_meta::SessionMeta = serde_json::from_str(&json).unwrap();
        assert!(!is_stale(back.cached_at, now, Duration::hours(12)));
    }

    #[test]
    fn missing_meta_treated_as_stale_when_decoded_as_none() {
        // The wasm path converts `Ok(None)` from the KV get into
        // `cached_at: None`, then falls through to `stale = true`. This
        // test pins that contract at the predicate-level: there's no
        // "fresh because missing" path.
        let cached_at: Option<DateTime<Utc>> = None;
        let now = t(2026, 5, 28, 12);
        let stale = match cached_at {
            Some(ts) => is_stale(ts, now, Duration::hours(12)),
            None => true,
        };
        assert!(stale);
    }
}
