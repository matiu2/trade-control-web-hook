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
    // scripts set `TRADE_CONTROL_WEBHOOK` before building so each suffixed
    // binary (`tv-arm-staging`, `tv-arm-dev`, …) embeds its own
    // environment's URL as the TradingView alert destination
    // (`web_hook` in the tv-mcp JS template). A plain `cargo install` with
    // no env set falls back to the local dev worker on loopback (both dev and
    // staging are native/Postgres workers now — Cloudflare is fully retired;
    // the deploy scripts set TRADE_CONTROL_WEBHOOK explicitly per environment).
    let webhook = std::env::var("TRADE_CONTROL_WEBHOOK")
        .unwrap_or_else(|_| "http://127.0.0.1:8787".to_string());
    println!("cargo:rustc-env=BAKED_WEBHOOK={webhook}");
    println!("cargo:rerun-if-env-changed=TRADE_CONTROL_WEBHOOK");

    // Bake this environment's CLI suffix (`dev` / `staging`, empty for a plain
    // `cargo build`). The deploy scripts set `TRADE_CONTROL_ENV_SUFFIX` so
    // `tv-arm-<suffix> --replay` shells out to the matching
    // `replay-candles-<suffix>` binary. An empty suffix falls back to the plain
    // `replay-candles` on PATH.
    let env_suffix = std::env::var("TRADE_CONTROL_ENV_SUFFIX").unwrap_or_default();
    println!("cargo:rustc-env=BAKED_ENV_SUFFIX={env_suffix}");
    println!("cargo:rerun-if-env-changed=TRADE_CONTROL_ENV_SUFFIX");
}
