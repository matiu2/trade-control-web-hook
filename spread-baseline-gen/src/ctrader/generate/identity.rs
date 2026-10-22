use crate::ctrader::Identity;
use broker_ctrader::{access::Profile, market_data::ChartData};
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};
pub fn demo_config(profile: &Profile) -> Result<&ctrader_api::Config> {
    let config = match profile {
        Profile::OpenApi(config)
        | Profile::Hybrid {
            open_api: config, ..
        } => config,
        _ => {
            return Err(eyre!(
                "spread calibration requires genuine paired Open API history"
            ));
        }
    };
    ensure!(
        config.environment == ctrader_api::Environment::Demo,
        "only the cTrader demo account is authorized for calibration"
    );
    config.validate()?;
    Ok(config)
}

pub async fn identity(account: &str, data: &ChartData) -> Result<Identity> {
    let verified = data.account_identity().await?;
    let identity = Identity {
        profile: account.into(),
        environment: "demo".into(),
        endpoint: verified.endpoint,
        account_id: verified.account_id,
        broker: verified.broker_name,
        login: verified.trader_login,
    };
    ensure!(
        verified.environment == ctrader_api::Environment::Demo,
        "verified account is live"
    );
    identity.validate_demo()?;
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_profile_is_rejected_before_any_connection() {
        crate::ctrader::test_logging();
        let mut config = ctrader_api::Config {
            client_id: "client".into(),
            client_secret: "private".into(),
            access_token: "private".into(),
            account_id: 42,
            environment: ctrader_api::Environment::Demo,
        };
        assert!(demo_config(&Profile::OpenApi(config.clone())).is_ok());
        config.environment = ctrader_api::Environment::Live;
        assert!(demo_config(&Profile::OpenApi(config)).is_err());
    }
}
