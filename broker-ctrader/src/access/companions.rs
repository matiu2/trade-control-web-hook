//! Explicit bindings between distinct Open API and FIX account identifiers.
use super::CompanionBinding;
use color_eyre::eyre::{Result, ensure, eyre};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

pub(super) fn load(name: &str) -> Result<Option<CompanionBinding>> {
    Ok(read(&path()?)?.remove(name))
}

fn path() -> Result<PathBuf> {
    match std::env::var_os("CTRADER_FIX_COMPANIONS_FILE") {
        Some(path) => Ok(path.into()),
        None => Ok(
            Path::new(&std::env::var_os("HOME").ok_or_else(|| eyre!("HOME is unset"))?)
                .join(".config/trade-control/ctrader-fix-companions.toml"),
        ),
    }
}

pub(super) fn read(path: &Path) -> Result<BTreeMap<String, CompanionBinding>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(_) => return Err(eyre!("cannot open cTrader FIX companion bindings")),
    };
    let metadata = file
        .metadata()
        .map_err(|_| eyre!("cannot inspect cTrader FIX companion bindings"))?;
    ensure!(
        metadata.is_file(),
        "cTrader FIX companion bindings must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "cTrader FIX companion bindings must be private (chmod 600)"
        );
    }
    let mut document = String::new();
    file.take(1_048_577)
        .read_to_string(&mut document)
        .map_err(|_| eyre!("cannot read cTrader FIX companion bindings"))?;
    ensure!(
        document.len() <= 1_048_576,
        "cTrader FIX companion bindings exceed size limit"
    );
    let bindings: BTreeMap<String, CompanionBinding> = toml::from_str(&document)
        .map_err(|_| eyre!("invalid cTrader FIX companion binding fields or TOML syntax"))?;
    bindings.iter().try_for_each(|(name, binding)| {
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            "invalid cTrader FIX companion profile name"
        );
        binding.validate()
    })?;
    Ok(bindings)
}
