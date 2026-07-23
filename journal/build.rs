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
    // Re-run when HEAD or the tag set moves so the baked string stays fresh.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/tags");

    let env_suffix = std::env::var("TRADE_CONTROL_ENV_SUFFIX").unwrap_or_default();
    println!("cargo:rustc-env=BAKED_ENV_SUFFIX={env_suffix}");
    println!("cargo:rerun-if-env-changed=TRADE_CONTROL_ENV_SUFFIX");
}
