//! Canonical report ownership and process-scoped advisory locks.
use crate::ctrader::Report;
use color_eyre::{
    Result,
    eyre::{Context, ensure, eyre},
};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

/// Locks are released by the OS, including on process exit or interruption.
/// Sidecars remain to keep every process locking the same inode.
pub struct Destination {
    output: PathBuf,
    resume: Option<PathBuf>,
    _locks: Vec<File>,
}
impl Destination {
    pub fn acquire(output: &Path, resume: Option<&Path>) -> Result<Self> {
        let (output, resume) = validate_paths(output, resume)?;
        let mut paths = vec![output.clone()];
        paths.extend(resume.iter().cloned());
        paths.sort();
        paths.dedup();
        let locks = paths
            .iter()
            .map(|path| lock(path))
            .collect::<Result<Vec<_>>>()?;
        // A concurrent fresh run could have created the report before we won its lock.
        validate_paths(&output, resume.as_deref())?;
        Ok(Self {
            output,
            resume,
            _locks: locks,
        })
    }
    pub async fn read_resume(&self) -> Result<Option<Report>> {
        match &self.resume {
            Some(path) => Ok(Some(serde_json::from_slice(&tokio::fs::read(path).await?)?)),
            None => Ok(None),
        }
    }
    pub async fn save(&self, report: &Report) -> Result<()> {
        regular_file(&self.output, false)?;
        let temporary = self
            .output
            .with_extension(format!("json.{}.tmp", std::process::id()));
        tokio::fs::write(&temporary, serde_json::to_vec_pretty(report)?).await?;
        tokio::fs::rename(temporary, &self.output).await?;
        Ok(())
    }
}
fn validate_paths(output: &Path, resume: Option<&Path>) -> Result<(PathBuf, Option<PathBuf>)> {
    let existing = regular_file(output, false)?;
    let resume = resume
        .map(|path| {
            regular_file(path, true)?;
            Ok::<_, color_eyre::Report>(path.canonicalize()?)
        })
        .transpose()?;
    let output = if existing {
        let output = output.canonicalize()?;
        ensure!(
            resume.as_ref() == Some(&output),
            "existing --out must be the same regular file as --resume; unrelated reports cannot be overwritten"
        );
        output
    } else {
        let parent = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        parent.canonicalize()?.join(
            output
                .file_name()
                .ok_or_else(|| eyre!("report output needs a filename"))?,
        )
    };
    Ok((output, resume))
}
fn regular_file(path: &Path, required: bool) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file(),
                "checkpoint paths must be regular files, not symlinks or directories"
            );
            Ok(true)
        }
        Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).wrap_err("inspect checkpoint path"),
    }
}
fn lock(path: &Path) -> Result<File> {
    let path = path.with_extension("json.lock");
    // Never truncate/remove a lock sidecar: replacing its inode permits two owners.
    regular_file(&path, false)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    file.try_lock()
        .wrap_err("another calibration owns this checkpoint or its resume source")?;
    Ok(file)
}

#[cfg(test)]
mod tests;
