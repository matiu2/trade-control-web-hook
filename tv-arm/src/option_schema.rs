//! GUI controls derived from the same clap definitions as both commands.

use std::any::TypeId;

use clap::{ArgAction, CommandFactory};
use serde::Serialize;
use trade_control_cli::replay_args::ReplayArgs;

use crate::args::Args;

#[derive(Debug, Serialize)]
pub struct OptionSchema {
    pub arm: Vec<OptionDescriptor>,
    pub register: Vec<OptionDescriptor>,
    pub replay: Vec<OptionDescriptor>,
}

#[derive(Debug, Serialize)]
pub struct OptionDescriptor {
    pub id: String,
    pub long: String,
    pub label: String,
    pub help: String,
    pub kind: &'static str,
    pub choices: Vec<String>,
    pub defaults: Vec<String>,
    pub group: &'static str,
    pub repeatable: bool,
    pub optional_value: bool,
    pub takes_value: bool,
}

const ARM_INTERNAL: &[&str] = &[
    "broker",
    "start",
    "spec-in",
    "spec-url",
    "spec-out",
    "spec-note",
    "tv-mcp-root",
    "new-tv",
    "print-completions",
    "expiry-hours",
    "help",
    "version",
];

const REPLAY_INTERNAL: &[&str] = &[
    "plan",
    "instrument",
    "granularity",
    "source",
    "mt5-account",
    "ctrader-account",
    "start",
    "tv-mcp-root",
    "annotate",
    "annotate-unfilled",
    "positions",
    "print-completions",
    "help",
    "version",
];

/// Includes every semantic option; chart identity and transport are supplied by
/// the server. Validation still runs through clap, including conflicts/requires.
pub fn options() -> OptionSchema {
    let arm = Args::command();
    let register = arm
        .find_subcommand("register")
        .map(descriptors)
        .unwrap_or_default();
    OptionSchema {
        arm: descriptors(&arm)
            .into_iter()
            .filter(|d| !ARM_INTERNAL.contains(&d.long.as_str()))
            .collect(),
        register,
        replay: descriptors(&ReplayArgs::command())
            .into_iter()
            .filter(|d| !REPLAY_INTERNAL.contains(&d.long.as_str()) && !d.long.starts_with("arm-"))
            .collect(),
    }
}

fn descriptors(command: &clap::Command) -> Vec<OptionDescriptor> {
    command.get_arguments().filter_map(descriptor).collect()
}

fn descriptor(arg: &clap::Arg) -> Option<OptionDescriptor> {
    let long = arg.get_long()?.to_string();
    if matches!(long.as_str(), "help" | "version") {
        return None;
    }
    let choices: Vec<String> = arg
        .get_value_parser()
        .possible_values()
        .map(|values| {
            values
                .filter(|v| !v.is_hide_set())
                .map(|v| v.get_name().to_string())
                .collect()
        })
        .unwrap_or_default();
    let primitive = kind(arg);
    let kind = if primitive == "boolean" {
        primitive
    } else if !choices.is_empty() {
        "select"
    } else {
        primitive
    };
    let range = arg.get_num_args().unwrap_or_else(|| {
        if matches!(arg.get_action(), ArgAction::SetTrue | ArgAction::SetFalse) {
            0.into()
        } else {
            1.into()
        }
    });
    Some(OptionDescriptor {
        id: arg.get_id().to_string(),
        label: long.replace('-', " "),
        help: arg
            .get_long_help()
            .or_else(|| arg.get_help())
            .map(ToString::to_string)
            .unwrap_or_default(),
        kind,
        defaults: arg
            .get_default_values()
            .iter()
            .map(|v| v.to_string_lossy().into_owned())
            .collect(),
        choices,
        group: group(&long),
        repeatable: matches!(arg.get_action(), ArgAction::Append),
        optional_value: range.min_values() == 0 && range.max_values() > 0,
        takes_value: !matches!(
            arg.get_action(),
            ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count
        ),
        long,
    })
}

fn kind(arg: &clap::Arg) -> &'static str {
    let id = arg.get_value_parser().type_id();
    if id == TypeId::of::<bool>() {
        "boolean"
    } else if id == TypeId::of::<f64>() || id == TypeId::of::<f32>() {
        "number"
    } else if id == TypeId::of::<u32>()
        || id == TypeId::of::<usize>()
        || id == TypeId::of::<u64>()
        || id == TypeId::of::<i64>()
    {
        "integer"
    } else {
        "text"
    }
}

