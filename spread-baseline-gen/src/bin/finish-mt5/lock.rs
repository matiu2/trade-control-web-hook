//! One completion process per status file.
use color_eyre::{Result, eyre::Context};
use std::path::{Path, PathBuf};

pub struct CompletionLock(PathBuf);

impl CompletionLock {
    pub fn acquire(status: &Path) -> Result<Self> {
        let path = status.with_extension("lock");
        std::fs::OpenOptions::new().write(true).create_new(true).open(&path)
            .wrap_err_with(|| format!("completion lock {} exists; check the recorded process before removing a stale lock", path.display()))?;
        std::fs::write(&path, std::process::id().to_string())?;
        Ok(Self(path))
    }
}

impl Drop for CompletionLock {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0) {
            tracing::warn!(%error, "could not remove completion lock");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_completion_process_is_refused_until_the_first_exits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        let lock = CompletionLock::acquire(&path).unwrap();
        assert!(CompletionLock::acquire(&path).is_err());
        drop(lock);
        assert!(CompletionLock::acquire(&path).is_ok());
    }
}
