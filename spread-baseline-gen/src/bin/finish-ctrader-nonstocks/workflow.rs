//! Collection retains checkpoints; any scope/source/quality change stops activation.
use super::{args::Args, commands, scope, source, status, steps};
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};
use spread_baseline_gen::ctrader::{self, checkpoint::Destination};
use std::path::{Path, PathBuf};

#[path = "../finish-ctrader/runtime.rs"]
mod runtime;

struct Pinned {
    workspace: source::Source,
    cache: source::Source,
    table: PathBuf,
    original: Vec<u8>,
}
impl Pinned {
    fn capture(args: &Args, status: &status::Status) -> Result<Self> {
        let artifacts = artifacts(args);
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
    let runtime = runtime::build()?;
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
    status.write("complete", &format!("strict native row verification and release gates passed; {} symbols; verify_baked={}; workspace HEAD={}, SHA256={}; candle-cache HEAD={}, SHA256={}; staging_deployed={}; no Git writes", args.only.len(), args.verify_baked, pinned.workspace.head(), pinned.workspace.digest(), pinned.cache.head(), pinned.cache.digest(), args.deploy_staging))
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
    let checkpoint = runtime.block_on(preflight_report(args, &identity))?;
    pinned.verify(&pinned.original)?;
    let report_lock = finish_collection(checkpoint, |resume| {
        commands::execute(
            &commands::generate(args, &generator, resume),
            &args.workspace,
            status,
        )?;
        Destination::acquire(&args.report, Some(&args.report))
    })?;
    status.write("verifying-complete-native", "reauthenticate selected native catalogue and require every requested profile to pass strict complete/quality gates")?;
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
    status.write("verifying-baked-native", "reauthenticate demo identity/native metadata, require complete exact-scope report and literal baked row equality; collection and baking are skipped")?;
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

enum Checkpoint {
    Fresh,
    Resume,
    Complete(Destination),
}

fn finish_collection(
    checkpoint: Checkpoint,
    generate: impl FnOnce(bool) -> Result<Destination>,
) -> Result<Destination> {
    match checkpoint {
        Checkpoint::Complete(lock) => Ok(lock),
        Checkpoint::Fresh => generate(false),
        Checkpoint::Resume => generate(true),
    }
}

async fn preflight_report(args: &Args, identity: &ctrader::Identity) -> Result<Checkpoint> {
    let resume = args.report.try_exists()?;
    let lock = Destination::acquire(&args.report, resume.then_some(args.report.as_path()))?;
    if resume {
        let report = lock
            .read_resume()
            .await?
            .ok_or_else(|| eyre!("existing calibration report is unavailable"))?;
        // A claimed complete checkpoint must pass every strict gate before it
        // can bypass generation. Keep its lock through verification and baking.
        scope::report(args, identity, &report, report.complete)?;
        if report.complete {
            return Ok(Checkpoint::Complete(lock));
        }
    }
    Ok(if resume {
        Checkpoint::Resume
    } else {
        Checkpoint::Fresh
    })
}

fn artifacts(args: &Args) -> Vec<PathBuf> {
    let mut paths = vec![args.report.clone(), args.status.clone()];
    if args.quarantine_crossed_minutes {
        paths.push(args.report.with_extension("quarantine.json"));
    }
    paths
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
        assert!(matches!(
            runtime
                .block_on(preflight_report(&args, &report.identity))
                .unwrap(),
            Checkpoint::Complete(_)
        ));
        args.only = vec!["XAUEUR".into()];
        assert!(
            runtime
                .block_on(preflight_report(&args, &report.identity))
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&args.report).unwrap(), original);
    }
    #[test]
    fn complete_reports_skip_generation_but_incomplete_reports_use_it_and_invalid_complete_fail() {
        crate::test_logging();
        let dir = tempfile::tempdir().unwrap();
        let mut args = super::super::args::tests::args();
        args.report = dir.path().join("report.json");
        let original = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/validation/ctrader-demo-xpdusd-90d-report.json"
        ));
        std::fs::write(&args.report, original).unwrap();
        let mut report: ctrader::Report = serde_json::from_str(original).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let checkpoint = runtime
            .block_on(preflight_report(&args, &report.identity))
            .unwrap();
        let lock = finish_collection(checkpoint, |_| {
            color_eyre::eyre::bail!("complete report invoked generator")
        })
        .unwrap();
        assert!(Destination::acquire(&args.report, Some(&args.report)).is_err());
        assert_eq!(std::fs::read_to_string(&args.report).unwrap(), original);
        drop(lock);
        report.complete = false;
        std::fs::write(&args.report, serde_json::to_vec(&report).unwrap()).unwrap();
        let checkpoint = runtime
            .block_on(preflight_report(&args, &report.identity))
            .unwrap();
        let mut called = false;
        let lock = finish_collection(checkpoint, |resume| {
            assert!(resume);
            called = true;
            Destination::acquire(&args.report, Some(&args.report))
        })
        .unwrap();
        assert!(called);
        drop(lock);
        report.complete = true;
        report.instruments[0].measurement.profile.n_bars = 0;
        std::fs::write(&args.report, serde_json::to_vec(&report).unwrap()).unwrap();
        assert!(
            runtime
                .block_on(preflight_report(&args, &report.identity))
                .is_err()
        );
    }
    #[test]
    fn report_audit_and_lock_do_not_invalidate_the_requested_source_pin() {
        crate::test_logging();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "// fixture").unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
        )
        .unwrap();
        source::output(root, &["init", "--quiet"]).unwrap();
        assert!(
            std::process::Command::new("cargo")
                .args(["generate-lockfile", "--offline"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        source::output(root, &["add", "--", "."]).unwrap();
        source::output(
            root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        )
        .unwrap();
        let mut args = super::super::args::tests::args();
        args.report = root.join("r.json");
        args.status = root.join("s.json");
        args.quarantine_crossed_minutes = true;
        let pinned = source::Source::pin(root, false, &artifacts(&args)).unwrap();
        let audit = args.report.with_extension("quarantine.json");
        for path in [
            args.report.clone(),
            args.status.clone(),
            audit.clone(),
            audit.with_extension("json.lock"),
        ] {
            std::fs::write(path, "generated artifact").unwrap();
            pinned.verify(false).unwrap();
        }
        std::fs::write(root.join("unrelated.json"), "real build input").unwrap();
        assert!(pinned.verify(false).is_err());
    }
}