fn group(long: &str) -> &'static str {
    match long {
        "account-id" | "risk-pct" | "risk-amount" | "broker-dry-run" | "max-retries"
        | "expiry-bars" | "blackout-close" => "Risk and order lifetime",
        "entry-market" | "entry-stop" | "entry-limit" | "market-entry" | "stop-entry"
        | "limit-entry" | "recover-entry" | "strategy-v2" | "qm-entry" | "quasimodo" | "trend" => {
            "Entry strategy"
        }
        "sl-anchor"
        | "sl-from-recent"
        | "sl-note-buffer-atr-pct"
        | "no-breakeven"
        | "breakeven-pct"
        | "spread-window"
        | "pip-size"
        | "tick-size" => "Stops and sizing",
        "skip-calendar-bars" | "news-before-hours" | "news-after-hours" | "as-of" => "Calendar",
        "skip-reversals"
        | "reversal-band-pct"
        | "tp-resistance-fib-level"
        | "skip-tp-resistance"
        | "veto-on-reversal"
        | "ma-cross-veto"
        | "sma-200-break-veto" => "Exit rules",
        "replace" | "shadow" => "Registration",
        "simulate"
        | "verbose"
        | "candle-detector-direction"
        | "candle-detector-golden"
        | "end"
        | "mt5-history"
        | "warmup-bars"
        | "cron-gap"
        | "upkeep"
        | "probe-account" => "Replay",
        "cache-dir" | "fixtures-dir" | "test-mode" | "fixture" | "fixtures-glob" | "json"
        | "check" | "rebless" | "baseline" | "bless-baseline" | "baseline-label" | "save"
        | "message" | "trade-ref" | "save-fixture" | "save-matrix" | "sl-matrix"
        | "entry-matrix" | "reversal-matrix" | "fixture-name" | "spec-note" => {
            "Fixtures and advanced output"
        }
        _ => "Entry conditions",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    fn init() {
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    }

    #[test]
    fn schema_covers_semantics_and_hides_transport() {
        init();
        let schema = options();
        assert!(schema.arm.iter().any(|d| d.long == "enable-on-ma-touch"));
        let alignment = schema
            .arm
            .iter()
            .find(|d| d.long == "ma-cross-pause")
            .unwrap();
        assert_eq!(alignment.id, "ma_cross_pause");
        assert_eq!(alignment.label, "ma cross pause");
        assert!(!schema.arm.iter().any(|d| d.long == "require-ma-alignment"));
        assert_eq!(alignment.kind, "boolean");
        assert_eq!(alignment.group, "Entry conditions");
        assert!(alignment.help.contains("Unfilled orders are held"));
        let cross = schema
            .arm
            .iter()
            .find(|d| d.long == "ma-cross-veto")
            .unwrap();
        assert_eq!(cross.kind, "boolean");
        assert_eq!(cross.group, "Exit rules");
        assert!(cross.help.contains("actual fill"));
        let sma = schema
            .arm
            .iter()
            .find(|d| d.long == "sma-200-break-veto")
            .unwrap();
        assert_eq!(sma.kind, "boolean");
        assert_eq!(sma.group, "Exit rules");
        assert!(schema.replay.iter().any(|d| d.long == "bless-baseline"));
        assert!(
            schema
                .register
                .iter()
                .any(|d| d.long == "replace" && d.optional_value)
        );
        assert!(!schema.arm.iter().any(|d| d.long == "spec-url"));
        assert!(!schema.replay.iter().any(|d| d.long == "annotate"));
        let anchor = schema.arm.iter().find(|d| d.long == "sl-anchor").unwrap();
        assert_eq!(anchor.choices, ["signal", "invalidation", "fib-top"]);
    }

    #[test]
    fn typed_controls_and_defaults_come_from_clap() {
        init();
        let schema = options();
        let option = |long: &str| schema.arm.iter().find(|d| d.long == long).unwrap();
        assert_eq!(option("risk-pct").kind, "number");
        assert_eq!(option("max-retries").kind, "integer");
        assert_eq!(option("skip-golden").kind, "boolean");
        assert!(!option("skip-golden").takes_value);
        assert!(option("pull-back").optional_value);
        assert_eq!(option("reversal-band-pct").defaults, ["0.1"]);
        let simulate = schema.replay.iter().find(|d| d.long == "simulate").unwrap();
        assert_eq!(simulate.kind, "boolean");
        assert!(simulate.takes_value);
    }
}
