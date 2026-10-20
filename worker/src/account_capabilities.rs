//! Account-list capabilities are resolved locally and contain no credentials.
use broker_ctrader::access::{Capabilities, Profile};
use serde::Serialize;
use trade_control_core::{account::AccountMetadata, intent::BrokerKind};

#[derive(Serialize)]
pub struct AccountListing<'a> {
    #[serde(flatten)]
    pub metadata: &'a AccountMetadata,
    pub capabilities: Capabilities,
}

pub fn capabilities(meta: &AccountMetadata) -> Capabilities {
    if meta.broker != BrokerKind::Ctrader {
        return Capabilities {
            quotes: true,
            execution: true,
            history: true,
            readiness: "existing-broker".into(),
            reason: "Existing broker integration; connectivity not checked".into(),
        };
    }
    match Profile::load(&meta.name).and_then(|profile| {
        profile.validate(meta.kind.is_live(), meta.oanda_account_id.as_deref())?;
        Ok(profile.capabilities())
    }) {
        Ok(capabilities) => capabilities,
        Err(error) => Capabilities::unavailable(error.to_string()),
    }
}

pub fn listing(meta: &AccountMetadata) -> AccountListing<'_> {
    AccountListing {
        metadata: meta,
        capabilities: capabilities(meta),
    }
}
