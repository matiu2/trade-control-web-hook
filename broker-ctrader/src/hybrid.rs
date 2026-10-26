//! Bind FIX management to Open API risk, protection and history for the same account.
use super::*;
mod exact_close;
mod writes;

pub(crate) struct Hybrid {
    pub data: OpenApiBroker,
    pub management: fix::FixBroker,
}

impl Hybrid {
    pub async fn connect(
        name: &str,
        open_api: Config,
        quote: ctrader_fix::Config,
        trade: ctrader_fix::TradeConfig,
        binding: access::CompanionBinding,
    ) -> Result<Self> {
        access::Profile::Hybrid {
            open_api: open_api.clone(),
            quote: quote.clone(),
            trade: trade.clone(),
            binding: binding.clone(),
        }
        .validate(false, None)?;
        let login = quote.username()?.to_owned();
        let management = fix::FixBroker::connect(name, quote, trade).await?;
        let data = OpenApiBroker::connect_config(name, open_api).await?;
        let trader = metadata::trader(&data).await?;
        verify_identity(&trader, &login, &binding)?;
        Ok(Self { data, management })
    }
}

fn verify_identity(trader: &Value, login: &str, binding: &access::CompanionBinding) -> Result<()> {
    ensure!(
        wire::integer(trader, "ctidTraderAccountId")? == binding.open_api_account_id,
        "Open API companion returned a different bound account"
    );
    let actual_login = wire::integer(trader, "traderLogin")?;
    ensure!(
        login.parse::<i64>().ok() == Some(actual_login),
        "Open API companion terminal login differs from the FIX account"
    );
    ensure!(
        trader.get("brokerName").and_then(Value::as_str) == Some(binding.broker_name.as_str()),
        "Open API companion broker identity differs from the explicit FIX binding"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn init() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    }
    fn binding() -> access::CompanionBinding {
        access::CompanionBinding {
            open_api_account_id: 789,
            fix_sender_comp_id: "demo.example.123".into(),
            broker_name: "Example Broker".into(),
        }
    }
    #[test]
    fn companion_matches_both_identifier_kinds_and_explicit_broker_pin() {
        init();
        let trader =
            json!({"ctidTraderAccountId":789,"traderLogin":123,"brokerName":"Example Broker"});
        assert!(verify_identity(&trader, "123", &binding()).is_ok());
        for (field, value) in [
            ("ctidTraderAccountId", json!(999)),
            ("traderLogin", json!(999)),
            ("brokerName", json!("Other Broker")),
        ] {
            let mut wrong = trader.clone();
            wrong[field] = value;
            assert!(verify_identity(&wrong, "123", &binding()).is_err());
        }
    }
    #[test]
    fn missing_optional_broker_identity_cannot_establish_a_companion_binding() {
        init();
        assert!(verify_identity(&json!({"ctidTraderAccountId":789}), "123", &binding()).is_err());
        assert!(
            verify_identity(
                &json!({"ctidTraderAccountId":789,"traderLogin":123}),
                "123",
                &binding()
            )
            .is_err()
        );
    }
}
