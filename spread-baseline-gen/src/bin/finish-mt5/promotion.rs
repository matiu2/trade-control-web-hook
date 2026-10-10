//! Explicitly requested staging promotion; never invokes a production deploy.
use color_eyre::{Result, eyre::ensure};
use std::{path::Path, process::Command};

pub fn publish_main(repo: &Path) -> Result<()> {
    git(repo, &["fetch", "origin", "main"])?;
    let head = git(repo, &["rev-parse", "HEAD"])?;
    ensure!(
        git(repo, &["branch", "--show-current"])? == "staging",
        "promotion requires staging"
    );
    // Refuse to overwrite independently advanced main commits.
    git(repo, &["merge-base", "--is-ancestor", "origin/main", &head])?;
    git(repo, &["merge-base", "--is-ancestor", "main", &head])?;
    git(
        repo,
        &["push", "origin", &format!("{head}:refs/heads/main")],
    )?;
    if !git(repo, &["worktree", "list", "--porcelain"])?
        .lines()
        .any(|line| line == "branch refs/heads/main")
    {
        git(repo, &["branch", "-f", "main", &head])?;
    }
    tracing::info!(%head, "main fast-forwarded to validated staging release");
    Ok(())
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    tracing::info!(?args, "MT5 release promotion");
    let result = Command::new("git").current_dir(repo).args(args).output()?;
    ensure!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    #[test]
    fn publishes_fast_forward_but_preserves_diverged_main() {
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let remote = dir.path().join("remote");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--bare"]).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "Test"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.invalid"]).unwrap();
        git(&repo, &["commit", "--allow-empty", "-m", "base"]).unwrap();
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )
        .unwrap();
        git(&repo, &["push", "origin", "main"]).unwrap();
        git(&repo, &["checkout", "-b", "staging"]).unwrap();
        git(&repo, &["commit", "--allow-empty", "-m", "release"]).unwrap();
        publish_main(&repo).unwrap();
        let release = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(git(&remote, &["rev-parse", "main"]).unwrap(), release);
        assert_eq!(git(&repo, &["rev-parse", "main"]).unwrap(), release);
        git(&repo, &["checkout", "main"]).unwrap();
        git(
            &repo,
            &["commit", "--allow-empty", "-m", "independent main"],
        )
        .unwrap();
        git(&repo, &["push", "origin", "main"]).unwrap();
        let main = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        git(&repo, &["checkout", "staging"]).unwrap();
        git(&repo, &["commit", "--allow-empty", "-m", "next release"]).unwrap();
        assert!(publish_main(&repo).is_err());
        assert_eq!(git(&remote, &["rev-parse", "main"]).unwrap(), main);
        assert_eq!(
            git(&repo, &["branch", "--show-current"]).unwrap(),
            "staging"
        );
    }
}
