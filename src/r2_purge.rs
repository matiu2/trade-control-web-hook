//! R2 housekeeping for the recording bucket.
//!
//! The worker records two kinds of JSON bundle to the R2 bucket bound as
//! [`crate::recording::R2_BINDING`]:
//!
//! - `req/<YYYY-MM-DD>/<ts>-<request_id>.json`   — inbound request bundles.
//!   The key suffix is a body-hash, **not** a trade-id.
//! - `ticks/<YYYY-MM-DD>/<ts>-<trade_id>.json`   — engine tick bundles.
//!   The key suffix **is** the trade-id, immediately before `.json`.
//!
//! This module provides two purges over those objects:
//!
//! 1. [`purge_trade_ticks`] — drop every tick bundle for one trade-id (e.g.
//!    when a plan is purged and its replay history is no longer wanted).
//! 2. [`purge_older_than`] — retention sweep across both prefixes, dropping
//!    anything whose date-partition is strictly before a cutoff date.
//!
//! Both are **fail-soft**: a missing bucket binding logs and returns `Ok(0)`,
//! and an individual delete failure is logged and skipped rather than aborting
//! the whole sweep. The intent is that housekeeping never takes down a request.

use chrono::{DateTime, NaiveDate, Utc};
use worker::console_log;

use crate::recording::R2_BINDING;

/// The request-bundle prefix (`req/<YYYY-MM-DD>/...`).
const REQ_PREFIX: &str = "req/";
/// The engine tick-bundle prefix (`ticks/<YYYY-MM-DD>/...`).
const TICKS_PREFIX: &str = "ticks/";

/// Delete every tick bundle in `ticks/` whose key belongs to `trade_id`.
///
/// Pages the R2 list cursor until exhausted, deleting any object whose key ends
/// in `-<trade_id>.json`. Returns the number of objects deleted.
///
/// Fail-soft: if the bucket binding is missing this logs and returns `Ok(0)`.
/// A single delete failure is logged and counted as a skip; the sweep continues.
pub async fn purge_trade_ticks(env: &worker::Env, trade_id: &str) -> Result<usize, String> {
    let bucket = match env.bucket(R2_BINDING) {
        Ok(bucket) => bucket,
        Err(err) => {
            console_log!("r2_purge: bucket binding '{R2_BINDING}' unavailable: {err}");
            return Ok(0);
        }
    };

    let keys = list_keys(&bucket, TICKS_PREFIX).await?;
    let mut deleted = 0usize;
    for key in keys {
        if key_is_for_trade(&key, trade_id) {
            deleted += delete_key(&bucket, &key).await;
        }
    }

    console_log!("r2_purge: purged {deleted} tick bundle(s) for trade_id '{trade_id}'");
    Ok(deleted)
}

/// Retention sweep across both `req/` and `ticks/` prefixes.
///
/// Deletes every object whose date-partition segment (`<prefix>/<YYYY-MM-DD>/...`)
/// is strictly before `cutoff`'s UTC date. Objects with an unparseable date
/// segment are left untouched. Returns the total number of objects deleted.
///
/// Fail-soft: a missing bucket binding logs and returns `Ok(0)`; individual
/// delete failures are logged and skipped.
pub async fn purge_older_than(env: &worker::Env, cutoff: DateTime<Utc>) -> Result<usize, String> {
    let bucket = match env.bucket(R2_BINDING) {
        Ok(bucket) => bucket,
        Err(err) => {
            console_log!("r2_purge: bucket binding '{R2_BINDING}' unavailable: {err}");
            return Ok(0);
        }
    };

    let cutoff_date = cutoff.date_naive();
    let mut deleted = 0usize;

    for prefix in [REQ_PREFIX, TICKS_PREFIX] {
        let keys = list_keys(&bucket, prefix).await?;
        for key in keys {
            match key_date(&key) {
                Some(date) if date < cutoff_date => {
                    deleted += delete_key(&bucket, &key).await;
                }
                _ => {}
            }
        }
    }

    console_log!("r2_purge: retention sweep deleted {deleted} object(s) older than {cutoff_date}");
    Ok(deleted)
}

