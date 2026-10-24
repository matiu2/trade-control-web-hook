//! Collection retains checkpoints; any scope/source/quality change stops activation.
use super::{args::Args, commands, scope, source, status, steps};
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};
use spread_baseline_gen::ctrader::{self, checkpoint::Destination};
use std::path::{Path, PathBuf};

struct Pinned {
    workspace: source::Source,
    cache: source::Source,
    table: PathBuf,
    original: Vec<u8>,
}
impl Pinned {
    fn capture(args: &Args, status: &status::Status) -> Result<Self> {
        let artifacts = [args.report.clone(), args.status.clone()];
        let cache_path = args.workspace.join("../candle-cache").canonicalize()?;
        let workspace = source::Source::pin(&args.workspace, false, &artifacts)?;
        let cache = source::Source::pin(&cache_path, true, &artifacts)?;
        let table = cache_path.join("src/spread_profiles/ctrader.rs");
        let original = std::fs::read(&table)?;
        status.pin_sources(vec![
            status::SourceEvidence {
                name: "workspace".into(),
                head: workspace.head().into(),
                sha256: workspace.digest().into(),
            },
            status::SourceEvidence {
                name: "candle-cache".into(),
                head: cache.head().into(),
                sha256: cache.digest().into(),
            },
        ])?;
        status.write(
            "sources-pinned",
            "exact source snapshots and existing table bytes captured; drafts remain untouched",
        )?;
        Ok(Self {
            workspace,
            cache,
            table,
            original,
        })
    }
    fn verify(&self, table: &[u8]) -> Result<()> {
        self.workspace.verify(false)?;
        self.cache.verify(true)?;
        unchanged(&self.table, table)
    }
}

