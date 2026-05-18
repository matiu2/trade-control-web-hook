//! Per-instrument trade-expiry anchor stored under
//! `$XDG_CONFIG_HOME/trade-control/expiry/<INSTRUMENT>.txt`.
//!
//! The anchor is purely a CLI-side default — the worker has no opinion
//! about it. When the operator declares a `trade-expiry` veto, we stash
//! the timestamp so later prep/veto/enter prompts can pre-fill
//! `ttl_hours` and `not_after` against it. A stale anchor (in the past)
//! is silently dropped and the caller falls back to the 2-day default.

use std::fs;
use std::path::PathBuf;

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

fn anchor_path(instrument: &str) -> Result<PathBuf> {
    let name = instrument.to_uppercase();
    Ok(expiry_root()?.join(format!("{name}.txt")))
}

/// Load the stored anchor for `instrument`. Returns `None` if:
///   - no file exists,
///   - the file is unparseable, or
///   - the stored timestamp is in the past (the file is deleted).
pub fn load(instrument: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let path = anchor_path(instrument).ok()?;
    let raw = fs::read_to_string(&path).ok()?;
    let trimmed = raw.trim();
    let parsed: DateTime<Utc> = trimmed.parse().ok()?;
    if parsed <= now {
        let _ = fs::remove_file(&path);
        return None;
    }
    Some(parsed)
}

/// Persist `anchor` for `instrument`. Creates the parent directory if
/// it doesn't exist yet.
pub fn save(instrument: &str, anchor: DateTime<Utc>) -> Result<()> {
    let path = anchor_path(instrument)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| eyre!("creating {}: {e}", parent.display()))?;
    }
    fs::write(&path, anchor.to_rfc3339()).map_err(|e| eyre!("writing {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests in this module mutate `$XDG_CONFIG_HOME` and the shared
    /// temp directory under it. Cargo runs tests in parallel by default,
    /// so we serialize through a process-wide mutex.
    static GUARD: Mutex<()> = Mutex::new(());

    /// Override `$XDG_CONFIG_HOME` for the duration of the test so the
    /// real config dir is left alone.
    fn isolated_root(tag: &str) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "trade-control-expiry-test-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // SAFETY: serialized via GUARD; only this module's tests touch
        // XDG_CONFIG_HOME.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        (dir, guard)
    }

    #[test]
    fn save_then_load_round_trips() {
        let (_root, _g) = isolated_root("round-trip");
        let now: DateTime<Utc> = "2026-05-18T10:00:00Z".parse().unwrap();
        let anchor: DateTime<Utc> = "2026-05-22T14:00:00Z".parse().unwrap();
        save("GBPJPY", anchor).unwrap();
        let loaded = load("GBPJPY", now).unwrap();
        assert_eq!(loaded, anchor);
    }

    #[test]
    fn load_returns_none_for_missing_instrument() {
        let (_root, _g) = isolated_root("missing");
        let now: DateTime<Utc> = "2026-05-18T10:00:00Z".parse().unwrap();
        assert!(load("EURUSD", now).is_none());
    }

    #[test]
    fn load_drops_stale_anchor() {
        let (_root, _g) = isolated_root("stale");
        let stale: DateTime<Utc> = "2026-05-10T10:00:00Z".parse().unwrap();
        save("USDJPY", stale).unwrap();
        let now: DateTime<Utc> = "2026-05-18T10:00:00Z".parse().unwrap();
        assert!(load("USDJPY", now).is_none());
        let path = anchor_path("USDJPY").unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn instrument_name_is_uppercased() {
        let (_root, _g) = isolated_root("case");
        let now: DateTime<Utc> = "2026-05-18T10:00:00Z".parse().unwrap();
        let anchor: DateTime<Utc> = "2026-05-22T14:00:00Z".parse().unwrap();
        save("gbpjpy", anchor).unwrap();
        assert_eq!(load("GBPJPY", now), Some(anchor));
        assert_eq!(load("gbpjpy", now), Some(anchor));
    }

    #[test]
    fn load_falls_back_to_horizon_via_caller() {
        // Callers do `load(...).unwrap_or(now + DEFAULT_HORIZON)` —
        // sanity check that both halves are wired.
        let (_root, _g) = isolated_root("fallback");
        let now: DateTime<Utc> = "2026-05-18T10:00:00Z".parse().unwrap();
        let fallback = load("EURUSD", now).unwrap_or(now + DEFAULT_HORIZON);
        assert_eq!(fallback, now + DEFAULT_HORIZON);
    }
}
