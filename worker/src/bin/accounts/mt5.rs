//! Coordinate named MT5 profile and private credential-file updates.

#[path = "mt5/document.rs"]
mod document;
#[path = "mt5/files.rs"]
mod files;
#[path = "mt5/options.rs"]
mod options;

pub use options::Options;

use color_eyre::{
    Result,
    eyre::{Context, ensure, eyre},
};
use files::Change;
use std::{
    fs::File,
    path::{Path, PathBuf},
};

pub struct Update {
    login: u64,
    config: Change,
    credentials: Change,
    document: String,
    lock: File,
}

pub struct AppliedUpdate {
    config: Change,
    credentials: Change,
    _lock: File,
}

impl Update {
    pub fn prepare(path: &Path, name: &str, options: &Options) -> Result<Self> {
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "MT5 account name must contain only letters, digits, underscores or hyphens"
        );
        let path = std::path::absolute(path)?;
        let lock = files::lock(&path)?;
        let config = Change::read(path.clone())?;
        let profile = document::prepare(&path, name, options)?;
        let credentials = Change::read(profile.credentials_file.clone())?;
        ensure!(
            files::identity(&credentials.path)? != files::identity(&path)?
                && files::identity(&credentials.path)?
                    != files::identity(&path.with_extension("toml.lock"))?,
            "MT5 credentials must use a separate file from the profile configuration and lock"
        );
        Ok(Self {
            login: profile.login,
            config,
            credentials,
            document: profile.document,
            lock,
        })
    }

    pub fn login(&self) -> u64 {
        self.login
    }

    pub fn credentials_path(&self) -> &Path {
        &self.credentials.path
    }

    pub fn apply(self, password: &str) -> Result<AppliedUpdate> {
        ensure!(!password.is_empty(), "MT5 password must be nonempty");
        let mut credentials = toml_edit::DocumentMut::new();
        credentials["creds"]["username"] = toml_edit::value(self.login.to_string());
        credentials["creds"]["password"] = toml_edit::value(password);
        let credential_file = self
            .credentials
            .stage(credentials.to_string().as_bytes(), true)?;
        let config_file = self.config.stage(self.document.as_bytes(), false)?;
        self.credentials.install(credential_file)?;
        if let Err(error) = self.config.install(config_file) {
            return Err(rollback_error(error, self.credentials.restore()));
        }
        Ok(AppliedUpdate {
            config: self.config,
            credentials: self.credentials,
            _lock: self.lock,
        })
    }
}

impl AppliedUpdate {
    pub fn rollback(self) -> Result<()> {
        // Attempt both restorations even when one fails.
        let config = self.config.restore();
        let credentials = self.credentials.restore();
        match (config, credentials) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(config), Err(credentials)) => Err(eyre!("{config:#}; {credentials:#}")),
        }
    }
}

pub fn rollback_error(error: color_eyre::Report, restored: Result<()>) -> color_eyre::Report {
    match restored {
        Ok(()) => error.wrap_err("MT5 file changes rolled back"),
        Err(rollback) => eyre!("{error:#}; MT5 file rollback failed: {rollback:#}"),
    }
}

pub fn config_path() -> Result<PathBuf> {
    mt5_data_source::account_config::config_path().wrap_err("locating MT5 account configuration")
}

#[cfg(test)]
#[path = "mt5/tests.rs"]
mod tests;