pub fn finish(args: &Args, status: &status::Status) -> Result<()> {
    let pinned = Pinned::capture(args, status)?;
    if args.deploy_staging {
        require_staging(&args.workspace)?;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    // Recovery never invokes the generator or baker, and holds the report lock throughout gates.
    let _report_lock = if args.verify_baked {
        verify_baked(args, &pinned, status, &runtime)?
    } else {
        collect(args, &pinned, status, &runtime)?
    };
    let baked = if args.verify_baked {
        pinned.original.clone()
    } else {
        std::fs::read(&pinned.table)?
    };
    for step in steps::validation(args.deploy_staging) {
        pinned.verify(&baked)?;
        if step.state == "deploying-staging" {
            require_staging(&args.workspace)?;
        }
        commands::execute(&step, &args.workspace, status)?;
    }
    pinned.verify(&baked)?;
    status.write("complete", &format!("strict non-stock row verification and release gates passed; {} symbols; verify_baked={}; workspace HEAD={}, SHA256={}; candle-cache HEAD={}, SHA256={}; staging_deployed={}; no Git writes", args.only.len(), args.verify_baked, pinned.workspace.head(), pinned.workspace.digest(), pinned.cache.head(), pinned.cache.digest(), args.deploy_staging))
}

fn collect(
    args: &Args,
    pinned: &Pinned,
    status: &status::Status,
    runtime: &tokio::runtime::Runtime,
) -> Result<Destination> {
    let binaries = std::env::current_exe()?;
    let binaries = binaries
        .parent()
        .ok_or_else(|| eyre!("completion executable has no parent directory"))?;
    let generator = binaries.join("generate-ctrader");
    let baker = binaries.join("bake-ctrader");
    ensure!(
        generator.is_file() && baker.is_file(),
        "build sibling generate-ctrader and bake-ctrader release binaries first"
    );
    let info = ctrader::output::table_info(std::str::from_utf8(&pinned.original)?)?;
    status.write("verifying-missing-scope", "reauthenticate demo account and verify explicit native subset, table identity/window and missing keys before collection")?;
    let identity = runtime.block_on(scope::fresh(args))?;
    scope::table(args, &identity, &info)?;
    let resume = runtime.block_on(preflight_report(args, &identity))?;
    pinned.verify(&pinned.original)?;
    commands::execute(
        &commands::generate(args, &generator, resume),
        &args.workspace,
        status,
    )?;
    // Hold the writer lock throughout verification, merge and release gates.
    let report_lock = Destination::acquire(&args.report, Some(&args.report))?;
    status.write("verifying-complete-nonstocks", "reauthenticate selected native catalogue and require every requested profile to pass strict complete/quality gates")?;
    runtime.block_on(async {
        let fresh = scope::fresh(args).await?;
        ensure!(
            identity == fresh,
            "verified account identity changed during collection"
        );
        scope::table(args, &fresh, &info)?;
        let report = report_lock
            .read_resume()
            .await?
            .ok_or_else(|| eyre!("completed calibration report is unavailable"))?;
        scope::report(args, &fresh, &report, true)
    })?;
    pinned.verify(&pinned.original)?;
    commands::execute(
        &commands::bake(args, &baker, &pinned.table),
        &args.workspace,
        status,
    )?;
    Ok(report_lock)
}

fn verify_baked(
    args: &Args,
    pinned: &Pinned,
    status: &status::Status,
    runtime: &tokio::runtime::Runtime,
) -> Result<Destination> {
    ensure!(
        args.report.try_exists()?,
        "--verify-baked requires the existing complete batch report"
    );
    let report_lock = Destination::acquire(&args.report, Some(&args.report))?;
    status.write("verifying-baked-nonstocks", "reauthenticate demo identity/native metadata, require complete exact-scope report and literal baked row equality; collection and baking are skipped")?;
    runtime.block_on(async {
        let report = report_lock
            .read_resume()
            .await?
            .ok_or_else(|| eyre!("completed calibration report is unavailable"))?;
        scope::verify_baked(args, &report, std::str::from_utf8(&pinned.original)?).await
    })?;
    pinned.verify(&pinned.original)?;
    Ok(report_lock)
}

async fn preflight_report(args: &Args, identity: &ctrader::Identity) -> Result<bool> {
    let resume = args.report.try_exists()?;
    let lock = Destination::acquire(&args.report, resume.then_some(args.report.as_path()))?;
    if resume {
        let report = lock
            .read_resume()
            .await?
            .ok_or_else(|| eyre!("existing calibration report is unavailable"))?;
        scope::report(args, identity, &report, false)?;
    }
    Ok(resume)
}

fn require_staging(workspace: &Path) -> Result<()> {
    source::require_staging(&source::output(workspace, &["branch", "--show-current"])?)
}

fn unchanged(table: &Path, expected: &[u8]) -> Result<()> {
    ensure!(
        std::fs::read(table)? == expected,
        "generated spread table changed during completion; review and restart before merging/deploying"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_table_changes_block_merge_without_overwriting_rows() {
        crate::test_logging();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctrader.rs");
        std::fs::write(&path, b"original").unwrap();
        unchanged(&path, b"original").unwrap();
        std::fs::write(&path, b"concurrent-bake").unwrap();
        assert!(unchanged(&path, b"original").is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"concurrent-bake");
    }
    #[test]
    fn unrelated_existing_report_is_never_replaced_on_resume() {
        crate::test_logging();
        let dir = tempfile::tempdir().unwrap();
        let mut args = super::super::args::tests::args();
        args.report = dir.path().join("report.json");
        let original = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/validation/ctrader-demo-xpdusd-90d-report.json"
        ));
        std::fs::write(&args.report, original).unwrap();
        let report: ctrader::Report = serde_json::from_str(original).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            runtime
                .block_on(preflight_report(&args, &report.identity))
                .unwrap()
        );
        args.only = vec!["XAUEUR".into()];
        assert!(
            runtime
                .block_on(preflight_report(&args, &report.identity))
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&args.report).unwrap(), original);
    }
}
