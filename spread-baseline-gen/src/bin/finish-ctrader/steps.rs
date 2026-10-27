//! Explicit ordered commands, with deployment absent unless requested.
#[derive(Debug, Clone)]
pub struct Step {
    pub state: &'static str,
    pub program: String,
    pub args: Vec<String>,
}
fn command(state: &'static str, program: &str, args: &[&str]) -> Step {
    Step {
        state,
        program: program.into(),
        args: args.iter().map(|a| (*a).into()).collect(),
    }
}
pub fn validation(deploy: bool) -> Vec<Step> {
    let mut steps = vec![
        command(
            "checking-cache",
            "cargo",
            &[
                "test",
                "--release",
                "--offline",
                "--locked",
                "--manifest-path",
                "../candle-cache/Cargo.toml",
                "--features",
                "ctrader",
                "--lib",
            ],
        ),
        command(
            "linting-cache",
            "cargo",
            &[
                "clippy",
                "--release",
                "--offline",
                "--locked",
                "--manifest-path",
                "../candle-cache/Cargo.toml",
                "--features",
                "ctrader",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ),
        command(
            "formatting-cache",
            "cargo",
            &[
                "fmt",
                "--manifest-path",
                "../candle-cache/Cargo.toml",
                "--",
                "--check",
            ],
        ),
    ];
    let packages = [
        "spread-baseline-gen",
        "broker-ctrader",
        "trade-control-core",
        "trade-control-cli",
        "tv-arm",
        "trade-control-worker",
    ];
    for mode in ["test", "clippy"] {
        let mut args = vec![
            mode.to_string(),
            "--release".into(),
            "--offline".into(),
            "--locked".into(),
        ];
        args.extend(packages.into_iter().flat_map(|p| ["-p".into(), p.into()]));
        args.extend(if mode == "test" {
            vec!["--lib".into(), "--bins".into()]
        } else {
            vec![
                "--all-targets".into(),
                "--".into(),
                "-D".into(),
                "warnings".into(),
            ]
        });
        steps.push(Step {
            state: if mode == "test" {
                "checking-consumers"
            } else {
                "linting-consumers"
            },
            program: "cargo".into(),
            args,
        });
    }
    steps.push(command(
        "formatting-consumers",
        "cargo",
        &["fmt", "--all", "--", "--check"],
    ));
    if deploy {
        steps.push(command("deploying-staging", "./deploy-staging.sh", &[]));
        steps.push(command(
            "verifying-staging",
            "systemctl",
            &[
                "--user",
                "is-active",
                "--quiet",
                "trade-control-worker-staging.service",
            ],
        ));
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_never_deploys_or_publishes_and_all_cargo_compiles_are_release() {
        crate::test_logging();
        let steps = validation(false);
        assert!(steps.iter().all(|s| s.program == "cargo"));
        assert!(
            steps
                .iter()
                .filter(|s| s.args.first().is_some_and(|a| a == "test" || a == "clippy"))
                .all(|s| s.args.iter().any(|a| a == "--release"))
        );
        let deployed = validation(true);
        assert_eq!(deployed[deployed.len() - 2].program, "./deploy-staging.sh");
        assert_eq!(deployed.last().unwrap().state, "verifying-staging");
        assert_eq!(deployed.len(), steps.len() + 2);
    }
}
