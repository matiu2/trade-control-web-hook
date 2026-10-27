use crate::{files, paths::Paths};
use adaptive_risk::{
    DayReference, RiskState,
    store::{FileStore, LockedStore},
};
use broker_ctrader::protection::{FeeMode, PropAccount, registration};
use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, ensure, eyre};
use std::path::Path;
#[path = "risk/options.rs"]
mod options;
#[path = "risk/policy.rs"]
mod policy;
#[path = "risk/prompt.rs"]
mod prompt;
pub use options::Options;

pub struct Prepared {
    pub profile: Option<PropAccount>,
    pub writes: Vec<(std::path::PathBuf, Vec<u8>)>,
    pub _state_gate: Option<LockedStore>,
}
#[derive(serde::Deserialize, Default)]
struct Profiles {
    #[serde(default)]
    accounts: std::collections::BTreeMap<String, PropAccount>,
}
pub fn load(path: &Path, name: &str) -> Result<Option<PropAccount>> {
    let doc = files::read_document(path, false)?;
    let mut profiles: Profiles =
        toml::from_str(&doc.to_string()).map_err(|_| eyre!("invalid prop risk profiles"))?;
    Ok(profiles.accounts.remove(name))
}
pub fn prepare(
    name: &str,
    api: &ctrader_api::Config,
    o: &Options,
    paths: &Paths,
    now: DateTime<Utc>,
) -> Result<Prepared> {
    ensure!(
        !o.is_present() || o.prop_firm,
        "risk settings require --prop-firm"
    );
    let mut existing = load(&paths.risk, name)?;
    if existing.as_ref().is_none_or(|p| !p.enabled) {
        let doc = files::read_document(&paths.risk, false)?;
        let all: Profiles =
            toml::from_str(&doc.to_string()).map_err(|_| eyre!("invalid prop risk profiles"))?;
        existing = all
            .accounts
            .into_values()
            .find(|p| {
                p.enabled && p.account_id == api.account_id && p.environment == api.environment
            })
            .or(existing);
    }
    if let Some(p) = existing.filter(|p| p.enabled) {
        ensure!(
            !o.is_present(),
            "risk state already enrolled; omit risk options to reuse it (policy changes require migration)"
        );
        ensure!(
            p.account_id == api.account_id && p.environment == api.environment,
            "cannot change a protected account's physical identity through credentials"
        );
        p.validate()?;
        let (binding_path, _) = registration::activation_file(&p, &paths.registry)?;
        let binding = std::fs::read(binding_path)
            .map_err(|_| eyre!("protected account activation record missing"))?;
        registration::validate_activation(&p, &binding)?;
        let locked = FileStore::new(&p.state_directory).lock()?;
        let state = locked.load()?;
        ensure!(
            state.account_id == p.identity() && state.policy == p.risk,
            "enrolled risk policy mismatch"
        );
        return Ok(Prepared {
            profile: Some(p),
            writes: vec![],
            _state_gate: Some(locked),
        });
    }
    let (activation_path, existing_activation) = activation_for(paths, api)?;
    ensure!(
        !existing_activation,
        "this physical account is protected but its enabled profile is missing; reconcile its registration"
    );
    if !o.prop_firm {
        return Ok(Prepared {
            profile: None,
            writes: vec![],
            _state_gate: None,
        });
    }
    ensure!(
        o.rules_verified && o.fees_verified,
        "verify program rules and broker fee conventions before passing --rules-verified --fees-verified"
    );
    let initial = o
        .initial_balance
        .ok_or_else(|| eyre!("--initial-balance required for prop enrollment"))?;
    let history = o.history_from.unwrap_or(now);
    ensure!(history <= now, "risk history cannot start in the future");
    let risk = policy::policy(o)?;
    let day = DayReference {
        day: risk.calendar.day_at(history)?,
        balance: o
            .day_start_balance
            .or(o.new_flat_account.then_some(initial))
            .ok_or_else(|| eyre!("--day-start-balance or --new-flat-account required"))?,
        equity: o
            .day_start_equity
            .or(o.new_flat_account.then_some(initial))
            .ok_or_else(|| eyre!("--day-start-equity or --new-flat-account required"))?,
    };
    let state_directory = o
        .risk_state_directory
        .clone()
        .unwrap_or_else(|| paths.state_root.join(name));
    let p = PropAccount {
        enabled: true,
        account_id: api.account_id,
        environment: api.environment,
        state_directory,
        history_from: history,
        initial_balance: initial,
        rulebook_verified: true,
        fees_verified: true,
        fee_mode: if o.close_detail_includes_opening {
            FeeMode::CloseDetailIncludesOpening
        } else {
            FeeMode::PerDeal
        },
        commission_is_positive_cost: !o.commission_is_signed,
        risk,
        costs: policy::costs(o)?,
        firm_evidence_file: o.firm_evidence_file.clone(),
    };
    p.validate()?;
    ensure!(
        p.firm_evidence_file
            .as_ref()
            .is_none_or(|path| path.is_absolute()),
        "firm evidence file must be absolute"
    );
    ensure!(
        (matches!(
            p.risk.daily_reference,
            adaptive_risk::DailyReference::Balance
        ) && !matches!(
            p.risk.drawdown_reference,
            adaptive_risk::DrawdownReference::TrailingEquity
        )) || p.firm_evidence_file.is_some(),
        "this reference policy requires --firm-evidence-file from an authoritative source"
    );
    let state = RiskState::enroll(p.identity(), initial, day, p.risk.clone())?;
    let (path, binding) = registration::activation_file(&p, &paths.registry)?;
    ensure!(path == activation_path, "risk registry identity mismatch");
    let mut document = files::read_document(&paths.risk, false)?;
    let all: Profiles = toml::from_str(&document.to_string())
        .map_err(|_| eyre!("invalid risk profile configuration"))?;
    ensure!(
        !all.accounts.values().any(|other| other.enabled
            && (other.account_id == p.account_id && other.environment == p.environment
                || other.state_directory == p.state_directory)),
        "duplicate protected account or state directory"
    );
    let table = toml::to_string(&p)?.parse::<toml_edit::DocumentMut>()?;
    document["accounts"][name] = toml_edit::Item::Table(table.as_table().clone());
    std::fs::create_dir_all(&p.state_directory)?;
    let locked = FileStore::new(&p.state_directory).lock()?;
    ensure!(
        !p.state_directory.join("state.json").exists(),
        "risk state already exists; never reenroll or reset it"
    );
    let writes = vec![
        (path, binding),
        (
            p.state_directory.join("state.json"),
            serde_json::to_vec(&state)?,
        ),
        (paths.risk.clone(), document.to_string().into_bytes()),
    ];
    Ok(Prepared {
        profile: Some(p),
        writes,
        _state_gate: Some(locked),
    })
}
fn activation_for(paths: &Paths, api: &ctrader_api::Config) -> Result<(std::path::PathBuf, bool)> {
    let path = paths
        .registry
        .join(format!("{:?}-{}.json", api.environment, api.account_id));
    let exists = match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            ensure!(meta.is_file(), "risk activation must be a regular file");
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    Ok((path, exists))
}
pub fn describe(paths: &Paths, name: &str) -> Result<String> {
    Ok(match display_profile(paths, name)? {
        Some(p) if p.enabled => format!(
            "  prop-risk=enabled base={} daily={}/{} overall={}/{} reset={} {}",
            p.risk.base_risk_pct,
            p.risk.internal_daily_stop_pct,
            p.risk.daily_loss_limit_pct,
            p.risk.internal_account_stop_pct,
            p.risk.max_account_loss_pct,
            p.risk.calendar.reset_time,
            p.risk.calendar.timezone
        ),
        _ => String::new(),
    })
}

