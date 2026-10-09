//! Replace one account's generated table without erasing another account's bake.
use color_eyre::{Result, eyre::ensure};
use std::path::Path;

pub fn write(path: &Path, account: &str, table: &str) -> Result<()> {
    let previous = match std::fs::read_to_string(path) {
        Ok(previous) => previous,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        previous
            .match_indices("\"mt5-")
            .all(|(start, _)| previous[start + 5..]
                .split_once('"')
                .is_some_and(|(existing, _)| existing == account)),
        "MT5 table includes another account; merge its profiles before replacing the table"
    );
    let temporary = path.with_extension("rs.tmp");
    std::fs::write(&temporary, table)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_accounts_profiles_cannot_be_erased() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("table.rs");
        write(&file, "five", "(\"mt5-five\", \"mt5:five:EURUSD\")").unwrap();
        assert!(write(&file, "other", "replacement").is_err());
        assert!(std::fs::read_to_string(&file).unwrap().contains("mt5-five"));
        assert!(write(&file, "five", "updated own profiles").is_ok());
    }
}
