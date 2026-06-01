//! Disk cache for forex-factory weekly fetches.
//!
//! `CalendarService::get_week_events_for(date)` is a network round-trip
//! that returns the events for the ISO week containing `date`. Every
//! `tv-arm` run hits it at least once (often for several adjacent weeks
//! via `fetch_events_for_range`), and the same week's data is identical
//! across consecutive runs.
//!
//! This module wraps that call with a system-wide on-disk cache so
//! repeat fetches inside the TTL window read JSON from
//! `~/.cache/tv-arm/forex-factory/<YYYY>-W<WW>.json` instead of going
//! over the network.
//!
//! Cache key: the Monday of the requested date's ISO week.
//! TTL: 4 weeks from file mtime. Past that we refetch and overwrite.
//! Older trades replayed for backtests still benefit — once a week is
//! fetched its file lasts a month, and a manual `rm` busts it
//! deliberately if upstream data was wrong.
//!
//! Cache misses or unreadable cache files fall through to the live
//! fetch and try to populate the cache; write failures only log and
//! don't fail the call.
//!
//! Test seam: pass a custom `dir` and `now` to [`get_week_events_in`]
//! so tests don't touch `~/.cache/`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{Datelike, NaiveDate};
use color_eyre::eyre::{Result, eyre};
use forex_factory::{CalendarService, EconomicEvent};

/// Cache TTL: 4 weeks. After this, the file is considered stale and we
/// refetch. We don't expire by week date — older replay runs benefit
/// from the cache for the full TTL after first fetch.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 28);

/// Public entrypoint: fetch the events for the ISO week containing
/// `date`, reading from / writing to the user's default cache dir.
///
/// Equivalent to `CalendarService::get_week_events_for(date)` with a
/// 4-week disk cache in front.
pub async fn get_week_events_cached(date: NaiveDate) -> Result<Vec<EconomicEvent>> {
    let dir = default_cache_dir()?;
    get_week_events_in(date, &dir, SystemTime::now(), DEFAULT_TTL, &live_fetch).await
}

/// Test-friendly variant. Lets the caller override the cache dir, the
/// notion of "now" (for TTL math), the TTL itself, and the live fetch
/// closure so unit tests can verify the cache without a network.
pub async fn get_week_events_in<F, Fut>(
    date: NaiveDate,
    dir: &Path,
    now: SystemTime,
    ttl: Duration,
    fetch: &F,
) -> Result<Vec<EconomicEvent>>
where
    F: Fn(NaiveDate) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<EconomicEvent>>>,
{
    let monday = monday_of_week(date);
    let path = cache_path_in(dir, monday);

    if let Some(events) = read_if_fresh(&path, now, ttl)? {
        tracing::debug!(week = %iso_week_label(monday), path = %path.display(), "forex-factory cache hit");
        return Ok(events);
    }

    tracing::debug!(week = %iso_week_label(monday), path = %path.display(), "forex-factory cache miss, fetching");
    let events = fetch(monday).await?;
    if let Err(e) = write_cache(&path, &events) {
        tracing::warn!(error = %e, path = %path.display(), "failed to write forex-factory cache (continuing)");
    }
    Ok(events)
}

/// Live fetcher used by [`get_week_events_cached`]. Separate so tests
/// can swap it.
async fn live_fetch(date: NaiveDate) -> Result<Vec<EconomicEvent>> {
    let service =
        CalendarService::new().map_err(|e| eyre!("creating forex-factory CalendarService: {e}"))?;
    service
        .get_week_events_for(date)
        .await
        .map_err(|e| eyre!("fetching week events for {date}: {e}"))
}

/// `~/.cache/tv-arm/forex-factory/` (honours `$XDG_CACHE_HOME`).
fn default_cache_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or_else(|| eyre!("can't determine cache dir: neither $XDG_CACHE_HOME nor $HOME set"))?;
    Ok(base.join("tv-arm").join("forex-factory"))
}

/// Monday of `date`'s ISO week. ISO weeks start on Monday, so this
/// just walks back to the previous Monday (or stays put if `date` is
/// already a Monday).
fn monday_of_week(date: NaiveDate) -> NaiveDate {
    let offset = date.weekday().num_days_from_monday() as i64;
    date - chrono::Duration::days(offset)
}

/// Cache filename for a given Monday-anchored week: `YYYY-Www.json`,
/// where `YYYY-Www` is the ISO-week label of that Monday (e.g.
/// `2026-W22.json`).
fn cache_path_in(dir: &Path, monday: NaiveDate) -> PathBuf {
    dir.join(format!("{}.json", iso_week_label(monday)))
}

/// `YYYY-Www` — ISO-week label, zero-padded week number.
fn iso_week_label(monday: NaiveDate) -> String {
    let iso = monday.iso_week();
    format!("{}-W{:02}", iso.year(), iso.week())
}

/// Read the cache file if present and within TTL; return its parsed
/// contents. Returns `Ok(None)` on missing file or stale file (so the
/// caller knows to fetch). Bubble up only "file exists but unreadable
/// JSON" via the warn-and-miss path.
fn read_if_fresh(
    path: &Path,
    now: SystemTime,
    ttl: Duration,
) -> Result<Option<Vec<EconomicEvent>>> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(eyre!("stat {}: {e}", path.display())),
    };
    let mtime = metadata
        .modified()
        .map_err(|e| eyre!("mtime of {}: {e}", path.display()))?;
    let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
    if age > ttl {
        return Ok(None);
    }
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "reading forex-factory cache failed; refetching");
            return Ok(None);
        }
    };
    match serde_json::from_slice::<Vec<EconomicEvent>>(&raw) {
        Ok(events) => Ok(Some(events)),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "forex-factory cache file unparseable; refetching");
            Ok(None)
        }
    }
}