pub fn complete(o: &Options) -> Result<Options> {
    let o = prompt::complete(o)?;
    ensure!(
        !o.is_present() || o.prop_firm,
        "backoff options require --prop-firm"
    );
    if o.prop_firm {
        ensure!(
            o.rules_verified && o.fees_verified,
            "verify rules and fees before enrollment"
        );
        policy::policy(&o)?;
        policy::costs(&o)?;
        ensure!(o.initial_balance.is_some(), "--initial-balance required");
    }
    Ok(o)
}

pub fn display_profile(paths: &Paths, name: &str) -> Result<Option<PropAccount>> {
    let named = load(&paths.risk, name)?;
    if named.as_ref().is_some_and(|p| p.enabled) {
        return Ok(named);
    }
    let oauth = files::read_document(&paths.oauth, true)?;
    let Some(id) = oauth
        .get(name)
        .and_then(|p| p.get("account_id"))
        .and_then(toml_edit::Item::as_integer)
    else {
        return Ok(named);
    };
    let environment = oauth
        .get(name)
        .and_then(|p| p.get("environment"))
        .and_then(toml_edit::Item::as_str);
    let doc = files::read_document(&paths.risk, false)?;
    let profiles: Profiles =
        toml::from_str(&doc.to_string()).map_err(|_| eyre!("invalid risk profiles"))?;
    Ok(profiles
        .accounts
        .into_values()
        .find(|p| {
            p.enabled
                && p.account_id == id
                && Some(if matches!(p.environment, ctrader_api::Environment::Live) {
                    "live"
                } else {
                    "demo"
                }) == environment
        })
        .or(named))
}
