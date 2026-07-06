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
    // no env set falls back to the local dev worker (dev is the native/Postgres
    // worker on loopback now; only staging is on Cloudflare, and
    // deploy-staging.sh sets TRADE_CONTROL_WEBHOOK explicitly).
    let webhook = std::env::var("TRADE_CONTROL_WEBHOOK")
        .unwrap_or_else(|_| "http://127.0.0.1:8787".to_string());
    println!("cargo:rustc-env=BAKED_WEBHOOK={webhook}");
    println!("cargo:rerun-if-env-changed=TRADE_CONTROL_WEBHOOK");

    // Bake a Pine study title. LEGACY / DEAD: signal detection moved fully into
    // Rust (core/src/signals/, evaluated server-side as PinePattern). tv-arm no
    // longer matches a chart study by name, and nothing reads BAKED_PINE_NAME —
    // it's baked only to keep the deploy_env signature stable. The deploy
    // scripts still pass TRADE_CONTROL_PINE_NAME (dev and staging use the same
    // value now); it has no runtime effect. Left in place rather than ripped out
    // mid-week; safe to remove once the deploy scripts drop the arg.
    //
    // Default stays in sync with the canonical `trade_control_conventions::pine::
    // PINE_INDICATOR_NAME` for a plain `cargo install` (no env set).
    let pine_name =
        std::env::var("TRADE_CONTROL_PINE_NAME").unwrap_or_else(|_| "Candle Signals".to_string());
    println!("cargo:rustc-env=BAKED_PINE_NAME={pine_name}");
    println!("cargo:rerun-if-env-changed=TRADE_CONTROL_PINE_NAME");
}
