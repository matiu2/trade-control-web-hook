//! Bake two compile-time strings into the `journal` binary:
//!
//! * `GIT_VERSION` — the git tag/commit the binary was built from, so
//!   `--version` reports it (falls back to the crate version off-git).
//! * `BAKED_ENV_SUFFIX` — this environment's CLI suffix (`dev` / `staging`,
//!   empty for a plain `cargo build`). The deploy scripts set
//!   `TRADE_CONTROL_ENV_SUFFIX` so `journal-<suffix>` shells out to the
//!   matching `trade-control-<suffix>` / `replay-candles-<suffix>` binaries
//!   (same environment). An empty suffix falls back to the plain names on PATH.
//!
//! Unlike `tv-arm`/`cli`, `journal` never posts to the worker directly — it
//! drives the already-baked `trade-control-<suffix>` CLI, which owns the
//! webhook URL. So there is deliberately **no** `BAKED_WEBHOOK` here.

use std::process::Command;

fn main() {
    let describe = Command::new("git")
        .args(["describe", "--tags", "--dirty", "--always"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    println!("cargo:rustc-env=GIT_VERSION={describe}");
    // Re-run when the commit, the branch, or the tag set moves, so the baked
    // string stays fresh.
    //
    // `.git/HEAD` alone is NOT enough: on a branch it holds a constant
    // `ref: refs/heads/<branch>` and only changes when you SWITCH branches, so
    // committing left the baked version frozen at a stale hash (and could bake
    // a stale `-dirty`). The file that actually moves per-commit is the branch
    // ref — which for a packed ref may not exist as a loose file, hence also
    // watching `packed-refs` and the log that every commit appends to.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/tags");
    println!("cargo:rerun-if-changed=../.git/packed-refs");
    println!("cargo:rerun-if-changed=../.git/logs/HEAD");
    if let Some(branch_ref) = current_branch_ref() {
        println!("cargo:rerun-if-changed=../.git/{branch_ref}");
    }

    let env_suffix = std::env::var("TRADE_CONTROL_ENV_SUFFIX").unwrap_or_default();
    println!("cargo:rustc-env=BAKED_ENV_SUFFIX={env_suffix}");
    println!("cargo:rerun-if-env-changed=TRADE_CONTROL_ENV_SUFFIX");
}

/// The path (relative to `.git/`) of the ref HEAD points at, e.g.
/// `refs/heads/staging`. `None` when detached (HEAD holds a raw hash, and it
/// changes on every commit anyway, so watching HEAD alone is then correct).
fn current_branch_ref() -> Option<String> {
    let head = std::fs::read_to_string("../.git/HEAD").ok()?;
    head.trim().strip_prefix("ref: ").map(str::to_string)
}
