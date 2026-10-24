//! Keep resume arguments compatible, and promotion strictly additive and complete.
use super::{args::Args, status::Status, steps::Step};
use color_eyre::{Result, eyre::ensure};
use std::path::Path;

pub fn generate(args: &Args, binary: &Path, resume: bool) -> Step {
    let mut flags = vec![
        "--account".into(),
        args.account.clone(),
        "--out".into(),
        args.report.to_string_lossy().into(),
        "--window-hours".into(),
        args.window_hours.to_string(),
    ];
    if resume {
        flags.extend(["--resume".into(), args.report.to_string_lossy().into()]);
    } else {
        // Separate process arguments preserve spaces in exact native names.
        for name in &args.only {
            flags.extend(["--only".into(), name.clone()]);
        }
        flags.extend([
            "--from".into(),
            args.from.to_rfc3339(),
            "--to".into(),
            args.to.to_rfc3339(),
        ]);
    }
    Step {
        state: "calibrating-nonstocks",
        program: binary.to_string_lossy().into(),
        args: flags,
    }
}

pub fn bake(args: &Args, binary: &Path, table: &Path) -> Step {
    Step {
        state: "baking-nonstocks",
        program: binary.to_string_lossy().into(),
        args: vec![
            "--account".into(),
            args.account.clone(),
            "--report".into(),
            args.report.to_string_lossy().into(),
            "--out".into(),
            table.to_string_lossy().into(),
            "--merge".into(),
        ],
    }
}

pub fn execute(step: &Step, workspace: &Path, status: &Status) -> Result<()> {
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
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn fresh_and_resume_match_generator_contract_and_bake_is_strictly_additive() {
        crate::test_logging();
        let a = super::super::args::tests::args();
        let fresh = generate(&a, Path::new("generate-ctrader"), false);
        let fresh = spread_baseline_gen::ctrader::generate::Args::try_parse_from(
            std::iter::once("generate-ctrader".to_string()).chain(fresh.args),
        )
        .unwrap();
        assert_eq!(fresh.only, a.only);
        assert_eq!(fresh.from, Some(a.from));
        assert_eq!(fresh.to, Some(a.to));
        let resume = generate(&a, Path::new("generate-ctrader"), true);
        let resume = spread_baseline_gen::ctrader::generate::Args::try_parse_from(
            std::iter::once("generate-ctrader".to_string()).chain(resume.args),
        )
        .unwrap();
        assert_eq!(resume.resume, Some(a.report.clone()));
        assert!(resume.only.is_empty());
        let bake = bake(&a, Path::new("bake-ctrader"), Path::new("ctrader.rs"));
        assert!(bake.args.contains(&"--merge".into()));
        assert!(!bake.args.contains(&"--allow-partial".into()));
    }
    #[test]
    fn failed_step_stops_following_commands() {
        crate::test_logging();
        let dir = tempfile::tempdir().unwrap();
        let s = Status::acquire(&dir.path().join("r.json"), &dir.path().join("s.json")).unwrap();
        let bad = Step {
            state: "checking",
            program: "false".into(),
            args: vec![],
        };
        let marker = dir.path().join("must-not-run");
        let later = Step {
            state: "deploying",
            program: "touch".into(),
            args: vec![marker.to_string_lossy().into()],
        };
        let run = || -> Result<()> {
            execute(&bad, dir.path(), &s)?;
            execute(&later, dir.path(), &s)
        };
        assert!(run().is_err());
        assert!(!marker.exists());
    }
}
