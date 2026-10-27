//! Verified broker account identity for read-only calibration evidence.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountIdentity {
    pub account_id: i64,
    pub environment: ctrader_api::Environment,
    pub endpoint: String,
    pub broker_name: String,
    pub trader_login: i64,
}

pub(super) fn decode(
    trader: &Value,
    account_id: i64,
    environment: ctrader_api::Environment,
) -> Result<AccountIdentity> {
    ensure!(
        wire::integer(trader, "ctidTraderAccountId")? == account_id,
        "calibration trader account identity mismatch"
    );
    let trader_login = wire::integer(trader, "traderLogin")?;
    ensure!(trader_login > 0, "invalid calibration terminal login");
    let broker_name = trader["brokerName"]
        .as_str()
        .filter(|name| {
            !name.trim().is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
        })
        .ok_or_else(|| eyre!("calibration requires an explicit broker identity"))?;
    Ok(AccountIdentity {
        account_id,
        environment,
        endpoint: environment.endpoint().into(),
        broker_name: broker_name.into(),
        trader_login,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_identity_requires_both_account_identifiers_and_broker() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
        let trader = json!({"ctidTraderAccountId":42,"traderLogin":123,"brokerName":"Example"});
        let actual = decode(&trader, 42, ctrader_api::Environment::Demo).unwrap();
        assert_eq!(actual.trader_login, 123);
        assert_eq!(actual.endpoint, ctrader_api::Environment::Demo.endpoint());
        assert!(decode(&trader, 99, ctrader_api::Environment::Demo).is_err());
        for key in ["traderLogin", "brokerName"] {
            let mut missing = trader.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(decode(&missing, 42, ctrader_api::Environment::Demo).is_err());
        }
        for value in [json!(""), json!("Example\nBroker")] {
            let mut invalid = trader.clone();
            invalid["brokerName"] = value;
            assert!(decode(&invalid, 42, ctrader_api::Environment::Demo).is_err());
        }
    }
}
