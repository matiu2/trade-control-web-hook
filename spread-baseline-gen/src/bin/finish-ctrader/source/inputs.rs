//! Inventory every local Cargo package, including non-Rust build inputs.
use super::*;
use color_eyre::eyre::{Context, eyre};
use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}
#[derive(Deserialize)]
struct Package {
    name: String,
    manifest_path: PathBuf,
    source: Option<String>,
}

pub(super) fn inventory(directory: &Path, cache: bool, artifacts: &[PathBuf]) -> Result<Inventory> {
    let (packages, mut excluded) = packages(directory)?;
    let mut inventory = Inventory::new();
    if cache {
        excluded.insert(
            directory
                .canonicalize()?
                .join("src/spread_profiles/ctrader.rs"),
        );
    }
    for package in packages {
        let head = output(&package, &["rev-parse", "HEAD"])?;
        inventory.insert(format!("git-head:{}", package.display()), Some(head));
        let files = output(
            &package,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?;
        for relative in files.split('\0').filter(|path| !path.is_empty()) {
            let path = package.join(relative);
            if !artifact(Path::new(relative))
                && !excluded.contains(&path)
                && !super::artifacts::contains(artifacts, &path)
            {
                record(&mut inventory, &path)?;
            }
        }
        configuration(&mut inventory, &package)?;
    }
    // Cargo can read caller-level configuration even when this directory is a
    // virtual workspace with no package of its own.
    configuration(&mut inventory, directory)?;
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    if let Some(home) = cargo_home {
        for name in ["config", "config.toml"] {
            record(&mut inventory, &home.join(name))?;
        }
    }
    Ok(inventory)
}

type PackageInputs = (BTreeSet<PathBuf>, BTreeSet<PathBuf>);

fn packages(directory: &Path) -> Result<PackageInputs> {
    let response = std::process::Command::new("cargo")
        .args(["metadata", "--offline", "--locked", "--format-version", "1"])
        .current_dir(directory)
        .output()?;
    ensure!(
        response.status.success(),
        "offline Cargo dependency discovery failed"
    );
    let metadata: Metadata = serde_json::from_slice(&response.stdout)?;
    let mut packages = BTreeSet::new();
    let mut excluded = BTreeSet::new();
    for package in metadata
        .packages
        .into_iter()
        .filter(|package| package.source.is_none())
    {
        let directory = package
            .manifest_path
            .parent()
            .ok_or_else(|| eyre!("local package manifest has no directory"))?
            .canonicalize()?;
        if package.name == "candle-cache" {
            // This table is deliberately replaced by the baker and checked
            // separately by the finisher, including when reached transitively.
            excluded.insert(directory.join("src/spread_profiles/ctrader.rs"));
        }
        packages.insert(directory);
    }

    // The workspace root also owns Cargo.lock, deployment scripts and settings.
    packages.insert(directory.canonicalize()?);
    Ok((packages, excluded))
}

fn artifact(path: &Path) -> bool {
    path.components().any(|part| part.as_os_str() == "target")
        || path.starts_with("spread-baseline-gen/validation")
        || path.starts_with("validation")
}

fn configuration(inventory: &mut Inventory, directory: &Path) -> Result<()> {
    for parent in directory.ancestors() {
        for name in [
            ".cargo/config",
            ".cargo/config.toml",
            "rust-toolchain",
            "rust-toolchain.toml",
        ] {
            record(inventory, &parent.join(name))?;
        }
    }
    Ok(())
}

fn record(inventory: &mut Inventory, path: &Path) -> Result<()> {
    let digest = match std::fs::read(path) {
        Ok(bytes) => Some(format!("{:x}", Sha256::digest(bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).wrap_err_with(|| format!("snapshot build input {}", path.display()));
        }
    };
    inventory.insert(path.to_string_lossy().into_owned(), digest);
    Ok(())
}
