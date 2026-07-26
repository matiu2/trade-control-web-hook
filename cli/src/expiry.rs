//! Per-instrument trade-expiry anchor stored under
//! `$XDG_CONFIG_HOME/trade-control/expiry/<INSTRUMENT>.txt`.
//!
//! The anchor is purely a CLI-side default — the worker has no opinion
//! about it. When the operator declares a `trade-expiry` veto, we stash
//! the timestamp so later prep/veto/enter prompts can pre-fill
//! `ttl_hours` and `not_after` against it. A stale anchor (in the past)
//! is silently dropped and the caller falls back to the 2-day default.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use color_eyre::eyre::{Result, eyre};

/// Default fallback when no anchor is stored or the stored anchor has
/// already passed.
pub const DEFAULT_HORIZON: Duration = Duration::hours(48);

/// Directory holding `<INSTRUMENT>.txt` anchor files. Honors
/// `XDG_CONFIG_HOME`, otherwise falls back to `~/.config`.
pub fn expiry_root() -> Result<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        let home = std::env::var("HOME").map_err(|_| eyre!("HOME not set"))?;
        PathBuf::from(home).join(".config")
    };
    Ok(base.join("trade-control").join("expiry"))
}

/// The anchor file for `instrument` under an explicit root. Instrument names are
/// upper-cased so `gbpjpy` and `GBPJPY` share one anchor.
///
/// (There was an env-resolving `anchor_path` wrapper alongside this; nothing
/// called it once `load`/`save` delegated to the `_in` variants, so it's gone.)
fn anchor_path_in(root: &Path, instrument: &str) -> PathBuf {
    root.join(format!("{}.txt", instrument.to_uppercase()))
}

/// Load the stored anchor for `instrument`. Returns `None` if:
///   - no file exists,
///   - the file is unparseable, or
///   - the stored timestamp is in the past (the file is deleted).
pub fn load(instrument: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    load_in(&expiry_root().ok()?, instrument, now)
}

/// [`load`] against an explicit root.
///
/// Tests use this instead of overriding `$XDG_CONFIG_HOME`. That override was
/// `unsafe` (a `set_var` races any concurrent `getenv` in the process, including
/// from libc or a tokio worker — not just other tests), it was process-wide while
/// the mutex guarding it was module-local, and it made this module's tests flaky
/// at ~5-10% because `interactive.rs` had a *second*, separate mutex over the same
/// variable. Passing the root removes the shared mutable state entirely.
pub fn load_in(root: &Path, instrument: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let path = anchor_path_in(root, instrument);
    let raw = fs::read_to_string(&path).ok()?;
    let parsed: DateTime<Utc> = raw.trim().parse().ok()?;
    if parsed <= now {
        let _ = fs::remove_file(&path);
        return None;
    }
    Some(parsed)
}

/// Persist `anchor` for `instrument`. Creates the parent directory if
/// it doesn't exist yet.
pub fn save(instrument: &str, anchor: DateTime<Utc>) -> Result<()> {
    save_in(&expiry_root()?, instrument, anchor)
}

/// [`save`] against an explicit root. See [`load_in`] for why this exists.
pub fn save_in(root: &Path, instrument: &str, anchor: DateTime<Utc>) -> Result<()> {
    let path = anchor_path_in(root, instrument);
    fs::create_dir_all(root).map_err(|e| eyre!("creating {}: {e}", root.display()))?;
    fs::write(&path, anchor.to_rfc3339()).map_err(|e| eyre!("writing {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A private root per test. No env var, no mutex, no shared state — so these
    /// run in parallel with everything else and clean up on drop.
    ///
    /// This replaced an `isolated_root` helper that did `unsafe
    /// set_var("XDG_CONFIG_HOME", …)` behind a module-local mutex. Three problems
    /// with that: the variable is **process-wide** while the mutex only covered
    /// this module; `interactive.rs` had a *second, separate* mutex over the same
    /// variable (its comment claimed the mutex was shared — it wasn't, and two
    /// different mutexes give zero mutual exclusion), which made
    /// `instrument_name_is_uppercased` fail ~5-10% of full-suite runs; and
    /// `set_var` is `unsafe` for a real reason — it races any concurrent `getenv`
    /// anywhere in the process, including from libc or a tokio worker.
    ///
    /// It also leaked: the old helper keyed its dir on the PID and only cleaned up
    /// on the way *in*, so `/tmp` accumulated a dir per test per run.
    fn root() -> TempDir {
        TempDir::new().unwrap()
    }

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = root();
        let anchor = ts("2026-05-22T14:00:00Z");
        save_in(dir.path(), "GBPJPY", anchor).unwrap();
        assert_eq!(
            load_in(dir.path(), "GBPJPY", ts("2026-05-18T10:00:00Z")),
            Some(anchor)
        );
    }

    #[test]
    fn load_returns_none_for_missing_instrument() {
        let dir = root();
        assert!(load_in(dir.path(), "EURUSD", ts("2026-05-18T10:00:00Z")).is_none());
    }

    #[test]
    fn load_drops_stale_anchor() {
        let dir = root();
        save_in(dir.path(), "USDJPY", ts("2026-05-10T10:00:00Z")).unwrap();
        assert!(load_in(dir.path(), "USDJPY", ts("2026-05-18T10:00:00Z")).is_none());
        // The stale file is deleted, not just ignored.
        assert!(!anchor_path_in(dir.path(), "USDJPY").exists());
    }

    #[test]
    fn instrument_name_is_uppercased() {
        let dir = root();
        let now = ts("2026-05-18T10:00:00Z");
        let anchor = ts("2026-05-22T14:00:00Z");
        save_in(dir.path(), "gbpjpy", anchor).unwrap();
        assert_eq!(load_in(dir.path(), "GBPJPY", now), Some(anchor));
        assert_eq!(load_in(dir.path(), "gbpjpy", now), Some(anchor));
    }

    #[test]
    fn load_falls_back_to_horizon_via_caller() {
        // Callers do `load(...).unwrap_or(now + DEFAULT_HORIZON)` —
        // sanity check that both halves are wired.
        let dir = root();
        let now = ts("2026-05-18T10:00:00Z");
        let fallback = load_in(dir.path(), "EURUSD", now).unwrap_or(now + DEFAULT_HORIZON);
        assert_eq!(fallback, now + DEFAULT_HORIZON);
    }

    /// `save_in` creates the root if it doesn't exist yet (the production `save`
    /// relies on this for a first-ever anchor).
    #[test]
    fn save_creates_a_missing_root() {
        let dir = root();
        let nested = dir.path().join("trade-control").join("expiry");
        assert!(!nested.exists());
        let anchor = ts("2026-05-22T14:00:00Z");
        save_in(&nested, "AUDUSD", anchor).unwrap();
        assert_eq!(
            load_in(&nested, "AUDUSD", ts("2026-05-18T10:00:00Z")),
            Some(anchor)
        );
    }

    /// The env-resolving wrapper still composes the documented path
    /// (`$XDG_CONFIG_HOME/trade-control/expiry/<INSTRUMENT>.txt`). Read-only — it
    /// never touches the filesystem, so it needs no isolation.
    #[test]
    fn expiry_root_honours_xdg_config_home() {
        let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") else {
            // Unset in this environment: the HOME fallback is exercised instead.
            let root = expiry_root().unwrap();
            assert!(root.ends_with("trade-control/expiry"), "got {root:?}");
            return;
        };
        assert_eq!(
            expiry_root().unwrap(),
            PathBuf::from(xdg).join("trade-control").join("expiry")
        );
    }
}
