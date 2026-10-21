//! Stage private files beside their destinations and restore on ordinary errors.
use color_eyre::{
    Result,
    eyre::{Context, ensure, eyre},
};
use std::{
    fs::{self, File, OpenOptions, Permissions},
    io::{ErrorKind, Write},
    path::{Component, Path, PathBuf},
};
use tempfile::NamedTempFile;

pub struct Change {
    pub path: PathBuf,
    before: Option<Vec<u8>>,
    permissions: Option<Permissions>,
}

impl Change {
    pub fn read(path: PathBuf) -> Result<Self> {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(meta) => Some(meta),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).wrap_err_with(|| format!("reading {}", path.display()));
            }
        };
        if let Some(meta) = &metadata {
            ensure!(
                meta.is_file(),
                "MT5 destination must be a regular file, not a symlink or directory: {}",
                path.display()
            );
        }
        let before = metadata.as_ref().map(|_| fs::read(&path)).transpose()?;
        Ok(Self {
            path,
            before,
            permissions: metadata.map(|m| m.permissions()),
        })
    }

    pub fn stage(&self, bytes: &[u8], private: bool) -> Result<NamedTempFile> {
        let parent = parent(&self.path)?;
        fs::create_dir_all(parent).wrap_err_with(|| format!("creating {}", parent.display()))?;
        let mut file = NamedTempFile::new_in(parent)?;
        if !private && let Some(permissions) = &self.permissions {
            file.as_file().set_permissions(permissions.clone())?;
        }
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        Ok(file)
    }

    pub fn install(&self, file: NamedTempFile) -> Result<()> {
        file.persist(&self.path)
            .map_err(|error| eyre!("writing {}: {}", self.path.display(), error.error))?;
        Ok(())
    }

    pub fn restore(&self) -> Result<()> {
        match &self.before {
            Some(bytes) => self.install(self.stage(bytes, false)?),
            None => fs::remove_file(&self.path)
                .wrap_err_with(|| format!("removing newly created {}", self.path.display())),
        }
    }
}

pub fn lock(config: &Path) -> Result<File> {
    fs::create_dir_all(parent(config)?)?;
    let path = config.with_extension("toml.lock");
    if let Ok(meta) = fs::symlink_metadata(&path) {
        ensure!(meta.is_file(), "MT5 profile lock must be a regular file");
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).wrap_err("opening MT5 profile lock")?;
    file.try_lock()
        .wrap_err("another MT5 account update is in progress")?;
    Ok(file)
}

pub fn identity(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    if absolute.exists() {
        return fs::canonicalize(absolute).map_err(Into::into);
    }
    if let Some(parent) = absolute.parent()
        && parent.exists()
    {
        return Ok(fs::canonicalize(parent)?.join(
            absolute
                .file_name()
                .ok_or_else(|| eyre!("MT5 file needs a filename"))?,
        ));
    }
    Ok(absolute
        .components()
        .fold(PathBuf::new(), |mut normalized, part| {
            match part {
                Component::ParentDir => {
                    normalized.pop();
                }
                Component::CurDir => {}
                other => normalized.push(other),
            }
            normalized
        }))
}

fn parent(path: &Path) -> Result<&Path> {
    path.parent()
        .ok_or_else(|| eyre!("MT5 file needs a parent directory"))
}