/// List every object key under `prefix`, paging the cursor until not truncated.
async fn list_keys(bucket: &worker::Bucket, prefix: &str) -> Result<Vec<String>, String> {
    let mut keys = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let mut request = bucket.list().prefix(prefix.to_string());
        if let Some(cursor) = cursor.clone() {
            request = request.cursor(cursor);
        }

        let objects = request
            .execute()
            .await
            .map_err(|err| format!("r2_purge: list '{prefix}' failed: {err}"))?;

        for object in objects.objects() {
            keys.push(object.key());
        }

        if objects.truncated() {
            cursor = objects.cursor();
            // Defensive: a truncated page with no cursor would loop forever.
            if cursor.is_none() {
                break;
            }
        } else {
            break;
        }
    }

    Ok(keys)
}

/// Delete one key, returning `1` on success and `0` (with a log) on failure.
async fn delete_key(bucket: &worker::Bucket, key: &str) -> usize {
    match bucket.delete(key).await {
        Ok(()) => 1,
        Err(err) => {
            console_log!("r2_purge: delete '{key}' failed (skipped): {err}");
            0
        }
    }
}

/// Extract the `YYYY-MM-DD` date partition from a recording key.
///
/// The date is the path segment between the first and second `/` — i.e. the
/// segment immediately after the prefix. Returns `None` if the key has too few
/// segments or the segment is not a valid date.
///
/// ```text
/// ticks/2026-06-24/2026-06-24T01:00:00+00:00-hs-1.json -> 2026-06-24
/// ```
fn key_date(key: &str) -> Option<NaiveDate> {
    let mut segments = key.split('/');
    let _prefix = segments.next()?;
    let date_segment = segments.next()?;
    // Require at least one more segment so a bare `prefix/date` (no object) is
    // not mistaken for a dated object.
    segments.next()?;
    NaiveDate::parse_from_str(date_segment, "%Y-%m-%d").ok()
}

/// Whether `key` is a tick bundle belonging to `trade_id`.
///
/// Matches the literal `-<trade_id>.json` suffix, so a trade-id that is a
/// prefix of a longer one (e.g. `hs-1` vs `hs-10`) does **not** match.
fn key_is_for_trade(key: &str, trade_id: &str) -> bool {
    let suffix = format!("-{trade_id}.json");
    key.ends_with(&suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_date_extracts_partition() {
        let key = "ticks/2026-06-24/2026-06-24T01:00:00+00:00-hs-1.json";
        assert_eq!(
            key_date(key),
            Some(NaiveDate::from_ymd_opt(2026, 6, 24).expect("valid date"))
        );
    }

    #[test]
    fn key_date_handles_req_prefix() {
        let key = "req/2025-12-31/1735603200-abc123def.json";
        assert_eq!(
            key_date(key),
            Some(NaiveDate::from_ymd_opt(2025, 12, 31).expect("valid date"))
        );
    }

    #[test]
    fn key_date_rejects_bad_date() {
        assert_eq!(key_date("ticks/not-a-date/foo.json"), None);
    }

    #[test]
    fn key_date_rejects_short_key() {
        // No object segment after the date partition.
        assert_eq!(key_date("ticks/2026-06-24"), None);
        // No date partition at all.
        assert_eq!(key_date("ticks/"), None);
        assert_eq!(key_date("ticks"), None);
    }

    #[test]
    fn key_is_for_trade_matches_exact_suffix() {
        let key = "ticks/2026-06-24/2026-06-24T01:00:00+00:00-hs-1.json";
        assert!(key_is_for_trade(key, "hs-1"));
    }

    #[test]
    fn key_is_for_trade_rejects_prefix_collision() {
        // "hs-1" must NOT match a key ending in "-hs-10.json".
        let key = "ticks/2026-06-24/2026-06-24T01:00:00+00:00-hs-10.json";
        assert!(!key_is_for_trade(key, "hs-1"));
        assert!(key_is_for_trade(key, "hs-10"));
    }

    #[test]
    fn key_is_for_trade_rejects_non_json() {
        let key = "ticks/2026-06-24/2026-06-24T01:00:00+00:00-hs-1.txt";
        assert!(!key_is_for_trade(key, "hs-1"));
    }

    #[test]
    fn key_is_for_trade_rejects_other_trade() {
        let key = "ticks/2026-06-24/2026-06-24T01:00:00+00:00-mw-7.json";
        assert!(!key_is_for_trade(key, "hs-1"));
    }
}
