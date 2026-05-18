//! Persistent recall of recently-used prep / veto names.
//!
//! When the operator types a `requires_preps` or `vetos` list in the
//! `encrypt` flow, typos silently break the gate (the worker can't find a
//! prep named `break_and_close` if the prep was set as `break-and-close`).
//! To dodge that, every time we fire a `prep` / `veto` action — or record
//! one in an `enter` template — we append the name to a small history
//! file. Subsequent prompts suggest these names, most-recent-first.
//!
//! Storage is `~/.config/trade-control/history.yaml` (or
//! `$XDG_CONFIG_HOME/trade-control/history.yaml` if set). Format is a
//! YAML mapping with two keys (`preps`, `vetos`) each holding a list of
//! `{ name, last_used }` entries. Capped at 50 entries per list,
//! evicting oldest.
//!
//! Failures are non-fatal. A broken history file just means no
//! suggestions — the CLI still works.
//!
//! See also: the design note in
//! `~/.home-claude/plans/have-a-look-at-cozy-plum.md` (Feature 1).

use std::fs;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Serialize};

/// Maximum number of entries kept per list. Old entries fall off the end.
const MAX_ENTRIES: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub name: String,
    pub last_used: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History {
    #[serde(default)]
    pub preps: Vec<HistoryEntry>,
    #[serde(default)]
    pub vetos: Vec<HistoryEntry>,
}

impl History {
    pub fn prep_names(&self) -> Vec<String> {
        self.preps.iter().map(|e| e.name.clone()).collect()
    }

    pub fn veto_names(&self) -> Vec<String> {
        self.vetos.iter().map(|e| e.name.clone()).collect()
    }

    pub fn record_prep(&mut self, name: &str, now: DateTime<Utc>) {
        record(&mut self.preps, name, now);
    }

    pub fn record_veto(&mut self, name: &str, now: DateTime<Utc>) {
        record(&mut self.vetos, name, now);
    }
}

fn record(list: &mut Vec<HistoryEntry>, name: &str, now: DateTime<Utc>) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    list.retain(|e| e.name != name);
    list.insert(
        0,
        HistoryEntry {
            name: name.to_string(),
            last_used: now,
        },
    );
    list.truncate(MAX_ENTRIES);
}

/// Resolve the history file path. Honors `XDG_CONFIG_HOME` if set;
/// otherwise falls back to `~/.config/trade-control/history.yaml`.
pub fn history_path() -> Result<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        let home = std::env::var("HOME").map_err(|_| eyre!("HOME not set"))?;
        PathBuf::from(home).join(".config")
    };
    Ok(base.join("trade-control").join("history.yaml"))
}

/// Load history from disk. Missing file or parse errors yield a default
/// (empty) `History` — recall is best-effort.
pub fn load() -> History {
    let path = match history_path() {
        Ok(p) => p,
        Err(_) => return History::default(),
    };
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return History::default(),
    };
    serde_yaml::from_str(&text).unwrap_or_default()
}

/// Persist `history` to disk, creating the parent directory if needed.
pub fn save(history: &History) -> Result<()> {
    let path = history_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| eyre!("creating {}: {e}", parent.display()))?;
    }
    let yaml = serde_yaml::to_string(history).map_err(|e| eyre!("serialising history: {e}"))?;
    fs::write(&path, yaml).map_err(|e| eyre!("writing {}: {e}", path.display()))?;
    Ok(())
}

/// Convenience: load, append a prep name, save. Swallows errors.
pub fn record_prep_use(name: &str) {
    let mut h = load();
    h.record_prep(name, Utc::now());
    let _ = save(&h);
}

/// Convenience: load, append a veto name, save. Swallows errors.
pub fn record_veto_use(name: &str) {
    let mut h = load();
    h.record_veto(name, Utc::now());
    let _ = save(&h);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn record_prep_inserts_at_front() {
        let mut h = History::default();
        h.record_prep("retest", t("2026-05-15T12:00:00Z"));
        h.record_prep("break-and-close", t("2026-05-15T12:01:00Z"));
        assert_eq!(h.prep_names(), vec!["break-and-close", "retest"]);
    }

    #[test]
    fn record_prep_dedupes_and_promotes() {
        let mut h = History::default();
        h.record_prep("retest", t("2026-05-15T12:00:00Z"));
        h.record_prep("break-and-close", t("2026-05-15T12:01:00Z"));
        h.record_prep("retest", t("2026-05-15T12:02:00Z"));
        // Old `retest` removed, new one is at front.
        assert_eq!(h.prep_names(), vec!["retest", "break-and-close"]);
        assert_eq!(h.preps.len(), 2);
    }

    #[test]
    fn record_ignores_blank_names() {
        let mut h = History::default();
        h.record_prep("   ", t("2026-05-15T12:00:00Z"));
        h.record_prep("", t("2026-05-15T12:01:00Z"));
        assert!(h.preps.is_empty());
    }

    #[test]
    fn record_caps_at_max_entries() {
        let mut h = History::default();
        let base = t("2026-05-15T12:00:00Z");
        for i in 0..(MAX_ENTRIES + 5) {
            h.record_prep(&format!("step-{i}"), base);
        }
        assert_eq!(h.preps.len(), MAX_ENTRIES);
        // Newest first: step-54 .. step-5
        assert_eq!(h.preps[0].name, format!("step-{}", MAX_ENTRIES + 4));
    }

    #[test]
    fn record_trims_whitespace() {
        let mut h = History::default();
        h.record_prep("  retest  ", t("2026-05-15T12:00:00Z"));
        assert_eq!(h.prep_names(), vec!["retest"]);
    }

    #[test]
    fn history_round_trips_through_yaml() {
        let mut h = History::default();
        h.record_prep("retest", t("2026-05-15T12:00:00Z"));
        h.record_veto("news-window", t("2026-05-15T12:01:00Z"));
        let yaml = serde_yaml::to_string(&h).unwrap();
        let parsed: History = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.prep_names(), vec!["retest"]);
        assert_eq!(parsed.veto_names(), vec!["news-window"]);
    }

    #[test]
    fn empty_yaml_parses_to_default() {
        let parsed: History = serde_yaml::from_str("preps: []\nvetos: []\n").unwrap();
        assert!(parsed.preps.is_empty());
        assert!(parsed.vetos.is_empty());
    }
}
