//! Complete a demo calibration, validate its bake, and optionally deploy staging.
use clap::Parser;
use color_eyre::{
    Result,
    eyre::{Context, ensure},
};
use std::path::{Path, PathBuf};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
#[path = "finish-ctrader/source.rs"]
mod source;
#[path = "finish-ctrader/status.rs"]
mod status;
#[path = "finish-ctrader/steps.rs"]
mod steps;

#[derive(Parser)]
#[command(
    about = "Run/resume cTrader demo calibration, strictly bake, validate release consumers, optionally deploy staging; never commits or publishes"
)]
struct Args {
    #[arg(long)]
    account: String,
    #[arg(long)]
    report: PathBuf,
    /// Machine-readable progress, validation receipt and failure reason.
    #[arg(long)]
    status: PathBuf,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long,default_value_t=24,value_parser=clap::value_parser!(u32).range(1..=24))]
    window_hours: u32,
    /// Install/restart demo STAGING only after all strict gates pass. No production deployment.
    #[arg(long)]
    deploy_staging: bool,
}
fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()?;
    let mut args = Args::parse();
    args.workspace = args.workspace.canonicalize()?;
    // Resolve artifact paths before changing child working directories.
    args.report = absolute(&args.report)?;
    args.status = absolute(&args.status)?;
    let status = status::Status::acquire(&args.report, &args.status)?;
    if let Err(error) = finish(&args, &status) {
        status
            .write("failed", &format!("{error:#}"))
            .wrap_err_with(|| format!("completion failed: {error:#}"))?;
        return Err(error);
    }
    Ok(())
}
fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.into()
    } else {
        std::env::current_dir()?.join(path)
    })
}
fn finish(args: &Args, status: &status::Status) -> Result<()> {
    let checkout = source::Source::pin(
        &args.workspace,
        false,
        &[args.report.clone(), args.status.clone()],
    )?;
    let cache_path = args.workspace.join("../candle-cache").canonicalize()?;
    let cache = source::Source::pin(
        &cache_path,
        true,
        &[args.report.clone(), args.status.clone()],
    )?;
    let table = cache_path.join("src/spread_profiles/ctrader.rs");
    let original_table = std::fs::read(&table)?;
    status.pin_sources(vec![
        status::SourceEvidence {
            name: "workspace".into(),
            head: checkout.head().into(),
            sha256: checkout.digest().into(),
        },
        status::SourceEvidence {
            name: "candle-cache".into(),
            head: cache.head().into(),
            sha256: cache.digest().into(),
        },
    ])?;
    status.write(
        "sources-pinned",
        &format!(
            "workspace_head={}, workspace_source_sha256={}, cache_head={}, cache_source_sha256={}",
            checkout.head(),
            checkout.digest(),
            cache.head(),
            cache.digest()
        ),
    )?;
    if args.deploy_staging {
        require_staging(&args.workspace)?;
    }
    let executable = std::env::current_exe()?;
    let binaries = executable
        .parent()
        .ok_or_else(|| color_eyre::eyre::eyre!("completion executable has no parent directory"))?;
    let generator = binaries.join("generate-ctrader");
    let baker = binaries.join("bake-ctrader");
    ensure!(
        generator.is_file() && baker.is_file(),
        "build sibling generate-ctrader and bake-ctrader release binaries first"
    );
    let mut generation = steps::Step {
        state: "calibrating",
        program: generator.to_string_lossy().into(),
        args: vec![
            "--account".into(),
            args.account.clone(),
            "--out".into(),
            args.report.to_string_lossy().into(),
            "--window-hours".into(),
            args.window_hours.to_string(),
        ],
    };
    if args.report.exists() {
        generation
            .args
            .extend(["--resume".into(), args.report.to_string_lossy().into()]);
    }
    execute(&generation, &args.workspace, status)?;
    let report_lock = spread_baseline_gen::ctrader::checkpoint::Destination::acquire(
        &args.report,
        Some(&args.report),
    )?;
    verify_full_universe(args, status, &report_lock)?;
    checkout.verify(false)?;
    cache.verify(true)?;
    ensure!(
        std::fs::read(&table)? == original_table,
        "generated spread table changed during collection; review before replacing it"
    );
    execute(
        &steps::Step {
            state: "baking",
            program: baker.to_string_lossy().into(),
            args: vec![
                "--account".into(),
                args.account.clone(),
                "--report".into(),
                args.report.to_string_lossy().into(),
                "--out".into(),
                table.to_string_lossy().into(),
            ],
        },
        &args.workspace,
        status,
    )?;
    let baked_table = std::fs::read(&table)?;
    for step in steps::validation(args.deploy_staging) {
        checkout.verify(false)?;
        cache.verify(true)?;
        ensure!(
            std::fs::read(&table)? == baked_table,
            "baked spread table changed during validation"
        );
        if step.state == "deploying-staging" {
            require_staging(&args.workspace)?;
        }
        execute(&step, &args.workspace, status)?;
    }
    checkout.verify(false)?;
    cache.verify(true)?;
    ensure!(
        std::fs::read(&table)? == baked_table,
        "baked spread table changed during final validation/deployment"
    );
    status.write("complete",&format!("strict demo bake and release checks passed; workspace HEAD={}, source SHA256={}, candle-cache HEAD={}, cache SHA256={}, staging_deployed={}; generated table/evidence await scoped commits",checkout.head(),checkout.digest(),cache.head(),cache.digest(),args.deploy_staging))
}
fn verify_full_universe(
    args: &Args,
    status: &status::Status,
    report_lock: &spread_baseline_gen::ctrader::checkpoint::Destination,
) -> Result<()> {
    status.write(
        "verifying-full-catalogue",
        "verify report identity and every currently supported FX pair before baking",
    )?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let report = report_lock.read_resume().await?.ok_or_else(|| {
                color_eyre::eyre::eyre!("completed calibration report is unavailable")
            })?;
            spread_baseline_gen::ctrader::generate::verify_full_universe(&args.account, &report)
                .await
        })
}
fn require_staging(workspace: &Path) -> Result<()> {
    source::require_staging(&source::output(workspace, &["branch", "--show-current"])?)
}
fn execute(step: &steps::Step, workspace: &Path, status: &status::Status) -> Result<()> {
    status.write(step.state, &format!("{} {:?}", step.program, step.args))?;
    let exit = std::process::Command::new(&step.program)
        .args(&step.args)
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
        .status()?;
    ensure!(
        exit.success(),
        "{} failed with {exit}; later validation/deployment steps were not run",
        step.state
    );
    Ok(())
}

