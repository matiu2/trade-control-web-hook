//! Account-native validation must complete before signing a state-changing control.
use super::{BrokerKind, BrokerKindArg, Intent, Result, eyre, validate_instrument};

pub(super) fn canonicalize_instrument(
    broker: BrokerKindArg,
    account: Option<&str>,
    instrument: &str,
) -> Result<String> {
    canonicalize_instrument_or_force(broker, account, instrument, false)
}

pub(super) fn canonicalize_instrument_or_force(
    broker: BrokerKindArg,
    account: Option<&str>,
    instrument: &str,
    force: bool,
) -> Result<String> {
    canonicalize_with(
        broker.into(),
        account,
        instrument,
        force,
        validate_instrument,
    )
}

fn canonicalize_with(
    broker: BrokerKind,
    account: Option<&str>,
    instrument: &str,
    force: bool,
    validate: impl FnOnce(BrokerKind, Option<&str>, &str) -> Result<Option<String>>,
) -> Result<String> {
    if broker == BrokerKind::Ctrader && account.is_none_or(|name| name.trim().is_empty()) {
        return Err(eyre!(
            "cTrader controls require --account with a named Open API profile; FIX QUOTE profiles cannot manage plan state"
        ));
    }
    if force && broker != BrokerKind::Ctrader {
        tracing::warn!(
            input = instrument,
            "--force: sending instrument verbatim, skipping catalog validation"
        );
        return Ok(instrument.to_owned());
    }
    match validate(broker, account, instrument)? {
        Some(canonical) => {
            tracing::warn!(input = instrument, %canonical, "instrument resolved to canonical broker name");
            Ok(canonical)
        }
        None => Ok(instrument.to_owned()),
    }
}

pub(super) fn scope_ctrader_control(
    intent: &mut Intent,
    broker: BrokerKindArg,
    account: Option<&str>,
) -> Result<()> {
    if BrokerKind::from(broker) == BrokerKind::Ctrader {
        let account = account
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| eyre!("cTrader controls require --account"))?;
        intent.broker = BrokerKind::Ctrader;
        intent.account = Some(account.to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn init_tracing() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    }

    #[test]
    fn ctrader_controls_forward_account_and_canonical_instrument() {
        init_tracing();
        let actual = canonicalize_with(
            BrokerKind::Ctrader,
            Some("demo-account"),
            "AUD/USD",
            false,
            |broker, account, instrument| {
                assert_eq!(broker, BrokerKind::Ctrader);
                assert_eq!(account, Some("demo-account"));
                assert_eq!(instrument, "AUD/USD");
                Ok(Some("AUDUSD".into()))
            },
        )
        .unwrap();
        assert_eq!(actual, "AUDUSD");
    }

    #[test]
    fn force_cannot_bypass_fix_profile_refusal() {
        init_tracing();
        let result = canonicalize_with(
            BrokerKind::Ctrader,
            Some("fix-demo"),
            "AUDUSD",
            true,
            |_, account, _| {
                assert_eq!(account, Some("fix-demo"));
                Err(eyre!(
                    "FIX QUOTE profile is quotes-only; configure Open API"
                ))
            },
        );
        assert!(result.unwrap_err().to_string().contains("quotes-only"));
    }

    #[test]
    fn missing_ctrader_account_fails_before_validator() {
        init_tracing();
        for force in [false, true] {
            assert!(
                canonicalize_with(
                    BrokerKind::Ctrader,
                    None,
                    "AUDUSD",
                    force,
                    |_, _, _| panic!("no configuration should be read")
                )
                .unwrap_err()
                .to_string()
                .contains("--account")
            );
        }
    }

    #[test]
    fn other_broker_force_keeps_legacy_catalog_bypass() {
        init_tracing();
        assert_eq!(
            canonicalize_with(
                BrokerKind::TradeNation,
                None,
                "legacy-symbol",
                true,
                |_, _, _| panic!("legacy force skips validation")
            )
            .unwrap(),
            "legacy-symbol"
        );
    }

    #[test]
    fn all_ctrader_control_commands_accept_and_require_account_context() {
        init_tracing();
        for tail in [
            vec!["unlock", "AUDUSD"],
            vec!["prep", "AUDUSD", "ready"],
            vec!["veto", "AUDUSD", "news", "--trade-id", "trade-1"],
            vec!["clear-prep", "AUDUSD", "ready"],
            vec!["clear-veto", "AUDUSD", "news", "--trade-id", "trade-1"],
        ] {
            let mut args = vec!["trade-control"];
            args.extend(tail);
            args.extend(["--broker", "ctrader"]);
            assert!(super::super::Cli::try_parse_from(&args).is_err());
            args.extend(["--account", "demo-account"]);
            assert!(super::super::Cli::try_parse_from(args).is_ok());
        }
    }

    #[test]
    fn validated_ctrader_control_routes_to_same_broker_and_account() {
        init_tracing();
        let mut intent =
            trade_control_cli::build_unlock_intent("AUDUSD", chrono::Utc::now(), "test");
        scope_ctrader_control(&mut intent, BrokerKindArg::Ctrader, Some("demo-account")).unwrap();
        assert_eq!(intent.broker, BrokerKind::Ctrader);
        assert_eq!(intent.account.as_deref(), Some("demo-account"));
        assert!(scope_ctrader_control(&mut intent, BrokerKindArg::Ctrader, None).is_err());
    }

    #[test]
    fn legacy_controls_keep_optional_account_and_default_route() {
        init_tracing();
        for args in [
            vec!["trade-control", "prep", "EUR_USD", "ready"],
            vec!["trade-control", "unlock", "EUR_USD"],
            vec![
                "trade-control",
                "clear-prep",
                "EUR_USD",
                "ready",
                "--account",
                "reversals",
            ],
        ] {
            assert!(super::super::Cli::try_parse_from(args).is_ok());
        }
        let mut intent =
            trade_control_cli::build_unlock_intent("EUR_USD", chrono::Utc::now(), "test");
        scope_ctrader_control(&mut intent, BrokerKindArg::TradeNation, None).unwrap();
        assert_eq!(intent.broker, BrokerKind::Oanda);
        assert!(intent.account.is_none());
    }
}
