//! Prevent offline commands from reaching the direct position-entry POST.

use color_eyre::eyre::{Result, eyre};

use crate::args::Args;

pub(crate) fn validate_command(args: &Args) -> Result<()> {
    if args.position_entry_mode().is_some() && (args.plan_out().is_some() || args.replay()) {
        return Err(eyre!(
            "manual position entries do not produce an engine plan; plan-out and replay \
             are unavailable with --market-entry, --stop-entry or --limit-entry. \
             Use register to submit an entry, or --broker-dry-run register for a \
             server check without placing an order"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn offline_position_commands_fail_before_reading_the_spec_or_posting() {
        for entry in ["--market-entry", "--stop-entry", "--limit-entry"] {
            for command in [
                vec!["plan-out", "/tmp/unused-position-plan.json"],
                vec!["replay"],
            ] {
                let args = Args::try_parse_from(
                    ["tv-arm", "--spec-in", "/missing-position-spec.json", entry]
                        .into_iter()
                        .chain(command),
                )
                .expect("valid command syntax");
                let error = crate::pipeline::run(args)
                    .expect_err("offline commands must never submit a position entry")
                    .to_string();
                assert!(error.contains("do not produce an engine plan"), "{error}");
                assert!(!error.contains("missing-position-spec"), "{error}");
            }
        }
    }

    #[test]
    fn register_and_pattern_offline_commands_remain_available() {
        for tokens in [
            vec!["tv-arm", "--market-entry", "register"],
            vec!["tv-arm", "--stop-entry", "--broker-dry-run", "register"],
            vec!["tv-arm", "--limit-entry"],
            vec!["tv-arm", "plan-out", "/tmp/pattern-plan.json"],
            vec!["tv-arm", "replay"],
        ] {
            let args = Args::try_parse_from(tokens).expect("valid command syntax");
            validate_command(&args).expect("supported command");
        }
    }
}