#[cfg(test)]
fn test_logging() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_step_stops_later_deployment_commands() {
        test_logging();
        let directory = tempfile::tempdir().unwrap();
        let status = status::Status::acquire(
            &directory.path().join("report.json"),
            &directory.path().join("status.json"),
        )
        .unwrap();
        let bad = steps::Step {
            state: "checking",
            program: "false".into(),
            args: vec![],
        };
        let marker = directory.path().join("deployment-must-not-run");
        let deploy = steps::Step {
            state: "deploying",
            program: "touch".into(),
            args: vec![marker.to_string_lossy().into()],
        };
        let result = (|| -> Result<()> {
            execute(&bad, directory.path(), &status)?;
            execute(&deploy, directory.path(), &status)
        })();
        assert!(result.is_err());
        assert!(!marker.exists());
    }
    #[test]
    fn deployment_is_explicit_and_window_is_bounded() {
        test_logging();
        let args = Args::try_parse_from([
            "finish-ctrader",
            "--account",
            "ctrader-demo",
            "--report",
            "r.json",
            "--status",
            "s.json",
        ])
        .unwrap();
        assert!(!args.deploy_staging);
        assert_eq!(args.window_hours, 24);
        assert!(
            Args::try_parse_from([
                "finish-ctrader",
                "--account",
                "ctrader-demo",
                "--report",
                "r.json",
                "--status",
                "s.json",
                "--window-hours",
                "25"
            ])
            .is_err()
        );
    }
}
