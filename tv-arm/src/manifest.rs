//! On-disk manifest reading and calendar-bar bundle discovery.
//!
//! Mirrors the Python `parse_manifest`, `_read_window_yaml` and
//! `discover_calendar_bundles`.
//!
//! Why hand-roll the manifest parser instead of using `serde_yaml`?
//! The CLI's pause/news manifest writer emits unquoted `purpose:`
//! values that contain colons (e.g.
//! `purpose: pause: arm blackout ...`). That's valid in the loose
//! reader Python uses but real YAML rejects it. Rather than fix the
//! writer (separate concern) and risk re-signing churn, we read the
//! shape we actually emit — the same approach Python took.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, Result, eyre};
use serde::Deserialize;

// `Deserialize` is used inside `read_window_yaml` for the inline `W` struct.

/// One alert entry in a `manifest.yaml`. Mirrors Python's
/// `parse_manifest` shape: `file` is the only field tv-arm uses for
/// dispatch; the rest land in `extra` for completeness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// File name, including the `.yaml` extension.
    pub file: String,
    /// All remaining `key: value` lines under this entry,
    /// unprocessed.
    pub extra: HashMap<String, String>,
}

/// Top-level shape of `manifest.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    /// All header fields (trade_id, instrument, trade_expiry, ...)
    /// keyed by their YAML key.
    pub header: HashMap<String, String>,
    /// Ordered alert list.
    pub alerts: Vec<ManifestEntry>,
}

impl Manifest {
    /// Convenience: pick out `trade_id` from the header.
    pub fn trade_id(&self) -> Option<&str> {
        self.header.get("trade_id").map(String::as_str)
    }

    /// Convenience: pick out `instrument` from the header.
    pub fn instrument(&self) -> Option<&str> {
        self.header.get("instrument").map(String::as_str)
    }
}

/// Parse a `manifest.yaml` body into a [`Manifest`].
///
/// Ports the loose-YAML reader from `tv_arm_hs.py::parse_manifest`.
/// Recognised shape:
/// - Top-level `key: value` lines populate the header.
/// - `alerts:` starts a list; each `  - file: <basename>.yaml` entry
///   becomes a new [`ManifestEntry`], and subsequent indented
///   `    key: value` lines populate its `extra` map until the next
///   `  - file:` or the end of the alerts list.
/// - Quoted values have their surrounding quotes stripped.
pub fn parse_manifest(text: &str) -> Result<Manifest> {
    let mut manifest = Manifest::default();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = trim_trailing_ws(lines[i]);
        if line.is_empty() || line.starts_with('#') {
            i += 1;
            continue;
        }
        if line == "alerts:" {
            i += 1;
            while i < lines.len() {
                let entry_line = trim_trailing_ws(lines[i]);
                if let Some(rest) = entry_line.strip_prefix("  - file:") {
                    let file = strip_quotes(rest.trim());
                    let mut extra = HashMap::new();
                    i += 1;
                    while i < lines.len() && lines[i].starts_with("    ") {
                        let kv = trim_trailing_ws(lines[i].trim_start());
                        if let Some((k, v)) = kv.split_once(':') {
                            extra.insert(k.trim().to_string(), strip_quotes(v.trim()));
                        }
                        i += 1;
                    }
                    manifest.alerts.push(ManifestEntry {
                        file: file.to_string(),
                        extra,
                    });
                } else {
                    break;
                }
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            manifest
                .header
                .insert(k.trim().to_string(), strip_quotes(v.trim()));
        }
        i += 1;
    }
    Ok(manifest)
}

fn trim_trailing_ws(s: &str) -> &str {
    s.trim_end()
}

fn strip_quotes(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Pull `start_time` / `end_time` ISO strings out of a calendar-bars
/// round-trip spec (`pause.yaml` or `news.yaml`).
///
/// The two fields are required — returns an error when either is
/// missing.
pub fn read_window_yaml(path: &Path) -> Result<(String, String)> {
    let text = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("read window yaml at {}", path.display()))?;
    #[derive(Deserialize)]
    struct W {
        start_time: Option<String>,
        end_time: Option<String>,
    }
    let w: W = serde_yaml::from_str(&text)
        .wrap_err_with(|| format!("parse window yaml at {}", path.display()))?;
    match (w.start_time, w.end_time) {
        (Some(s), Some(e)) => Ok((s, e)),
        _ => Err(eyre!(
            "{} missing start_time and/or end_time",
            path.display()
        )),
    }
}

