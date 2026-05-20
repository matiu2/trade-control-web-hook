//! Discovery and fuzzy-pick of intent templates on disk.
//!
//! When the operator runs `trade-control encrypt` without `--template`,
//! we walk `~/.config/trade-control/templates/` for `*.yaml` files and
//! offer a fuzzy picker. The displayed name is the path relative to the
//! templates root, with the `.yaml` suffix stripped — so a file at
//! `templates/tradenation/ibex/break-and-close.yaml` shows as
//! `tradenation/ibex/break-and-close` and matches a typeahead like
//! `tr ib brea`.
//!
//! Layout decision: deliberately not a strict broker→instrument→setup
//! tree. The directory structure is whatever the operator picks; the
//! picker just flattens it. That way users can group templates however
//! makes sense to them (by broker, by strategy, by timeframe) and the
//! fuzzy match handles the rest.
//!
//! Empty directory or missing root → return an error pointing the user
//! at where to put templates. We don't auto-create the directory; that
//! would just leave an empty hint that does nothing.

use std::fs;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, eyre};
use dialoguer::{FuzzySelect, theme::ColorfulTheme};

/// Resolve the templates root. Honors `XDG_CONFIG_HOME` if set; otherwise
/// falls back to `~/.config/trade-control/templates`.
pub fn templates_root() -> Result<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        let home = std::env::var("HOME").map_err(|_| eyre!("HOME not set"))?;
        PathBuf::from(home).join(".config")
    };
    Ok(base.join("trade-control").join("templates"))
}

/// A discovered template on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateEntry {
    /// Absolute path to the YAML file.
    pub path: PathBuf,
    /// Human-readable label: path relative to the templates root with
    /// the `.yaml` suffix stripped. e.g. `tradenation/ibex/break-and-close`.
    pub label: String,
}

/// Walk `root` recursively and collect every `*.yaml` / `*.yml` file.
/// Returns entries sorted alphabetically by label. Errors only on I/O
/// failures reading the tree.
pub fn discover_templates(root: &Path) -> Result<Vec<TemplateEntry>> {
    if !root.exists() {
        return Err(eyre!(
            "no templates directory at {} — create it and add *.yaml templates",
            root.display()
        ));
    }
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<TemplateEntry>) -> Result<()> {
    let entries =
        fs::read_dir(dir).map_err(|e| eyre!("reading templates dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| eyre!("reading dir entry: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out)?;
            continue;
        }
        if !is_yaml(&path) {
            continue;
        }
        let label = relative_label(root, &path);
        out.push(TemplateEntry { path, label });
    }
    Ok(())
}

fn is_yaml(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|s| s.to_str()),
        Some("yaml") | Some("yml")
    )
}

/// Display label for `path`: relative-to-root, with `.yaml` / `.yml`
/// suffix stripped. Falls back to the file stem if `path` is not under
/// `root` (shouldn't happen during `walk`, but defensive).
fn relative_label(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let s = rel.to_string_lossy();
    s.strip_suffix(".yaml")
        .or_else(|| s.strip_suffix(".yml"))
        .map(|s| s.to_string())
        .unwrap_or_else(|| s.into_owned())
}

/// Discover templates under the configured root, prompt the user with a
/// fuzzy picker, and return the chosen path. Errors when:
///   - the templates root is missing
///   - no templates are found
///   - the user aborts the picker (Ctrl-C / Esc)
pub fn pick_template_interactive() -> Result<PathBuf> {
    let root = templates_root()?;
    let entries = discover_templates(&root)?;
    if entries.is_empty() {
        return Err(eyre!(
            "no *.yaml templates found under {} — add some first",
            root.display()
        ));
    }
    let labels: Vec<&str> = entries.iter().map(|e| e.label.as_str()).collect();
    let theme = ColorfulTheme::default();
    let idx = FuzzySelect::with_theme(&theme)
        .with_prompt("template (type to filter)")
        .items(&labels)
        .default(0)
        .interact()
        .map_err(|e| eyre!("template pick aborted: {e}"))?;
    Ok(entries[idx].path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, b"v: 1\n").unwrap();
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "trade-control-templates-test-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discover_finds_nested_yaml() {
        let root = temp_root("nested");
        touch(&root.join("oanda/eurusd/pin-bar.yaml"));
        touch(&root.join("tradenation/ibex/break-and-close.yaml"));
        touch(&root.join("notes.txt")); // ignored
        touch(&root.join("close.yml")); // .yml also accepted

        let mut entries = discover_templates(&root).unwrap();
        entries.sort_by(|a, b| a.label.cmp(&b.label));
        let labels: Vec<_> = entries.iter().map(|e| e.label.clone()).collect();
        assert_eq!(
            labels,
            vec![
                "close".to_string(),
                "oanda/eurusd/pin-bar".to_string(),
                "tradenation/ibex/break-and-close".to_string(),
            ]
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn discover_errors_when_root_missing() {
        let root = std::env::temp_dir().join("trade-control-no-such-dir-xyz");
        let _ = fs::remove_dir_all(&root);
        let err = discover_templates(&root).unwrap_err();
        assert!(err.to_string().contains("no templates directory"));
    }

    #[test]
    fn discover_returns_empty_for_empty_dir() {
        let root = temp_root("empty");
        let entries = discover_templates(&root).unwrap();
        assert!(entries.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn relative_label_strips_yaml_suffix() {
        let root = Path::new("/x");
        let path = Path::new("/x/a/b/c.yaml");
        assert_eq!(relative_label(root, path), "a/b/c");
    }

    #[test]
    fn relative_label_strips_yml_suffix() {
        let root = Path::new("/x");
        let path = Path::new("/x/d.yml");
        assert_eq!(relative_label(root, path), "d");
    }

    #[test]
    fn is_yaml_recognises_both_extensions() {
        assert!(is_yaml(Path::new("a.yaml")));
        assert!(is_yaml(Path::new("a.yml")));
        assert!(!is_yaml(Path::new("a.txt")));
        assert!(!is_yaml(Path::new("a")));
    }
}