/// Write events to `path` as pretty JSON, creating parent dirs as
/// needed. Atomic via tmp-file + rename so a concurrent reader never
/// sees a half-written file.
fn write_cache(path: &Path, events: &[EconomicEvent]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| eyre!("creating cache dir {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(events)
        .map_err(|e| eyre!("serializing {} events: {e}", events.len()))?;
    std::fs::write(&tmp, &body).map_err(|e| eyre!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| eyre!("renaming {} -> {}: {e}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::{Local, NaiveDate, TimeZone};
    use forex_factory::Impact;

    use super::*;

    fn mk_event(name: &str, ts_secs: i64) -> EconomicEvent {
        EconomicEvent {
            datetime: Local.timestamp_opt(ts_secs, 0).unwrap(),
            currency: "USD".into(),
            impact: Impact::High,
            name: name.into(),
            actual: None,
            forecast: None,
            previous: None,
        }
    }

    #[test]
    fn monday_of_week_rounds_back() {
        let wed = NaiveDate::from_ymd_opt(2026, 5, 27).unwrap();
        let mon = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        assert_eq!(monday_of_week(wed), mon);
        // Sunday → previous Monday (ISO weeks end on Sunday).
        let sun = NaiveDate::from_ymd_opt(2026, 5, 31).unwrap();
        assert_eq!(monday_of_week(sun), mon);
        // Monday stays put.
        assert_eq!(monday_of_week(mon), mon);
    }

    #[test]
    fn iso_week_label_zero_pads() {
        // 2026-W22 is the week of 2026-05-25 (Mon).
        let mon = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        assert_eq!(iso_week_label(mon), "2026-W22");
        // Week 1 must zero-pad.
        let early = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        assert_eq!(iso_week_label(early), "2026-W02");
    }

    #[test]
    fn cache_path_in_uses_iso_label() {
        let dir = PathBuf::from("/tmp/cache");
        let mon = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        assert_eq!(cache_path_in(&dir, mon), dir.join("2026-W22.json"));
    }

    #[tokio::test]
    async fn miss_then_hit() {
        let tmp = tempdir();
        let calls = AtomicUsize::new(0);
        let fetch = |_: NaiveDate| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok(vec![mk_event("CPI", 1_700_000_000)]) }
        };
        let date = NaiveDate::from_ymd_opt(2026, 5, 27).unwrap();
        let now = SystemTime::now();

        let first = get_week_events_in(date, tmp.path(), now, DEFAULT_TTL, &fetch)
            .await
            .unwrap();
        let second = get_week_events_in(date, tmp.path(), now, DEFAULT_TTL, &fetch)
            .await
            .unwrap();

        assert_eq!(first.len(), 1);
        assert_eq!(first, second, "cache must return identical events");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second call must be served from cache"
        );
    }

    #[tokio::test]
    async fn stale_file_refetches() {
        let tmp = tempdir();
        let calls = AtomicUsize::new(0);
        let fetch = |_: NaiveDate| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok(vec![mk_event("NFP", 1_700_000_000 + n as i64)]) }
        };
        let date = NaiveDate::from_ymd_opt(2026, 5, 27).unwrap();
        let now = SystemTime::now();
        let ttl = Duration::from_secs(60);

        // Prime the cache.
        let _ = get_week_events_in(date, tmp.path(), now, ttl, &fetch)
            .await
            .unwrap();
        // Fast-forward "now" past TTL — the cache file's mtime is
        // ~now, so now + 2*ttl reads as stale.
        let later = now + Duration::from_secs(2 * 60);
        let second = get_week_events_in(date, tmp.path(), later, ttl, &fetch)
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2, "stale entry must refetch");
        assert_eq!(second[0].name, "NFP");
    }

    #[tokio::test]
    async fn corrupted_cache_falls_through() {
        let tmp = tempdir();
        let date = NaiveDate::from_ymd_opt(2026, 5, 27).unwrap();
        let monday = monday_of_week(date);
        let path = cache_path_in(tmp.path(), monday);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json at all").unwrap();

        let calls = AtomicUsize::new(0);
        let fetch = |_: NaiveDate| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok(vec![mk_event("PMI", 1_700_000_500)]) }
        };
        let events = get_week_events_in(date, tmp.path(), SystemTime::now(), DEFAULT_TTL, &fetch)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // The cache should have been overwritten with a valid JSON
        // file — second call now serves from cache.
        let _ = get_week_events_in(date, tmp.path(), SystemTime::now(), DEFAULT_TTL, &fetch)
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "valid rewrite should be reused"
        );
    }

    /// Lightweight scoped tempdir — we don't want to pull the `tempfile`
    /// crate in just for this. Drops on scope exit.
    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    impl Tmp {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    fn tempdir() -> Tmp {
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!("ff-cache-test-{pid}-{nanos}"));
        std::fs::create_dir_all(&base).unwrap();
        Tmp(base)
    }
}
