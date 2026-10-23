//! Atomic writes preserve other accounts' generated rows.
use color_eyre::{Result, eyre::ensure};
use std::{fs::File, path::Path};

mod merge;
#[cfg(test)]
mod test_table;

pub fn write(path: &Path, account: &str, table: &str) -> Result<()> {
    let _guard = lock(path)?;
    let previous = read(path)?;
    merge::preserves(&previous, account, table)?;
    save(path, table)
}

/// Append newly measured symbols without replacing any existing account profile.
pub fn merge(path: &Path, account: &str, table: &str) -> Result<()> {
    ensure!(
        std::fs::symlink_metadata(path)?.file_type().is_file(),
        "additive cTrader bake requires an existing regular table file"
    );
    let _guard = lock(path)?;
    let previous = read(path)?;
    let combined = merge::tables(&previous, account, table)?;
    save(path, &combined)
}

fn lock(path: &Path) -> Result<File> {
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("rs.lock"))?;
    lock.try_lock()?;
    Ok(lock)
}

fn read(path: &Path) -> Result<String> {
    Ok(match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    })
}

fn save(path: &Path, table: &str) -> Result<()> {
    let temporary = path.with_extension("rs.tmp");
    std::fs::write(&temporary, table)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::test_table::table;

    #[test]
    fn another_account_cannot_be_erased() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctrader.rs");
        let original = table(&["EURUSD"]);
        super::write(&path, "demo", &original).unwrap();
        assert!(super::write(&path, "other", &table(&["AUDUSD"])).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        super::write(&path, "demo", &table(&["EURUSD", "AUDUSD"])).unwrap();
    }

    #[test]
    fn identity_header_with_ctrader_named_profile_does_not_block_own_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctrader.rs");
        let original = table(&["EURUSD"])
            .replace("\"profile\":\"demo\"", "\"profile\":\"ctrader-demo\"")
            .replace(
                "(\"ctrader-demo\", \"ctrader:demo:",
                "(\"ctrader-ctrader-demo\", \"ctrader:ctrader-demo:",
            );
        super::write(&path, "ctrader-demo", &original).unwrap();
        super::write(&path, "ctrader-demo", &original).unwrap();
        assert!(super::write(&path, "other", &original).is_err());
    }
}
