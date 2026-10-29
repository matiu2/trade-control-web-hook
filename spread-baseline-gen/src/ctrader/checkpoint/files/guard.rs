//! Release advisory ownership even while a fork or duplicate retains the same OFD.
use std::{fs::File, path::PathBuf};

pub(super) struct AdvisoryLock {
    pub(super) file: File,
    path: PathBuf,
}
impl AdvisoryLock {
    /// Construct only after this open file description successfully acquired its lock.
    pub(super) fn acquired(file: File, path: PathBuf) -> Self {
        Self { file, path }
    }
}
impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        // Closing one File cannot release flock while an inherited descriptor
        // survives. Unlock this OFD explicitly; it cannot unlock a later owner
        // acquired through a different open file description.
        if let Err(error) = self.file.unlock() {
            tracing::error!(path=%self.path.display(),%error,"failed to release calibration checkpoint advisory lock");
        }
    }
}
