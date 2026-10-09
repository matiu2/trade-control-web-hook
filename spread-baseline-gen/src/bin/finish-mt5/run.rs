use super::Args;
use super::release;
use color_eyre::{Result, eyre::ensure};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const TABLE_PATH: &str = "core/src/spread_baseline_mt5.rs";

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

pub fn finish(args: &Args) -> Result<()> {
    wait_for_calibration(args)?;
    ensure!(
        output("git", &["branch", "--show-current"])? == "staging",
        "checkout is no longer staging"
    );
    run("git", &["diff", "--quiet"])?;
    run("git", &["diff", "--cached", "--quiet"])?;
    let report = bake(args)?;
    check(args)?;
    record_checks(args, &report)?;
    release::build_and_publish(args)
}

fn bake(args: &Args) -> Result<spread_baseline_gen::mt5::Report> {
    let config = mt5_data_source::account_config::AccountConfig::load(&args.account)?;
    let evidence = std::fs::read(&args.report)?;
    let report = serde_json::from_slice(&evidence)?;
    let table = spread_baseline_gen::mt5::bake::render_finished(
        &report,
        &args.account,
        config.login,
        &config.server,
    )?;
    write_status(
        args,
        "baking",
        "report passed account, completeness and data checks",
    )?;
    let evidence_path = format!(
        "spread-baseline-gen/validation/{}-90d-report.json",
        args.account
    );
    spread_baseline_gen::mt5::output::write(
        std::path::Path::new(TABLE_PATH),
        &args.account,
        &table,
    )?;
    std::fs::write(&evidence_path, evidence)?;
    run("git", &["add", "--", TABLE_PATH, &evidence_path])?;
    run(
        "git",
        &[
            "commit",
            "-m",
            "feat(spreads): bake validated account-scoped MT5 calibration",
        ],
    )?;
    Ok(report)
}

fn check(args: &Args) -> Result<()> {
    write_status(args, "checking", "testing and linting generated profiles")?;
    run("cargo", &["fmt", "--all"])?;
    run("cargo", &check_args("test"))?;
    run("cargo", &check_args("clippy"))?;
    run("cargo", &["fmt", "--all", "--", "--check"])
}

fn check_args(mode: &str) -> Vec<&str> {
    let packages = [
        "spread-baseline-gen",
        "trade-control-core",
        "trade-control-cron",
        "trade-control-engine-v2",
        "tv-arm",
        "trade-control-cli",
        "trade-control-worker",
        "journal",
    ];
    let mut args = vec![mode];
    args.extend(packages.into_iter().flat_map(|package| ["-p", package]));
    args.extend(if mode == "test" {
        vec!["--lib", "--bins"]
    } else {
        vec!["--all-targets", "--", "-D", "warnings"]
    });
    args
}

fn record_checks(args: &Args, report: &spread_baseline_gen::mt5::Report) -> Result<()> {
    let receipt_path = format!(
        "spread-baseline-gen/validation/{}-bake-checks.json",
        args.account
    );
    std::fs::write(
        &receipt_path,
        serde_json::to_vec_pretty(
            &serde_json::json!({"account":args.account,"feed":report.feed,"instruments":report.instruments.len(),"tests":"passed","clippy":"passed","formatting":"passed"}),
        )?,
    )?;
    run("git", &["add", "--", TABLE_PATH, &receipt_path])?;
    run(
        "git",
        &["commit", "-m", "test(spreads): verify baked MT5 profiles"],
    )?;
    Ok(())
}

fn wait_for_calibration(args: &Args) -> Result<()> {
    for _ in 0..10_080 {
        let status: CalibrationStatus =
            serde_json::from_slice(&std::fs::read(&args.calibration_status)?)?;
        match status.state.as_str() {
            "finished" => {
                ensure!(
                    matches!(status.exit_code, Some(0 | 1)),
                    "calibration exited unexpectedly; inspect its report and log"
                );
                return Ok(());
            }
            "running" => std::thread::sleep(Duration::from_secs(60)),
            state => color_eyre::eyre::bail!("unexpected calibration state: {state}"),
        }
    }
    color_eyre::eyre::bail!("calibration did not finish within seven days")
}

pub fn write_status(args: &Args, state: &str, detail: &str) -> Result<()> {
    let temporary = args.status.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_vec_pretty(&Status {
            pid: std::process::id(),
            state,
            detail,
        })?,
    )?;
    std::fs::rename(temporary, &args.status)?;
    Ok(())
}

pub fn run(command: &str, args: &[&str]) -> Result<()> {
    tracing::info!(command, ?args, "running MT5 completion step");
    ensure!(
        std::process::Command::new(command)
            .args(args)
            .status()?
            .success(),
        "{command} {args:?} failed"
    );
    Ok(())
}

pub fn output(command: &str, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(command).args(args).output()?;
    ensure!(output.status.success(), "{command} {args:?} failed");
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}
