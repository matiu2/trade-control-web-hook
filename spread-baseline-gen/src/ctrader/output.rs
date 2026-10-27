//! Atomic writes preserve other accounts' generated rows.
use color_eyre::{Result, eyre::ensure};
use std::{fs::File, path::Path};

mod merge;
#[cfg(test)]
mod test_table;

/// Read-only identity, original evidence window and qualified native keys.
pub struct TableInfo {
    pub identity: super::Identity,
    pub window: String,
    pub keys: Vec<String>,
}

/// Inspect canonical baker output without modifying the table or its rows.
pub fn table_info(source: &str) -> Result<TableInfo> {
    merge::table_info(source)
}

/// Require every rendered row to exist literally, preserving all other rows.
/// This read-only recovery check never adds, replaces or rewrites profiles.
pub fn verify_present(existing: &str, rendered: &str) -> Result<()> {
    merge::verify_present(existing, rendered)
}

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
    fn table_info_retains_verified_identity_window_and_native_keys() {
        let info = super::table_info(&table(&["EURUSD", "Hong Kong 50"])).unwrap();
        assert_eq!(info.identity.profile, "demo");
        assert_eq!(info.identity.account_id, 42);
        assert_eq!(
            info.keys,
            ["ctrader:demo:EURUSD", "ctrader:demo:Hong Kong 50"]
        );
        assert_eq!(
            info.window,
            "// Genuine paired M1 ticks through candle-cache, UTC 2026-07-01 00:00:00 UTC through 2026-10-01 00:00:00 UTC."
        );
        assert!(super::table_info("not a canonical table").is_err());
    }

    #[test]
    fn baked_recovery_requires_every_rendered_row_byte_for_byte() {
        let existing = table(&["EURUSD", "AUDUSD", "XPDUSD"]);
        let rendered = table(&["AUDUSD", "XPDUSD"]);
        super::verify_present(&existing, &rendered).unwrap();
        for incompatible in [
            table(&["AUDUSD", "XAUEUR"]),
            rendered.replace("1.0, 0.5", "1.1, 0.5"),
            rendered.replace("    (", "  ("),
            rendered.replace("\"account_id\":42", "\"account_id\":43"),
            rendered.replace("2026-07-01", "2026-07-02"),
        ] {
            assert!(super::verify_present(&existing, &incompatible).is_err());
        }
        assert!(super::verify_present(&existing.replace("),\n", "),\r\n"), &rendered).is_err());
    }

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