/// Discovered calendar-bars bundle ready for alert-spec dispatch.
#[derive(Debug, Clone)]
pub struct CalendarBundle {
    /// Event slug (`eur-german-prelim-cpi-m-m-1780034400`).
    pub event_slug: String,
    /// `pause` or `news` — names the sub-directory the bundle lives
    /// in and selects which window edge to anchor each alert at.
    pub kind: CalendarKind,
    /// Parsed manifest for the bundle.
    pub manifest: Manifest,
    /// Directory containing this bundle's signed YAMLs and
    /// manifest.
    pub bundle_dir: PathBuf,
    /// ISO start time read from the bundle's `pause.yaml` /
    /// `news.yaml`.
    pub start_iso: String,
    /// ISO end time.
    pub end_iso: String,
}

/// Bundle kind — either a pause window (blocks entries) or a news
/// window (enables the close-on-reversal gate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarKind {
    /// `pause` bundle.
    Pause,
    /// `news` bundle.
    News,
}

impl CalendarKind {
    /// Subdirectory name and round-trip-spec filename root.
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::News => "news",
        }
    }
}

/// Walk `root/calendar-bars/<trade_id>/<event_slug>/{pause,news}/`
/// and return one [`CalendarBundle`] per (event × kind) found.
///
/// Sorted by `(event_slug, kind)` for stable ordering across runs.
/// Returns `Ok(vec![])` when the root directory is missing — the
/// subcommand was skipped or fetched no events.
pub fn discover_calendar_bundles(root: &Path, trade_id: &str) -> Result<Vec<CalendarBundle>> {
    let base = root.join("calendar-bars").join(trade_id);
    if !base.is_dir() {
        return Ok(Vec::new());
    }
    let mut event_dirs: Vec<PathBuf> = std::fs::read_dir(&base)
        .wrap_err_with(|| format!("read {}", base.display()))?
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    event_dirs.sort();

    let mut bundles = Vec::new();
    for event_dir in event_dirs {
        let event_slug = match event_dir.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        for kind in [CalendarKind::Pause, CalendarKind::News] {
            let bundle_dir = event_dir.join(kind.dir_name());
            if !bundle_dir.is_dir() {
                continue;
            }
            let manifest_path = bundle_dir.join("manifest.yaml");
            let spec_path = bundle_dir.join(format!("{}.yaml", kind.dir_name()));
            if !manifest_path.exists() || !spec_path.exists() {
                continue;
            }
            let manifest_text = std::fs::read_to_string(&manifest_path)
                .wrap_err_with(|| format!("read {}", manifest_path.display()))?;
            let manifest = parse_manifest(&manifest_text)?;
            let (start_iso, end_iso) = read_window_yaml(&spec_path)?;
            bundles.push(CalendarBundle {
                event_slug: event_slug.clone(),
                kind,
                manifest,
                bundle_dir,
                start_iso,
                end_iso,
            });
        }
    }
    Ok(bundles)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MANIFEST: &str = r#"trade_id: hs-eur-aud-060438cc
instrument: EUR_AUD
trade_expiry: "2026-06-10T00:00:00Z"
alerts:
  - file: 01-veto-too-high.yaml
    purpose: "veto: too-high"
    action: Veto
    name: too-high
    level: ClosePositions
    not_after: "2026-06-10T00:00:00Z"
  - file: 05-enter.yaml
    purpose: "enter"
    action: Enter
"#;

    #[test]
    fn parse_minimal_manifest() {
        let m = parse_manifest(SAMPLE_MANIFEST).expect("parse");
        assert_eq!(m.trade_id(), Some("hs-eur-aud-060438cc"));
        assert_eq!(m.instrument(), Some("EUR_AUD"));
        assert_eq!(m.alerts.len(), 2);
        assert_eq!(m.alerts[0].file, "01-veto-too-high.yaml");
        assert_eq!(m.alerts[1].file, "05-enter.yaml");
        // Quoted purpose round-trips with quotes stripped.
        let purpose = m.alerts[0].extra.get("purpose").map(String::as_str);
        assert_eq!(purpose, Some("veto: too-high"));
    }

    #[test]
    fn parse_manifest_with_pause_purpose_containing_colon() {
        // Real pause manifest emitted by the CLI today — `purpose:
        // pause: arm blackout …` is the line that breaks serde_yaml.
        // Our parser splits on the first `:` and accepts the rest,
        // matching what Python does.
        let yaml = "trade_id: smoke-test\nblackout_id: cal-x-pause\n\
                    alerts:\n  - file: 01-pause-cal-x-pause.yaml\n    \
                    purpose: pause: arm blackout cal-x-pause\n";
        let m = parse_manifest(yaml).expect("parse");
        assert_eq!(m.alerts.len(), 1);
        assert_eq!(m.alerts[0].file, "01-pause-cal-x-pause.yaml");
        assert_eq!(
            m.alerts[0].extra.get("purpose").map(String::as_str),
            Some("pause: arm blackout cal-x-pause")
        );
    }

    #[test]
    fn discover_bundles_in_phase_2_smoke_layout() {
        // Smoke output from phase 2 lives at /tmp/cal-smoke.
        let root = std::path::PathBuf::from("/tmp/cal-smoke");
        if !root.is_dir() {
            // Skip when running on a machine that doesn't have the
            // smoke output (e.g. CI). Phase 2's commit doesn't bake
            // a fixture into the repo, so we tolerate absence here.
            eprintln!("skipping discover_bundles_in_phase_2_smoke_layout: no /tmp/cal-smoke");
            return;
        }
        let bundles = discover_calendar_bundles(&root, "smoke-test").expect("discover");
        assert_eq!(bundles.len(), 2);
        let kinds: Vec<_> = bundles.iter().map(|b| b.kind).collect();
        // Sorted: pause comes before news (alphabetical at second
        // level), but we list pause first explicitly in the loop —
        // so kinds = [Pause, News] in order.
        assert_eq!(kinds, vec![CalendarKind::Pause, CalendarKind::News]);
        assert_eq!(bundles[0].start_iso, "2026-05-29T03:00:00Z");
        assert_eq!(bundles[0].end_iso, "2026-05-29T06:00:00Z");
        assert_eq!(bundles[1].start_iso, "2026-05-29T06:00:00Z");
        assert_eq!(bundles[1].end_iso, "2026-05-29T07:00:00Z");
    }

    #[test]
    fn discover_returns_empty_when_root_missing() {
        let root = std::path::PathBuf::from("/tmp/this-path-does-not-exist-xyz");
        let bundles = discover_calendar_bundles(&root, "any").expect("ok");
        assert!(bundles.is_empty());
    }

    #[test]
    fn read_window_yaml_picks_up_start_and_end() {
        let dir = std::env::temp_dir().join("tv-arm-test-window-yaml");
        std::fs::create_dir_all(&dir).expect("create");
        let path = dir.join("pause.yaml");
        std::fs::write(
            &path,
            "trade_id: x\nblackout_id: y\nstart_time: 2026-05-29T03:00:00Z\nend_time: 2026-05-29T06:00:00Z\n",
        )
        .expect("write");
        let (s, e) = read_window_yaml(&path).expect("read");
        assert_eq!(s, "2026-05-29T03:00:00Z");
        assert_eq!(e, "2026-05-29T06:00:00Z");
    }

    #[test]
    fn read_window_yaml_errors_when_missing() {
        let dir = std::env::temp_dir().join("tv-arm-test-window-missing");
        std::fs::create_dir_all(&dir).expect("create");
        let path = dir.join("pause.yaml");
        std::fs::write(&path, "trade_id: x\n").expect("write");
        let err = read_window_yaml(&path).expect_err("should error");
        assert!(format!("{err}").contains("missing"));
    }
}
