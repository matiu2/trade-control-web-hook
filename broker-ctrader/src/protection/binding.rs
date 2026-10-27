//! Sticky physical-account activation, independent of optional profile-file existence.
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
#[derive(Serialize, Deserialize)]
struct Binding {
    account_id: i64,
    environment: ctrader_api::Environment,
    state_directory: PathBuf,
}
fn directory() -> Result<PathBuf> {
    let path = if let Some(path) = std::env::var_os("PROP_RISK_REGISTRY_DIR") {
        PathBuf::from(path)
    } else {
        PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| eyre!("HOME required for risk activation registry"))?,
        )
        .join(".local/state/trade-control/prop-risk-registry")
    };
    ensure!(
        path.is_absolute(),
        "risk registry directory must be absolute"
    );
    Ok(path)
}
pub(super) fn path_in(directory: &Path, id: i64, environment: ctrader_api::Environment) -> PathBuf {
    directory.join(format!("{environment:?}-{id}.json"))
}
pub(super) fn check(
    profile: Option<&PropAccount>,
    id: i64,
    env: ctrader_api::Environment,
) -> Result<()> {
    check_at(&directory()?, profile, id, env)
}
pub(super) fn check_at(
    directory: &Path,
    profile: Option<&PropAccount>,
    id: i64,
    env: ctrader_api::Environment,
) -> Result<()> {
    let path = path_in(directory, id, env);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let binding: Binding = serde_json::from_slice(&bytes)?;
            let p=profile.ok_or_else(||eyre!("protected physical account policy missing/disabled; explicit migration required"))?;
            ensure!(
                binding.account_id == id
                    && binding.environment == env
                    && binding.state_directory == p.state_directory,
                "protected account binding changed; migration required"
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(p) = profile {
                p.validate()?;
                std::fs::create_dir_all(directory)?;
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = match options.open(&path) {
                    Ok(file) => file,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        return check_at(directory, profile, id, env);
                    }
                    Err(e) => return Err(e.into()),
                };
                file.write_all(&serde_json::to_vec(&Binding {
                    account_id: id,
                    environment: env,
                    state_directory: p.state_directory.clone(),
                })?)?;
                file.sync_all()?;
                File::open(directory)?.sync_all()?;
            }
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub(super) fn document(p: &PropAccount) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&Binding {
        account_id: p.account_id,
        environment: p.environment,
        state_directory: p.state_directory.clone(),
    })?)
}
pub(super) fn validate_document(bytes: &[u8], p: &PropAccount) -> Result<()> {
    let b: Binding =
        serde_json::from_slice(bytes).map_err(|_| eyre!("invalid risk activation document"))?;
    ensure!(
        b.account_id == p.account_id
            && b.environment == p.environment
            && b.state_directory == p.state_directory,
        "risk activation identity or state directory differs"
    );
    Ok(())
}
