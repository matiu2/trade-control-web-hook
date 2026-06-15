//! Capture the git version at build time so `--version` reports the
//! tag/commit the binary was built from (e.g. `v6`, `v6-2-gabc123-dirty`),
//! not the never-bumped crate version. Falls back to the crate version
//! when git isn't available (e.g. a source-tarball build).

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

    // Bake the per-environment webhook URL into the binary. The deploy
    // scripts (`deploy-dev.sh` / `deploy-staging.sh` / future
    // `deploy-live.sh`) set `TRADE_CONTROL_WEBHOOK` before building so each
    // suffixed binary (`trade-control-staging`, `-dev`, …) contains its own
    // environment's URL as the compiled-in default endpoint. A plain
    // `cargo install` with no env set falls back to the dev URL.
    let webhook = std::env::var("TRADE_CONTROL_WEBHOOK")
        .unwrap_or_else(|_| "https://trade-control-web-hook.msherborne.workers.dev".to_string());
    println!("cargo:rustc-env=BAKED_WEBHOOK={webhook}");
    // The value is an env input, not a file — re-run when it changes.
    println!("cargo:rerun-if-env-changed=TRADE_CONTROL_WEBHOOK");
}
