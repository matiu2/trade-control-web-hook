//! Wait for calibration, validate/bake it, then tag and rebuild staging artifacts.
use std::{path::PathBuf, time::Duration};

use clap::Parser;
use color_eyre::{Result, eyre::ensure};
use serde::{Deserialize, Serialize};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
struct Args {
    #[arg(long)] account: String,
    #[arg(long)] report: PathBuf,
    #[arg(long)] calibration_status: PathBuf,
    #[arg(long)] status: PathBuf,
}

#[derive(Deserialize)]
struct CalibrationStatus {
    state: String,
    exit_code: Option<i32>,
}

#[derive(Serialize)]
struct Status<'a> {
    pid: u32,
    state: &'a str,
    detail: &'a str,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry().with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))).with(tracing_error::ErrorLayer::default()).with(tracing_subscriber::fmt::layer()).try_init()?;
    let args = Args::parse();
    write_status(&args, "waiting", "90-day calibration still running")?;
    if let Err(error) = finish(&args) {
        write_status(&args, "failed", &format!("{error:#}"))?;
        return Err(error);
    }
    Ok(())
}

fn finish(args: &Args) -> Result<()> {
    wait_for_calibration(args)?;
    ensure!(output("git", &["branch", "--show-current"])? == "staging", "checkout is no longer staging");
    run("git", &["diff", "--quiet"])?;
    run("git", &["diff", "--cached", "--quiet"])?;
    let config = mt5_data_source::account_config::AccountConfig::load(&args.account)?;
    let evidence = std::fs::read(&args.report)?;
    let report = serde_json::from_slice(&evidence)?;
    let table = spread_baseline_gen::mt5::bake::render(&report, &args.account, config.login, &config.server)?;
    write_status(args, "baking", "report passed account, completeness and data checks")?;
    let table_path = "core/src/spread_baseline_mt5.rs";
    let evidence_path = format!("spread-baseline-gen/validation/{}-90d-report.json", args.account);
    std::fs::write(table_path, table)?;
    std::fs::write(&evidence_path, evidence)?;
    run("git", &["add", "--", table_path, &evidence_path])?;
    run("git", &["commit", "-m", "feat(spreads): bake validated account-scoped MT5 calibration"])?;
    write_status(args, "checking", "testing and linting generated profiles")?;
    run("cargo", &["fmt", "--all"])?;
    run("cargo", &["test", "-p", "spread-baseline-gen", "-p", "trade-control-core", "-p", "trade-control-cron", "-p", "trade-control-engine-v2", "-p", "tv-arm", "-p", "trade-control-cli", "-p", "trade-control-worker", "-p", "journal", "--lib", "--bins"])?;
    run("cargo", &["clippy", "-p", "spread-baseline-gen", "-p", "trade-control-core", "-p", "trade-control-cron", "-p", "trade-control-engine-v2", "-p", "tv-arm", "-p", "trade-control-cli", "-p", "trade-control-worker", "-p", "journal", "--all-targets", "--", "-D", "warnings"])?;
    run("cargo", &["fmt", "--all", "--", "--check"])?;
    let receipt_path = format!("spread-baseline-gen/validation/{}-bake-checks.json", args.account);
    std::fs::write(&receipt_path, serde_json::to_vec_pretty(&serde_json::json!({"account":args.account,"feed":report.feed,"instruments":report.instruments.len(),"tests":"passed","clippy":"passed","formatting":"passed"}))?)?;
    run("git", &["add", "--", table_path, &receipt_path])?;
    run("git", &["commit", "-m", "test(spreads): verify baked MT5 profiles"])?;
    // Do not publish a version built from other concurrent edits.
    run("git", &["diff", "--quiet"])?;
    run("git", &["diff", "--cached", "--quiet"])?;
    run("git", &["fetch", "origin", "--tags"])?;
    let version = next_version(&output("git", &["tag", "--list", "v*"])?)?;
    run("git", &["tag", "-a", &version, "-m", "Validated MT5 spread calibration"])?;
    write_status(args, "building", &version)?;
    let status = std::process::Command::new("cargo").args(["build", "--release", "-p", "trade-control-cli", "-p", "tv-arm", "-p", "tv-news", "-p", "journal", "-p", "trade-control-worker"]).env("TRADE_CONTROL_WEBHOOK", "http://127.0.0.1:8788").env("TRADE_CONTROL_ENV_SUFFIX", "staging").status()?;
    ensure!(status.success(), "release build failed; tag remains local and no running services changed");
    run("git", &["push", "origin", "staging", &version])?;
    write_status(args, "complete", &format!("{version}: spreads baked, checks passed, release binaries rebuilt; deployment not performed"))?;
    tracing::info!(%version, "MT5 spread bake and release build complete");
    Ok(())
}

fn wait_for_calibration(args: &Args) -> Result<()> {
    for _ in 0..10_080 {
        let status: CalibrationStatus = serde_json::from_slice(&std::fs::read(&args.calibration_status)?)?;
        match status.state.as_str() {
            "finished" => { ensure!(status.exit_code == Some(0), "calibration failed; inspect its report and log"); return Ok(()); }
            "running" => std::thread::sleep(Duration::from_secs(60)),
            state => color_eyre::eyre::bail!("unexpected calibration state: {state}"),
        }
    }
    color_eyre::eyre::bail!("calibration did not finish within seven days")
}

fn write_status(args: &Args, state: &str, detail: &str) -> Result<()> {
    let temporary = args.status.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(&Status { pid: std::process::id(), state, detail })?)?;
    std::fs::rename(temporary, &args.status)?;
    Ok(())
}

fn run(command: &str, args: &[&str]) -> Result<()> {
    tracing::info!(command, ?args, "running MT5 completion step");
    ensure!(std::process::Command::new(command).args(args).status()?.success(), "{command} {args:?} failed");
    Ok(())
}

fn output(command: &str, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(command).args(args).output()?;
    ensure!(output.status.success(), "{command} {args:?} failed");
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn next_version(tags: &str) -> Result<String> {
    let last = tags.lines().filter_map(|tag| tag.strip_prefix('v')?.parse::<u32>().ok()).max().ok_or_else(|| color_eyre::eyre::eyre!("no numbered release tags"))?;
    Ok(format!("v{}", last.checked_add(1).ok_or_else(|| color_eyre::eyre::eyre!("release version overflow"))?))
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
