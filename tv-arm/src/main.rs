//! `tv-arm` — read a TradingView chart and arm a full reversal-trade
//! bundle (vetoes, preps, enter, close-on-reversal) plus pause/news
//! windows, both operator-drawn and auto-derived from the
//! forex-factory calendar.
//!
//! Port of `scripts/tv_arm_hs.py`. The chart-reading + classification
//! live here; the signing layer is delegated to the `trade-control-cli`
//! crate as a library, and arming registers the signed `TradePlan` with
//! the worker's server-side engine (the legacy TradingView-alert POST
//! path has been retired).

// Some library-style helpers (`horizontal_price`,
// `DrawShapeResult.shape`/`.entity_id`, etc.) aren't consumed by the
// current pipeline but are public API for downstream tools; suppress
// the dead-code warnings rather than dropping useful surface.
#![allow(dead_code)]

use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use color_eyre::eyre::Result;

mod args;
mod geometry;
mod instrument_recovery;
mod instrument_resolution;
mod mw_geometry;
mod pipeline;
mod position_trade;
mod register_post;
mod roles;
mod spread;
mod timeframe;
mod trade_plan_build;

use crate::args::Args;

/// Hand-rolled zsh helper appended after the clap-generated tv-arm
/// completion. Mirrors the pattern in `trade-control` for
/// `_trade_control_tn_instruments`: define a helper, let the user wire
/// it in zshrc. We don't auto-wire because clap regenerates its script
/// every time we rename a flag, and an automatic compdef override
/// would race that.
///
/// To use, add to zshrc *after* sourcing the tv-arm completion file:
///
/// ```zsh
/// compdef -e "_arguments -S '--account-id=[server-side account name]:account:_tv_arm_account_names'" tv-arm
/// ```
///
/// (Or use `compctl`/zstyle if you prefer; the helper is the load-
/// bearing bit.)
const ZSH_ACCOUNT_ID_HOOK: &str = r#"
# tv-arm: --account-id completer. Lists every locally-known account name
# (operator history ∪ local TN store) via `trade-control account names`.
# No admin key or network call — safe to invoke on every TAB.
_tv_arm_account_names() {
    local -a names
    names=("${(@f)$(trade-control account names 2>/dev/null)}")
    compadd -- "${names[@]}"
}
"#;

fn main() -> Result<ExitCode> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let parsed = Args::parse();

    if parsed.print_completions {
        let mut cmd = Args::command();
        // Bind the completion to the actual invoked binary name (argv[0]
        // stem) so a renamed-on-install copy (`tv-arm-staging`,
        // `tv-arm-dev`) emits completions for *its own* name, not the
        // static `tv-arm`. Falls back to the clap command name.
        let name = std::env::args()
            .next()
            .and_then(|a| {
                std::path::Path::new(&a)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| cmd.get_name().to_string());
        generate(Shell::Zsh, &mut cmd, name, &mut std::io::stdout());
        // Append a dynamic completer for --account-id. The clap-generated
        // script lists it as a value but with no completion source; this
        // hook overrides its action to call `trade-control account names`,
        // which prints the union of locally-known account names with no
        // auth or network. See `pipeline.rs:resolve_account` for the
        // selection precedence the flag follows.
        print!("{ZSH_ACCOUNT_ID_HOOK}");
        return Ok(ExitCode::SUCCESS);
    }

    let code = pipeline::run(parsed.apply_aliases())?;
    Ok(ExitCode::from(code as u8))
}
