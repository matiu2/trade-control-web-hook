//! Atomic writes preserve other accounts' generated rows.
use color_eyre::{Result, eyre::ensure};
use std::path::Path;

pub fn write(path: &Path, account: &str, table: &str) -> Result<()> {
    let previous = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        previous
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix('('))
            .all(|row| {
                row.trim_start()
                    .strip_prefix("\"ctrader-")
                    .and_then(|tag| tag.split_once('"'))
                    .is_some_and(|(owner, _)| owner == account)
                    && row.contains(&format!("\"ctrader:{account}:"))
            }),
        "cTrader table includes another account; merge its profiles before replacement"
    );
    let temporary = path.with_extension("rs.tmp");
    std::fs::write(&temporary, table)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    #[test]
    fn another_account_cannot_be_erased() {
        crate::ctrader::test_logging();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctrader.rs");
        super::write(
            &path,
            "demo",
            "( \"ctrader-demo\", \"ctrader:demo:EURUSD\")",
        )
        .unwrap();
        assert!(super::write(&path, "other", "replacement").is_err());
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("ctrader:demo")
        );
        super::write(&path, "demo", "updated own rows").unwrap();
    }
    #[test]
    fn identity_header_with_ctrader_named_profile_does_not_block_own_updates() {
        crate::ctrader::test_logging();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctrader.rs");
        let table = "// Identity={\"profile\":\"ctrader-demo\"}\n(\"ctrader-ctrader-demo\", \"ctrader:ctrader-demo:EURUSD\")";
        super::write(&path, "ctrader-demo", table).unwrap();
        super::write(&path, "ctrader-demo", table).unwrap();
        assert!(super::write(&path, "other", table).is_err());
    }
}
