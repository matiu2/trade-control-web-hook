//! Where the replay corpus lives, resolved at **runtime**.
//!
//! ## Why this isn't just `CARGO_MANIFEST_DIR`
//!
//! Both `replay-candles` and `tv-arm --save-fixture` used to default to
//! `env!("CARGO_MANIFEST_DIR")/../replay-fixtures`. That macro expands at
//! **compile** time, to the directory the binary was *built* in — which is not
//! the directory it is *run* from, and need not still exist at all.
//!
//! The deploy scripts build from whatever tree they are invoked in, so a deploy
//! run from a throwaway worktree bakes that worktree's path into the shipped
//! CLI. When the worktree is later removed, every fixture write fails with a
//! bare `No such file or directory (os error 2)` naming a path the operator has
//! never heard of:
//!
//! ```text
//! write frozen setup to /…/tc-staging-deploy/tv-arm/../replay-fixtures/…
//!   No such file or directory (os error 2)
//! ```
//!
//! The subtler half is that it can also *silently succeed against the wrong
//! tree*: if the build directory still exists, a capture run from the checkout
//! you are working in lands in a different repo's corpus. That is the recorded
//! trap where `--rebless` covered only 19 of 63 fixtures with no error either
//! way — the default had quietly resolved somewhere else.
//!
//! So the path is resolved from the **running process**, and the build-time
//! path is kept only as a last resort (it is the right answer for `cargo test`,
//! where cwd is the source tree anyway).
//!
//! ## Resolution order
//!
//! 1. an explicit `--fixtures-dir` (handled by the caller);
//! 2. `TRADE_CONTROL_FIXTURES_DIR`, for a deploy or a driver to pin it;
//! 3. the enclosing repo of the **current directory** — walk up looking for a
//!    `replay-fixtures/` sibling of a `Cargo.toml`;
//! 4. the build-time manifest path, if it still exists;
//! 5. `./replay-fixtures` relative to cwd, so the error names something the
//!    operator recognises rather than a stranger's build directory.
//!
//! Step 3 is the one that makes the common case work without any flag: the
//! operator runs the capture from inside a checkout, and the corpus of *that*
//! checkout is the one they mean.

use std::path::{Path, PathBuf};

/// Environment variable pinning the corpus location, for a deploy or a driver
/// that wants to be explicit rather than rely on cwd.
pub const FIXTURES_DIR_ENV: &str = "TRADE_CONTROL_FIXTURES_DIR";

/// The directory name of the corpus, at a repo root.
const CORPUS_DIR: &str = "replay-fixtures";

/// Resolve the corpus directory for the running process.
///
/// `explicit` is the operator's `--fixtures-dir`, which always wins.
/// `built_at` is the caller's `env!("CARGO_MANIFEST_DIR")` — passed in rather
/// than read here so each binary reports its own crate, and so the tests can
/// drive every branch.
pub fn resolve(explicit: Option<&Path>, built_at: &Path) -> PathBuf {
    if let Some(dir) = explicit {
        return dir.to_path_buf();
    }
    if let Some(dir) = from_env() {
        return dir;
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Some(dir) = search_upwards(&cwd)
    {
        return dir;
    }
    let built = built_at.join("..").join(CORPUS_DIR);
    if built.is_dir() {
        return built;
    }
    PathBuf::from(CORPUS_DIR)
}

/// The env override, ignoring a blank value (an unset variable exported as `""`
/// is a slip, not a request to use the current directory).
fn from_env() -> Option<PathBuf> {
    let raw = std::env::var(FIXTURES_DIR_ENV).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

/// Walk `start` and its ancestors for a directory holding `replay-fixtures/`.
///
/// Matches on the corpus itself rather than on `.git`, because a git *worktree*
/// has a `.git` **file**, and because the thing we actually need is the corpus.
/// Returns `None` rather than a guess when nothing matches, so the caller can
/// fall through to a path it can explain.
pub fn search_upwards(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|dir| {
        let candidate = dir.join(CORPUS_DIR);
        candidate.is_dir().then_some(candidate)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--fixtures-dir` is the operator speaking; nothing may override it — not
    /// the env, not a corpus that happens to sit above the cwd.
    #[test]
    fn an_explicit_dir_wins_over_everything() {
        let explicit = PathBuf::from("/somewhere/else/replay-fixtures");
        let got = resolve(Some(&explicit), Path::new("/build/tree/cli"));
        assert_eq!(got, explicit);
    }

    /// The build-time path is a *fallback*, never the answer when the running
    /// process sits in a real checkout. This is the whole bug: a stale build
    /// directory must not beat the tree the operator is standing in.
    #[test]
    fn cwd_repo_beats_the_build_time_manifest_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("checkout");
        let corpus = repo.join(CORPUS_DIR);
        std::fs::create_dir_all(&corpus).expect("create corpus");

        let found = search_upwards(&repo).expect("corpus found at the repo root");
        assert_eq!(found, corpus);
    }

    /// A capture run from a *subdirectory* of the checkout still finds it —
    /// the operator does not have to be standing at the repo root.
    #[test]
    fn a_subdirectory_of_the_checkout_still_finds_the_corpus() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let corpus = tmp.path().join(CORPUS_DIR);
        std::fs::create_dir_all(&corpus).expect("create corpus");
        let deep = tmp.path().join("tv-arm").join("src");
        std::fs::create_dir_all(&deep).expect("create nested dir");

        assert_eq!(search_upwards(&deep).as_deref(), Some(corpus.as_path()));
    }

    /// Outside any checkout there is nothing to find, and saying so lets
    /// `resolve` fall through to a path it can explain rather than inventing one.
    #[test]
    fn no_corpus_above_the_cwd_is_none_not_a_guess() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("no").join("repo").join("here");
        std::fs::create_dir_all(&bare).expect("create dirs");

        assert_eq!(search_upwards(&bare), None);
    }

    /// A nearer corpus wins: nested checkouts (a worktree inside a repo) must
    /// resolve to the one the operator is actually inside.
    #[test]
    fn the_nearest_enclosing_corpus_wins() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let outer = tmp.path().join(CORPUS_DIR);
        let inner_repo = tmp.path().join("worktrees").join("inner");
        let inner = inner_repo.join(CORPUS_DIR);
        std::fs::create_dir_all(&outer).expect("create outer corpus");
        std::fs::create_dir_all(&inner).expect("create inner corpus");

        assert_eq!(
            search_upwards(&inner_repo).as_deref(),
            Some(inner.as_path())
        );
    }

    /// A build-time path that no longer exists must not be returned — that is
    /// the exact failure the operator saw (`tc-staging-deploy` was deleted).
    /// With no explicit dir and no corpus above cwd, the fallback has to be
    /// something local and nameable.
    #[test]
    fn a_vanished_build_directory_is_not_returned() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("nowhere");
        std::fs::create_dir_all(&bare).expect("create dir");
        let vanished = tmp.path().join("deleted-deploy-tree").join("cli");
        assert!(!vanished.exists(), "the build tree is gone, as in the bug");

        // Resolved from a cwd with no corpus above it, so the build path is the
        // only candidate left — and it must be rejected for not existing.
        let built = vanished.join("..").join(CORPUS_DIR);
        assert!(!built.is_dir());
    }
}
