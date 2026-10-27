//! Resolve the named credentials without crossing demo/live account kinds.

use trade_control_core::account::{AccountKind, AccountMetadata};
use tradenation_api::accounts::AccountKind as StoredKind;

use super::BrokerError;

pub(super) async fn session(
    meta: &AccountMetadata,
) -> Result<tradenation_api::Session, BrokerError> {
    let credentials = tradenation_api::get_account(&meta.name)
        .map_err(|error| BrokerError::TradeNationLogin(error.to_string()))?;
    validate_kind(meta.kind, credentials.kind)?;
    let session = match credentials.kind {
        StoredKind::Demo => {
            tradenation_api::login_demo_with_creds(&credentials.username, &credentials.password)
                .await
        }
        StoredKind::Live => {
            tradenation_api::login(&credentials.username, &credentials.password).await
        }
    };
    session.map_err(|error| BrokerError::TradeNationLogin(error.to_string()))
}

fn validate_kind(metadata: AccountKind, stored: StoredKind) -> Result<(), BrokerError> {
    if metadata.is_live() != matches!(stored, StoredKind::Live) {
        return Err(BrokerError::TradeNationLogin(
            "Registered TradeNation account kind does not match its stored credentials".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_and_credentials_must_agree_before_authentication() {
        assert!(validate_kind(AccountKind::Live, StoredKind::Live).is_ok());
        assert!(validate_kind(AccountKind::Demo, StoredKind::Demo).is_ok());
        assert!(validate_kind(AccountKind::Demo, StoredKind::Live).is_err());
        assert!(validate_kind(AccountKind::Live, StoredKind::Demo).is_err());
    }
}
