//! Authenticate catalog reads with the named account's stored platform type.

use color_eyre::Result;
use tradenation_api::accounts::{Account, AccountKind, get_account};
use tradenation_api::{Session, login, login_demo_with_creds};

pub(super) async fn login_named_account(name: &str) -> Result<Session> {
    let account = get_account(name)?;
    login_account_with(&account, login_demo_with_creds, login).await
}

async fn login_account_with(
    account: &Account,
    demo: impl AsyncFnOnce(&str, &str) -> Result<Session>,
    live: impl AsyncFnOnce(&str, &str) -> Result<Session>,
) -> Result<Session> {
    match account.kind {
        AccountKind::Demo => demo(&account.username, &account.password).await,
        AccountKind::Live => live(&account.username, &account.password).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_tracing() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    }

    fn account(kind: AccountKind) -> Account {
        Account {
            username: "stored-user".into(),
            password: "stored-password".into(),
            kind,
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn live_catalog_login_uses_live_credentials() {
        init_tracing();
        login_account_with(
            &account(AccountKind::Live),
            async |_, _| panic!("live catalog must not use demo login"),
            async |username, password| {
                assert_eq!(username, "stored-user");
                assert_eq!(password, "stored-password");
                Ok(Session::live("jwt", "asp", "ots-name", "ots-value"))
            },
        )
        .await
        .expect("live account login should succeed");
    }

    #[tokio::test]
    async fn demo_catalog_login_uses_demo_credentials() {
        init_tracing();
        login_account_with(
            &account(AccountKind::Demo),
            async |username, password| {
                assert_eq!(username, "stored-user");
                assert_eq!(password, "stored-password");
                Ok(Session::demo(username, password, "asp", None))
            },
            async |_, _| panic!("demo catalog must not use live login"),
        )
        .await
        .expect("demo account login should succeed");
    }

    #[tokio::test]
    async fn live_login_error_is_returned_without_demo_fallback() {
        init_tracing();
        let error = login_account_with(
            &account(AccountKind::Live),
            async |_, _| panic!("failed live login must not fall back to demo"),
            async |_, _| Err(color_eyre::eyre::eyre!("live authentication failed")),
        )
        .await
        .expect_err("login failure must be returned");
        assert_eq!(error.to_string(), "live authentication failed");
    }
}
