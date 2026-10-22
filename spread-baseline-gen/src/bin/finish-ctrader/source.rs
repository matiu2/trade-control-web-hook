//! Freeze exact source bytes, including existing drafts; never rewrite user changes.
use color_eyre::{Result, eyre::ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
type Inventory = BTreeMap<String, Option<String>>;

#[derive(Debug)]
pub struct Source {
    directory: PathBuf,
    head: String,
    inventory: Inventory,
    digest: String,
    artifacts: Vec<PathBuf>,
}
impl Source {
    pub fn pin(directory: &Path, cache: bool, artifacts: &[PathBuf]) -> Result<Self> {
        let artifacts = artifacts::normalize(artifacts)?;
        let inventory = inventory(directory, cache, &artifacts)?;
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&inventory)?));
        Ok(Self {
            directory: directory.into(),
            head: output(directory, &["rev-parse", "HEAD"])?,
            inventory,
            digest,
            artifacts,
        })
    }
    pub fn verify(&self, cache: bool) -> Result<()> {
        ensure!(
            output(&self.directory, &["rev-parse", "HEAD"])? == self.head,
            "source HEAD changed during calibration; review and restart completion before baking/deploying"
        );
        ensure!(
            inventory(&self.directory, cache, &self.artifacts)? == self.inventory,
            "source/manifests/dependency locks changed during calibration; existing drafts must remain byte-identical"
        );
        Ok(())
    }
    pub fn head(&self) -> &str {
        &self.head
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
}
fn inventory(directory: &Path, cache: bool, artifacts: &[PathBuf]) -> Result<Inventory> {
    inputs::inventory(directory, cache, artifacts)
}
pub fn require_staging(branch: &str) -> Result<()> {
    ensure!(
        branch == "staging",
        "deployment is permitted only from the staging branch"
    );
    Ok(())
}
pub fn output(directory: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()?;
    ensure!(output.status.success(), "git source check failed");
    Ok(String::from_utf8(output.stdout)?.trim().into())
}
#[path = "source/artifacts.rs"]
mod artifacts;
#[path = "source/inputs.rs"]
mod inputs;
#[cfg(test)]
#[path = "source/tests.rs"]
mod tests;
