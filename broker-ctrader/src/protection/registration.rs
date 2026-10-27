//! Pure activation-document helpers for the native account-registration transaction.
use super::*;
use std::path::{Path, PathBuf};
pub fn activation_file(profile: &PropAccount, directory: &Path) -> Result<(PathBuf, Vec<u8>)> {
    profile.validate()?;
    ensure!(
        directory.is_absolute(),
        "risk registry directory must be absolute"
    );
    Ok((
        super::binding::path_in(directory, profile.account_id, profile.environment),
        super::binding::document(profile)?,
    ))
}
pub fn validate_activation(profile: &PropAccount, bytes: &[u8]) -> Result<()> {
    super::binding::validate_document(bytes, profile)
}
