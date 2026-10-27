//! One durable lock and atomic state document per account. No implicit enrollment.
use crate::RiskState;
use color_eyre::eyre::{Result, ensure, eyre};
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct FileStore {
    directory: PathBuf,
}
pub struct LockedStore {
    directory: PathBuf,
    _lock: File,
}
impl FileStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }
    pub fn lock(&self) -> Result<LockedStore> {
        ensure!(
            self.directory.is_dir(),
            "risk directory missing; explicit enrollment required"
        );
        let path = self.directory.join("account.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        lock.try_lock_exclusive()
            .map_err(|_| eyre!("account risk transaction already active"))?;
        Ok(LockedStore {
            directory: self.directory.clone(),
            _lock: lock,
        })
    }
    pub fn enroll(&self, state: &RiskState) -> Result<()> {
        fs::create_dir_all(&self.directory)?;
        let locked = self.lock()?;
        ensure!(
            !self.directory.join("state.json").exists(),
            "risk account already enrolled"
        );
        locked.save(state)
    }
}
impl LockedStore {
    pub fn load(&self) -> Result<RiskState> {
        let file = File::open(self.directory.join("state.json"))?;
        ensure!(
            file.metadata()?.len() <= 128 * 1024 * 1024,
            "risk state exceeds size limit"
        );
        let mut bytes = Vec::new();
        file.take(128 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        let mut state: RiskState = serde_json::from_slice(&bytes)?;
        ensure!(state.schema == 1, "unsupported risk state schema");
        state.policy.validate()?;
        state.needs_reconciliation = true;
        Ok(state)
    }
    pub fn save(&self, state: &RiskState) -> Result<()> {
        let bytes = serde_json::to_vec(state)?;
        let temporary = self.directory.join("state.next");
        // Lock is held: a prior interrupted temporary document can be replaced safely.
        let mut file = private_file(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(temporary, self.directory.join("state.json"))?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
}
fn private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}
