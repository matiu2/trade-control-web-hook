//! Register-time guard: a plan's rules must route to the broker its account
//! belongs to.
//!
//! The engine fetches a plan's candles (and dispatches its intents) through the
//! **account's** broker, not the `broker` each rule's intent names. A plan armed
//! `broker: oanda` on a TradeNation account therefore asks TradeNation for an
//! OANDA symbol (`AUD_USD`) on every tick, fails, never seeds its state, and
//! never fires a single rule — not even its time-based `trade-expiry`. The
//! 2026-08-18 `hs-aud-usd-d6ca1271` / `hs-btc-usd-755e77b3` plans sat like that
//! for six weeks behind a `200 ok` register. This check turns that into a loud
//! rejection at arm time.

use std::fmt;

use crate::intent::BrokerKind;

use super::TradePlan;

/// One rule whose intent names a different broker than the plan's account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleBrokerMismatch {
    pub rule_id: String,
    pub broker: BrokerKind,
}

/// A plan whose rules disagree with the broker of the account it is registered
/// against. `Display` is the operator-facing rejection message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanBrokerMismatch {
    pub account: String,
    pub account_broker: BrokerKind,
    pub rules: Vec<RuleBrokerMismatch>,
}

impl fmt::Display for PlanBrokerMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let account = &self.account;
        let rules = self
            .rules
            .iter()
            .map(|r| format!("{} ({:?})", r.rule_id, r.broker))
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "register: account '{account}' is a {:?} account but these rules route to \
             another broker: {rules} — re-arm with the account's broker",
            self.account_broker
        )
    }
}

impl std::error::Error for PlanBrokerMismatch {}

/// Check every rule of `plan` routes to `account_broker`. A plan with no rules
/// trivially passes.
pub fn check_plan_broker(
    plan: &TradePlan,
    account: &str,
    account_broker: BrokerKind,
) -> Result<(), PlanBrokerMismatch> {
    let rules: Vec<RuleBrokerMismatch> = plan
        .rules
        .iter()
        .filter(|rule| rule.intent.broker != account_broker)
        .map(|rule| RuleBrokerMismatch {
            rule_id: rule.rule_id.clone(),
            broker: rule.intent.broker,
        })
        .collect();
    if rules.is_empty() {
        return Ok(());
    }
    Err(PlanBrokerMismatch {
        account: account.to_owned(),
        account_broker,
        rules,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plan whose two rules route to `veto_broker` and `time_broker`.
    fn plan_with(veto_broker: &str, time_broker: &str) -> TradePlan {
        let json = format!(
            r#"{{"trade_id":"hs-aud-usd-d6ca1271","instrument":"AUD_USD","direction":"short",
                "granularity":"h1","pip_size":0.0001,"rules":[
                {{"rule_id":"01-veto-too-high","fire_mode":"once","kind":"setup_invalidation",
                  "trigger":{{"type":"horizontal_cross","level":0.7113,"dir":"up","bar":"on_close"}},
                  "intent":{{"v":1,"id":"hs-aud-usd-d6ca1271-too-high","action":"veto",
                    "instrument":"AUD_USD","broker":"{veto_broker}","account":"experimental",
                    "name":"too-high","ttl_hours":42,"level":"close-positions",
                    "trade_id":"hs-aud-usd-d6ca1271","not_after":"2026-08-20T02:30:00Z"}}}},
                {{"rule_id":"02-veto-trade-expiry","fire_mode":"once","kind":"setup_invalidation",
                  "trigger":{{"type":"time_reached","at_epoch":1787191200}},
                  "intent":{{"v":1,"id":"hs-aud-usd-d6ca1271-trade-expiry","action":"veto",
                    "instrument":"AUD_USD","broker":"{time_broker}","account":"experimental",
                    "name":"trade-expiry","ttl_hours":42,"level":"close-positions",
                    "trade_id":"hs-aud-usd-d6ca1271","not_after":"2026-08-20T02:30:00Z"}}}}
                ]}}"#
        );
        serde_json::from_str(&json).expect("plan json")
    }

    #[test]
    fn matching_broker_passes() {
        let plan = plan_with("tradenation", "tradenation");
        assert_eq!(
            check_plan_broker(&plan, "experimental", BrokerKind::TradeNation),
            Ok(())
        );
    }

    /// The 2026-08-18 regression: an OANDA-armed plan on a TradeNation account.
    #[test]
    fn oanda_plan_on_tradenation_account_is_rejected() {
        let plan = plan_with("oanda", "oanda");
        let err = check_plan_broker(&plan, "experimental", BrokerKind::TradeNation)
            .expect_err("mismatch must be rejected");
        assert_eq!(err.account_broker, BrokerKind::TradeNation);
        assert_eq!(
            err.rules,
            vec![
                RuleBrokerMismatch {
                    rule_id: "01-veto-too-high".into(),
                    broker: BrokerKind::Oanda,
                },
                RuleBrokerMismatch {
                    rule_id: "02-veto-trade-expiry".into(),
                    broker: BrokerKind::Oanda,
                },
            ]
        );
    }

    /// A single stray rule is enough to reject, and only it is named.
    #[test]
    fn one_mismatched_rule_is_named_alone() {
        let plan = plan_with("tradenation", "oanda");
        let err = check_plan_broker(&plan, "experimental", BrokerKind::TradeNation)
            .expect_err("mismatch must be rejected");
        assert_eq!(err.rules.len(), 1);
        assert_eq!(err.rules[0].rule_id, "02-veto-trade-expiry");
    }

    #[test]
    fn message_names_account_and_rules() {
        let plan = plan_with("oanda", "tradenation");
        let msg = check_plan_broker(&plan, "experimental", BrokerKind::TradeNation)
            .expect_err("mismatch")
            .to_string();
        assert!(msg.contains("'experimental'"), "{msg}");
        assert!(msg.contains("TradeNation account"), "{msg}");
        assert!(msg.contains("01-veto-too-high (Oanda)"), "{msg}");
    }

    #[test]
    fn empty_plan_passes() {
        let plan: TradePlan = serde_json::from_str(
            r#"{"trade_id":"t","instrument":"AUD_USD","direction":"short",
                "granularity":"h1","pip_size":0.0001,"rules":[]}"#,
        )
        .expect("plan json");
        assert_eq!(check_plan_broker(&plan, "a", BrokerKind::Oanda), Ok(()));
    }
}
