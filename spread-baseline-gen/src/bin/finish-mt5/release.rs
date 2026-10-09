use super::{
    Args,
    run::{output, run, write_status},
};
use color_eyre::{Result, eyre::ensure};

pub fn build_and_publish(args: &Args) -> Result<()> {
    // Do not publish a version built from other concurrent edits.
    run("git", &["diff", "--quiet"])?;
    run("git", &["diff", "--cached", "--quiet"])?;
    run("git", &["fetch", "origin", "--tags"])?;
    let release_head = output("git", &["rev-parse", "HEAD"])?;
    let version = next_version(&output("git", &["tag", "--list", "v*"])?)?;
    run(
        "git",
        &[
            "tag",
            "-a",
            &version,
            "-m",
            "Validated MT5 spread calibration",
        ],
    )?;
    write_status(args, "building", &version)?;
    let status = std::process::Command::new("cargo")
        .args([
            "build",
            "--release",
            "-p",
            "trade-control-cli",
            "-p",
            "tv-arm",
            "-p",
            "tv-news",
            "-p",
            "journal",
            "-p",
            "trade-control-worker",
        ])
        .env("TRADE_CONTROL_WEBHOOK", "http://127.0.0.1:8788")
        .env("TRADE_CONTROL_ENV_SUFFIX", "staging")
        .status()?;
    ensure!(
        status.success(),
        "release build failed; tag remains local and no running services changed"
    );
    run("git", &["diff", "--quiet"])?;
    run("git", &["diff", "--cached", "--quiet"])?;
    ensure!(
        output("git", &["rev-parse", "HEAD"])? == release_head,
        "checkout changed during the release build"
    );
    run("git", &["push", "origin", "staging", &version])?;
    write_status(
        args,
        "complete",
        &format!(
            "{version}: spreads baked, checks passed, release binaries rebuilt; deployment not performed"
        ),
    )?;
    tracing::info!(%version, "MT5 spread bake and release build complete");
    Ok(())
}

fn next_version(tags: &str) -> Result<String> {
    let last = tags
        .lines()
        .filter_map(|tag| tag.strip_prefix('v')?.parse::<u32>().ok())
        .max()
        .ok_or_else(|| color_eyre::eyre::eyre!("no numbered release tags"))?;
    Ok(format!(
        "v{}",
        last.checked_add(1)
            .ok_or_else(|| color_eyre::eyre::eyre!("release version overflow"))?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_version_ignores_package_tags_and_never_reuses_a_number() {
        assert_eq!(next_version("v148\nv147\nv1.2.3\nv149\n").unwrap(), "v150");
        assert!(next_version("package-v1.0").is_err());
    }
}
