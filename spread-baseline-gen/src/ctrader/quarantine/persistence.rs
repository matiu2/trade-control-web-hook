//! Atomic private audit files; syncing precedes any data omission.
use super::audit::{Manifest, validate_records};
use color_eyre::{
    Result,
    eyre::{Context, ensure},
};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

pub(super) fn read(path: &Path) -> Result<(Manifest, String)> {
    use std::io::Read;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .wrap_err("open durable quarantine audit")?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= 16 * 1024 * 1024,
        "quarantine audit must be a bounded regular file"
    );
    let mut bytes = Vec::new();
    file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "quarantine audit grew beyond the size limit"
    );
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    validate_records(&manifest.binding, &manifest.records)?;
    Ok((manifest, format!("{:x}", Sha256::digest(&bytes))))
}

pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let temporary = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        File::open(
            path.parent()
                .ok_or_else(|| color_eyre::eyre::eyre!("audit has no parent"))?,
        )?
        .sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        std::fs::remove_file(&temporary).ok();
    }
    result.wrap_err("durably persist crossed-minute exclusion before omitting data")
}
