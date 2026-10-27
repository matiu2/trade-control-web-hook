//! Native cTrader registration: private credentials, optional prop enrollment, then metadata.
use crate::{files, meta_err, paths::Paths, risk};
use color_eyre::eyre::{Result, ensure, eyre};
use trade_control_core::{
    account::{AccountMetadata, MetadataError, MetadataStore},
    intent::BrokerKind,
};
#[path = "ctrader/options.rs"]
mod options;
#[path = "ctrader/profile.rs"]
mod profile;
#[cfg(test)]
#[path = "ctrader/tests.rs"]
mod tests;
#[cfg(test)]
use options::read_secrets;
pub use options::{Options, Secrets};

pub async fn register(
    store: &impl MetadataStore,
    mut meta: AccountMetadata,
    o: &Options,
    r: &risk::Options,
    paths: &Paths,
    read: impl FnOnce() -> Result<Secrets>,
) -> Result<()> {
    ensure!(
        meta.broker == BrokerKind::Ctrader,
        "cTrader options require --broker ctrader"
    );
    validate_name(&meta.name)?;
    match store.get(&meta.name).await {
        Ok(_) => {
            return Err(eyre!(
                "account already exists; use credentials to rotate credentials"
            ));
        }
        Err(MetadataError::NotFound(_)) => {}
        Err(error) => return Err(meta_err(error)),
    };
    let risk_options = risk::complete(r)?;
    let mut batch = lock(paths)?;
    if !o.is_present()
        && !risk_options.is_present()
        && let Some(backend) = profile::existing_fix(&meta.name, paths)?
    {
        backend.validate(meta.kind.is_live(), meta.oanda_account_id.as_deref())?;
        if meta.oanda_account_id.is_none() {
            meta.oanda_account_id = Some(profile::metadata_pin(&backend)?);
        }
        return store.add(meta).await.map_err(meta_err);
    }
    let credentials = profile::prepare(&meta.name, meta.kind.is_live(), o, paths, false, read)?;
    credentials
        .backend
        .validate(meta.kind.is_live(), meta.oanda_account_id.as_deref())?;
    if meta.oanda_account_id.is_none() {
        meta.oanda_account_id = Some(profile::metadata_pin(&credentials.backend)?);
    }
    protect_previous_identity(&credentials, paths)?;
    let enrollment = risk::prepare(
        &meta.name,
        &credentials.api,
        &risk_options,
        paths,
        chrono::Utc::now(),
    )?;
    for (path, bytes) in enrollment.writes.iter().chain(credentials.writes.iter()) {
        batch.write(path, bytes.clone())?;
    }
    let applied = batch.apply()?;
    match store.add(meta).await {
        Ok(()) => {
            drop(applied);
            Ok(())
        }
        Err(error) => {
            if enrollment.profile.is_some() {
                return Err(meta_err(error).wrap_err("prop registration files/state retained to protect against an ambiguous database insert; inspect status, then retry add without risk flags; never reenroll/reset state"));
            }
            let restored = applied.rollback();
            Err(match restored {
                Ok(()) => meta_err(error),
                Err(rollback) => eyre!(
                    "{}; account file rollback failed: {rollback:#}",
                    meta_err(error)
                ),
            })
        }
    }
}
pub async fn credentials(
    store: &impl MetadataStore,
    name: &str,
    o: &Options,
    paths: &Paths,
    read: impl FnOnce() -> Result<Secrets>,
) -> Result<()> {
    validate_name(name)?;
    let meta = store.get(name).await.map_err(meta_err)?;
    ensure!(
        meta.broker == BrokerKind::Ctrader,
        "cTrader credential options require a cTrader account"
    );
    let mut batch = lock(paths)?;
    let credentials = profile::prepare(name, meta.kind.is_live(), o, paths, true, read)?;
    credentials
        .backend
        .validate(meta.kind.is_live(), meta.oanda_account_id.as_deref())?;
    protect_previous_identity(&credentials, paths)?;
    let _risk = risk::prepare(
        name,
        &credentials.api,
        &risk::Options::default(),
        paths,
        chrono::Utc::now(),
    )?;
    for (path, bytes) in credentials.writes {
        batch.write(&path, bytes)?;
    }
    batch.apply()?;
    Ok(())
}
fn lock(p: &Paths) -> Result<files::Batch> {
    files::Batch::lock_paths(&[&p.oauth, &p.quote, &p.trade, &p.companion, &p.risk])
}
fn protect_previous_identity(prepared: &profile::Prepared, paths: &Paths) -> Result<()> {
    if let Some(old) = &prepared.previous
        && (old.account_id != prepared.api.account_id
            || old.environment != prepared.api.environment)
    {
        let binding = paths
            .registry
            .join(format!("{:?}-{}.json", old.environment, old.account_id));
        let unprotected = match std::fs::symlink_metadata(binding) {
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(error.into()),
        };
        ensure!(
            unprotected,
            "protected cTrader identity cannot change through credential rotation"
        );
    }
    Ok(())
}
pub fn risk_status(name: &str, paths: &Paths) -> Result<()> {
    validate_name(name)?;
    let profile = risk::display_profile(paths, name)?
        .ok_or_else(|| eyre!("no prop risk profile for this account"))?;
    let state = adaptive_risk::store::FileStore::new(&profile.state_directory)
        .lock()?
        .load()?;
    ensure!(
        state.account_id == profile.identity() && state.policy == profile.risk,
        "enrolled risk policy mismatch"
    );
    let status = serde_json::json!({"profile":profile,"state":{"consecutive_losses":state.consecutive_losses,"daily_locked":state.daily_locked,"account_locked":state.account_locked,"day_reference":state.day,"reservations":state.reservations.len(),"last_reconciled_at":state.last_reconciled_at,"policy_version":state.policy.version}});
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        "account name must contain 1..128 ASCII letters, digits, dashes or underscores"
    );
    Ok(())
}
