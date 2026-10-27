//! Serialize table read/merge/write operations across calibration processes.
use color_eyre::{Result, eyre::Context};
use std::{
    fs::File,
    path::{Path, PathBuf},
};

pub(super) struct WriteLock(PathBuf);

impl WriteLock {
    pub(super) fn acquire(table: &Path) -> Result<Self> {
        let path = table.with_extension("rs.lock");
        drop(File::create_new(&path).wrap_err_with(|| {
            format!(
                "MT5 bake lock exists or cannot be created: {}",
                path.display()
            )
        })?);
        let lock = Self(path);
        std::fs::write(&lock.0, std::process::id().to_string())?;
        Ok(lock)
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0) {
            tracing::warn!(path=%self.0.display(), %error, "could not remove MT5 bake lock");
        }
    }
}
