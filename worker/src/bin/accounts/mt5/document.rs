//! Preserve TOML comments and unrelated profiles while updating one identity.
use super::{Options, files};
use color_eyre::{
    Result,
    eyre::{Context, ensure, eyre},
};
use mt5_data_source::account_config::AccountConfig;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use toml_edit::{DocumentMut, value};

pub struct Profile {
    pub login: u64,
    pub credentials_file: PathBuf,
    pub document: String,
}

pub fn prepare(path: &Path, name: &str, options: &Options) -> Result<Profile> {
    let (mut doc, profiles) = load(path)?;
    let existing = profiles.get(name);
    let login = options
        .login
        .or(existing.map(|p| p.login))
        .ok_or_else(|| eyre!("new MT5 profile needs --login"))?;
    ensure!(login > 0, "MT5 account number must be positive");
    let server = field(&options.server, existing.map(|p| &p.server), "--server")?;
    ensure!(!server.trim().is_empty(), "MT5 server must be nonempty");
    let clock = field(
        &options.server_time,
        existing.map(|p| &p.server_time),
        "--server-time",
    )?;
    clock.parse::<mt5_data_source::ServerTime>()?;
    let mailbox = field(&options.mailbox, existing.map(|p| &p.mailbox), "--mailbox")?;
    let credentials = options
        .credentials_file
        .clone()
        .or_else(|| existing.map(|p| p.credentials_file.clone()))
        .unwrap_or_else(|| {
            path.with_file_name("mt5-credentials")
                .join(format!("{name}.toml"))
        });
    ensure!(
        mailbox.is_absolute() && credentials.is_absolute(),
        "MT5 mailbox and credentials-file must be absolute paths"
    );
    let identity = files::identity(&credentials)?;
    for (other, profile) in &profiles {
        if other != name {
            ensure!(
                files::identity(&profile.credentials_file)? != identity,
                "MT5 credential file is shared with profile '{other}'; choose a separate --credentials-file"
            );
        }
    }
    let account = &mut doc["accounts"][name];
    account["login"] =
        value(i64::try_from(login).wrap_err("MT5 login exceeds TOML integer range")?);
    account["server"] = value(server);
    account["server_time"] = value(clock);
    account["mailbox"] = value(path_string(&mailbox)?);
    account["credentials_file"] = value(path_string(&credentials)?);
    Ok(Profile {
        login,
        credentials_file: credentials,
        document: doc.to_string(),
    })
}

fn load(path: &Path) -> Result<(DocumentMut, BTreeMap<String, AccountConfig>)> {
    if !path.exists() {
        return Ok((DocumentMut::new(), BTreeMap::new()));
    }
    let text = fs::read_to_string(path).wrap_err("reading MT5 profiles")?;
    let doc = text
        .parse::<DocumentMut>()
        .map_err(|_| eyre!("invalid MT5 profile TOML"))?;
    let profiles = AccountConfig::load_all_from(path)
        .map_err(|_| eyre!("invalid MT5 account profiles; fix the existing configuration first"))?;
    Ok((doc, profiles))
}

fn field<T: Clone>(input: &Option<T>, previous: Option<&T>, flag: &str) -> Result<T> {
    input
        .clone()
        .or_else(|| previous.cloned())
        .ok_or_else(|| eyre!("new MT5 profile needs {flag}"))
}

fn path_string(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| eyre!("MT5 paths must be valid UTF-8"))
}
