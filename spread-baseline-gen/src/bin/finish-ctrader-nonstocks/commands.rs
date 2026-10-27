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
    if args.quarantine_crossed_minutes {
        flags.extend([
            "--quarantine-crossed-minutes".into(),
            "--max-quarantined-minutes".into(),
            args.max_quarantined_minutes.to_string(),
        ]);
    }
    if resume {
        flags.extend(["--resume".into(), args.report.to_string_lossy().into()]);
    } else {
        if args.stocks {
            flags.push("--stocks".into());
        }
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
        state: if args.stocks {
            "calibrating-stocks"
        } else {
            "calibrating-nonstocks"
        },
        program: binary.to_string_lossy().into(),
        args: flags,
    }
}

pub fn bake(args: &Args, binary: &Path, table: &Path) -> Step {
    Step {
        state: if args.stocks {
            "baking-stocks"
        } else {
            "baking-nonstocks"
        },
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
    fn stock_scope_and_native_names_survive_child_arguments_and_resume() {
        crate::test_logging();
        let mut a = super::super::args::tests::args();
        a.stocks = true;
        a.quarantine_crossed_minutes = true;
        a.max_quarantined_minutes = 2;
        a.only = vec!["AIA".into(), "REDES ENERGETICAS NACIONAIS".into()];
        let parse = |resume| {
            spread_baseline_gen::ctrader::generate::Args::try_parse_from(
                std::iter::once("generate-ctrader".to_string())
                    .chain(generate(&a, Path::new("generate-ctrader"), resume).args),
            )
            .unwrap()
        };
        let fresh = parse(false);
        assert!(fresh.stocks);
        assert!(fresh.quarantine_crossed_minutes);
        assert_eq!(fresh.max_quarantined_minutes, 2);
        assert_eq!(fresh.only, a.only);
        let resume = parse(true);
        assert!(!resume.stocks);
        assert!(resume.quarantine_crossed_minutes);
        assert_eq!(resume.max_quarantined_minutes, 2);
        assert_eq!(resume.resume, Some(a.report.clone()));
        assert_eq!(
            bake(&a, Path::new("bake-ctrader"), Path::new("ctrader.rs")).state,
            "baking-stocks"
        );
    }

    #[test]
    fn explicit_quality_policy_is_forwarded_for_fresh_and_resume() {
        crate::test_logging();
        let mut a = super::super::args::tests::args();
        a.quarantine_crossed_minutes = true;
        a.max_quarantined_minutes = 2;
        for resume in [false, true] {
            let command = generate(&a, Path::new("generate-ctrader"), resume);
            let parsed = spread_baseline_gen::ctrader::generate::Args::try_parse_from(
                std::iter::once("generate-ctrader".to_string()).chain(command.args),
            )
            .unwrap();
            assert!(parsed.quarantine_crossed_minutes);
            assert_eq!(parsed.max_quarantined_minutes, 2);
        }
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
