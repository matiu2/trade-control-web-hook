//! Keep metadata failures from leaving half-finished local MT5 updates.
use super::{meta_err, mt5};
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};
use std::path::Path;
use trade_control_core::{
    account::{AccountMetadata, MetadataError, MetadataStore},
    intent::BrokerKind,
};

pub async fn add(
    store: &impl MetadataStore,
    meta: AccountMetadata,
    options: &mt5::Options,
    path: Option<&Path>,
    password: impl FnOnce() -> Result<String>,
) -> Result<()> {
    match store.get(&meta.name).await {
        Ok(_) => {
            return Err(eyre!(
                "account '{}' already exists — use credentials to update an MT5 login",
                meta.name
            ));
        }
        Err(MetadataError::NotFound(_)) => {}
        Err(error) => return Err(meta_err(error)),
    }
    let applied = if meta.broker == BrokerKind::Mt5 {
        validate_mt5(&meta)?;
        let path = path.ok_or_else(|| eyre!("MT5 profile configuration path is required"))?;
        let update = mt5::Update::prepare(path, &meta.name, options)?;
        validate_pin(&meta, update.login())?;
        tracing::info!(account = %meta.name, credentials_file = %update.credentials_path().display(), "saving MT5 profile and credentials");
        Some(update.apply(&password()?)?)
    } else {
        None
    };
    match store.add(meta).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let error = meta_err(error);
            Err(match applied {
                Some(files) => mt5::rollback_error(error, files.rollback()),
                None => error,
            })
        }
    }
}

pub async fn credentials(
    store: &impl MetadataStore,
    name: &str,
    options: &mt5::Options,
    path: &Path,
    password: impl FnOnce() -> Result<String>,
) -> Result<()> {
    let meta = store.get(name).await.map_err(meta_err)?;
    validate_mt5(&meta)?;
    let update = mt5::Update::prepare(path, name, options)?;
    validate_pin(&meta, update.login())?;
    tracing::info!(account = name, credentials_file = %update.credentials_path().display(), "updating MT5 profile and credentials");
    update.apply(&password()?)?;
    Ok(())
}

fn validate_mt5(meta: &AccountMetadata) -> Result<()> {
    ensure!(
        meta.broker == BrokerKind::Mt5,
        "credentials currently supports MT5 accounts only"
    );
    ensure!(
        !meta.kind.is_live(),
        "MT5 currently supports demo/competition only"
    );
    Ok(())
}

fn validate_pin(meta: &AccountMetadata, login: u64) -> Result<()> {
    if let Some(id) = &meta.oanda_account_id {
        ensure!(
            id.parse::<u64>().ok() == Some(login),
            "MT5 metadata has a different explicit account-id pin; correct that metadata before changing login"
        );
    }
    Ok(())
}
