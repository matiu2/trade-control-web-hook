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
                "account destination must be a regular file, not a symlink or directory: {}",
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
        ensure!(
            meta.is_file(),
            "account profile lock must be a regular file"
        );
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .wrap_err("opening account profile lock")?;
    file.try_lock()
        .wrap_err("another account account update is in progress")?;
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
                .ok_or_else(|| eyre!("account file needs a filename"))?,
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
        .ok_or_else(|| eyre!("account file needs a parent directory"))
}

pub struct Batch {
    writes: Vec<(Change, Vec<u8>)>,
    _locks: Vec<File>,
}
pub struct AppliedBatch {
    changes: Vec<Change>,
    _locks: Vec<File>,
}
impl Batch {
    pub fn lock_paths(paths: &[&Path]) -> Result<Self> {
        let mut names = paths
            .iter()
            .map(|p| identity(p))
            .collect::<Result<Vec<_>>>()?;
        names.sort();
        names.dedup();
        let locks = names.iter().map(|p| lock(p)).collect::<Result<Vec<_>>>()?;
        Ok(Self {
            writes: Vec::new(),
            _locks: locks,
        })
    }
    pub fn write(&mut self, path: &Path, bytes: Vec<u8>) -> Result<()> {
        let destination = identity(path)?;
        let existing = self
            .writes
            .iter()
            .map(|(change, _)| identity(&change.path))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            !existing.contains(&destination),
            "account registration destinations overlap"
        );
        self.writes.push((Change::read(path.to_owned())?, bytes));
        Ok(())
    }
    pub fn apply(self) -> Result<AppliedBatch> {
        let staged = self
            .writes
            .iter()
            .map(|(change, bytes)| change.stage(bytes, true))
            .collect::<Result<Vec<_>>>()?;
        let mut installed = Vec::new();
        for ((change, _), file) in self.writes.into_iter().zip(staged) {
            if let Err(error) = change.install(file) {
                let restore = restore_all(&installed);
                return Err(match restore {
                    Ok(()) => error,
                    Err(rollback) => eyre!("{error:#}; account file rollback failed: {rollback:#}"),
                });
            }
            installed.push(change);
        }
        let synchronized = installed
            .iter()
            .map(|change| parent(&change.path))
            .collect::<Result<std::collections::BTreeSet<_>>>()?
            .into_iter()
            .try_for_each(|path| File::open(path)?.sync_all());
        if let Err(error) = synchronized {
            let restored = restore_all(&installed);
            return Err(match restored {
                Ok(()) => error.into(),
                Err(rollback) => eyre!("{error}; account rollback failed: {rollback:#}"),
            });
        }
        Ok(AppliedBatch {
            changes: installed,
            _locks: self._locks,
        })
    }
}
impl AppliedBatch {
    pub fn rollback(self) -> Result<()> {
        restore_all(&self.changes)
    }
}
fn restore_all(changes: &[Change]) -> Result<()> {
    let errors: Vec<_> = changes
        .iter()
        .rev()
        .filter_map(|change| change.restore().err())
        .map(|e| format!("{e:#}"))
        .collect();
    ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}
pub fn read_document(path: &Path, private: bool) -> Result<toml_edit::DocumentMut> {
    let change = Change::read(path.to_owned())?;
    if let Some(bytes) = change.before {
        ensure!(
            bytes.len() <= 1_048_576,
            "account configuration exceeds size limit"
        );
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                fs::metadata(path)?.permissions().mode() & 0o077 == 0,
                "private account configuration must have permissions 600"
            );
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| eyre!("invalid account configuration encoding; values withheld"))?;
        return text
            .parse()
            .map_err(|_| eyre!("invalid account configuration TOML; values withheld"));
    }
    Ok(toml_edit::DocumentMut::new())
}
