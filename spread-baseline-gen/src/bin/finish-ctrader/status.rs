//! Machine-readable completion state and a crash-safe, per-report completion lock.
use color_eyre::{
    Result,
    eyre::{Context, ensure},
};
use serde::Serialize;
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

pub struct Status {
    pub path: PathBuf,
    _lock: File,
    sources: std::sync::OnceLock<Vec<SourceEvidence>>,
}
#[derive(Serialize)]
struct Receipt<'a> {
    pid: u32,
    state: &'a str,
    detail: &'a str,
    updated_at: chrono::DateTime<chrono::Utc>,
    sources: Option<&'a Vec<SourceEvidence>>,
}
#[derive(Serialize)]
pub struct SourceEvidence {
    pub name: String,
    pub head: String,
    pub sha256: String,
}
impl Status {
    pub fn acquire(report: &Path, status: &Path) -> Result<Self> {
        let (report, status) = distinct_paths(report, status)?;
        let lock_path = report.with_extension("finish.lock");
        regular_file(&report)?;
        regular_file(&status)?;
        regular_file(&lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        lock.try_lock()
            .wrap_err("another cTrader completion owns this report")?;
        Ok(Self {
            path: status,
            _lock: lock,
            sources: std::sync::OnceLock::new(),
        })
    }
    pub fn pin_sources(&self, sources: Vec<SourceEvidence>) -> Result<()> {
        self.sources
            .set(sources)
            .map_err(|_| color_eyre::eyre::eyre!("sources already pinned"))?;
        Ok(())
    }
    pub fn write(&self, state: &str, detail: &str) -> Result<()> {
        regular_file(&self.path)?;
        let temporary = self
            .path
            .with_extension(format!("json.{}.tmp", std::process::id()));
        let receipt = Receipt {
            pid: std::process::id(),
            state,
            detail,
            updated_at: chrono::Utc::now(),
            sources: self.sources.get(),
        };
        std::fs::write(&temporary, serde_json::to_vec_pretty(&receipt)?)?;
        std::fs::rename(temporary, &self.path)?;
        tracing::info!(state, detail, "cTrader calibration completion");
        Ok(())
    }
}
fn distinct_paths(report: &Path, status: &Path) -> Result<(PathBuf, PathBuf)> {
    let report = normalize(report)?;
    let status = normalize(status)?;
    let paths = [
        &report,
        &status,
        &report.with_extension("finish.lock"),
        &report.with_extension("json.lock"),
    ];
    ensure!(
        paths
            .iter()
            .enumerate()
            .all(|(index, path)| paths[index + 1..].iter().all(|other| path != other)),
        "status, report and calibration lock paths must differ"
    );
    ensure!(
        report.with_extension("json.0.tmp") != status.with_extension("json.0.tmp")
            && paths
                .iter()
                .all(|path| { !is_temporary(path, &report) && !is_temporary(path, &status) }),
        "status/report paths must not collide with calibration temporary files"
    );
    Ok((report, status))
}
fn is_temporary(candidate: &Path, artifact: &Path) -> bool {
    let Some(stem) = artifact.file_stem().and_then(|name| name.to_str()) else {
        return false;
    };
    candidate.parent() == artifact.parent()
        && candidate
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.strip_prefix(&format!("{stem}.json."))
                    .and_then(|suffix| suffix.strip_suffix(".tmp"))
                    .is_some_and(|pid| {
                        !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit())
                    })
            })
}
impl Drop for Status {
    fn drop(&mut self) {
        if let Err(error) = self._lock.unlock() {
            tracing::warn!(%error,"could not release completion lock");
        }
    }
}
fn regular_file(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_file(),
            "completion paths must be regular files, not symlinks or directories"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
fn normalize(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(parent.canonicalize()?.join(
        path.file_name()
            .ok_or_else(|| color_eyre::eyre::eyre!("missing status/report filename"))?,
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_cannot_replace_report_locks_or_temporary_files() {
        crate::test_logging();
        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("report.json");
        for status in [
            report.with_extension("finish.lock"),
            report.with_extension("json.lock"),
            report.with_extension("json.123.tmp"),
            report.with_extension("status"),
        ] {
            assert!(Status::acquire(&report, &status).is_err(), "{status:?}");
        }
        let alias = directory.path().join(".").join("report.finish.lock");
        assert!(Status::acquire(&report, &alias).is_err());
        let temporary_report = directory.path().join("status.json.123.tmp");
        let status = directory.path().join("status.json");
        assert!(Status::acquire(&temporary_report, &status).is_err());
    }
    #[test]
    fn report_collision_and_concurrent_completion_are_refused() {
        crate::test_logging();
        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("report.json");
        let status = directory.path().join("status.json");
        assert!(Status::acquire(&report, &report).is_err());
        let first = Status::acquire(&report, &status).unwrap();
        assert!(Status::acquire(&report, &status).is_err());
        first
            .pin_sources(vec![SourceEvidence {
                name: "workspace".into(),
                head: "commit".into(),
                sha256: "digest".into(),
            }])
            .unwrap();
        first.write("checking", "test receipt").unwrap();
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&status).unwrap()).unwrap();
        assert_eq!(receipt["sources"][0]["sha256"], "digest");
        drop(first);
        assert!(Status::acquire(&report, &status).is_ok());
    }
}
