use super::*;
use std::collections::BTreeMap;
use toml_edit::{DocumentMut, value};
pub(super) struct Prepared {
    pub api: ctrader_api::Config,
    pub backend: broker_ctrader::access::Profile,
    pub writes: Vec<(std::path::PathBuf, Vec<u8>)>,
    pub previous: Option<ctrader_api::Config>,
}
pub(super) fn prepare(
    name: &str,
    live: bool,
    o: &Options,
    paths: &Paths,
    rotate: bool,
    read: impl FnOnce() -> Result<Secrets>,
) -> Result<Prepared> {
    let mut oauth = files::read_document(&paths.oauth, true)?;
    let profiles: BTreeMap<String, ctrader_api::Config> = toml::from_str(&oauth.to_string())
        .map_err(|_| eyre!("invalid cTrader profiles; credential values withheld"))?;
    let previous = profiles.get(name).cloned();
    let donor = o
        .reuse
        .as_ref()
        .map(|name| {
            profiles
                .get(name)
                .cloned()
                .ok_or_else(|| eyre!("unknown cTrader OAuth source profile"))
        })
        .transpose()?;
    ensure!(
        !(o.reuse.is_some() && o.secrets_stdin),
        "choose credential reuse or stdin, not both"
    );
    let account_id = options::account_id(o.account_id.or(previous.as_ref().map(|p| p.account_id)))?;
    let mut bundle = if o.secrets_stdin
        || o.fix
        || (rotate && o.reuse.is_none())
        || previous.is_none() && donor.is_none()
    {
        Some(read()?)
    } else {
        None
    };
    let app = donor.as_ref().or(previous.as_ref());
    let selected_bundle = bundle.as_ref().filter(|_| o.reuse.is_none());
    let (client_id, client_secret, access_token) = if let Some(s) = selected_bundle {
        (
            s.client_id.clone(),
            s.client_secret.clone(),
            s.access_token.clone(),
        )
    } else {
        let p = app.ok_or_else(|| eyre!("cTrader OAuth credentials missing"))?;
        (
            p.client_id.clone(),
            p.client_secret.clone(),
            p.access_token.clone(),
        )
    };
    let api = ctrader_api::Config {
        client_id,
        client_secret,
        access_token,
        account_id,
        environment: if live {
            ctrader_api::Environment::Live
        } else {
            ctrader_api::Environment::Demo
        },
    };
    api.validate()?;
    for (field, text) in [
        ("client_id", &api.client_id),
        ("client_secret", &api.client_secret),
        ("access_token", &api.access_token),
    ] {
        oauth[name][field] = value(text.clone());
    }
    oauth[name]["account_id"] = value(account_id);
    oauth[name]["environment"] = value(if live { "live" } else { "demo" });
    let mut writes = vec![(paths.oauth.clone(), oauth.to_string().into_bytes())];
    let new_fix = bundle.as_mut().and_then(|b| b.fix.take());
    ensure!(
        !o.fix || new_fix.is_some(),
        "--ctrader-fix requires complete QUOTE/TRADE credentials and broker binding"
    );
    let backend = if let Some(fix) = new_fix {
        let binding = broker_ctrader::access::CompanionBinding {
            open_api_account_id: account_id,
            fix_sender_comp_id: fix.quote.sender_comp_id.clone(),
            broker_name: fix.broker_name,
        };
        let backend = broker_ctrader::access::Profile::Hybrid {
            open_api: api.clone(),
            quote: fix.quote.clone(),
            trade: fix.trade.clone(),
            binding: binding.clone(),
        };
        backend.validate(live, None)?;
        let mut quote = files::read_document(&paths.quote, true)?;
        let mut trade = files::read_document(&paths.trade, true)?;
        let mut bindings = files::read_document(&paths.companion, true)?;
        write_fix(
            &mut quote,
            name,
            &FixFields {
                host: &fix.quote.host,
                port: fix.quote.port,
                sender: &fix.quote.sender_comp_id,
                target: &fix.quote.target_comp_id,
                session: "QUOTE",
                password: &fix.quote.password,
            },
        );
        write_fix(
            &mut trade,
            name,
            &FixFields {
                host: &fix.trade.host,
                port: fix.trade.port,
                sender: &fix.trade.sender_comp_id,
                target: &fix.trade.target_comp_id,
                session: "TRADE",
                password: &fix.trade.password,
            },
        );
        bindings[name]["open_api_account_id"] = value(account_id);
        bindings[name]["fix_sender_comp_id"] = value(binding.fix_sender_comp_id);
        bindings[name]["broker_name"] = value(binding.broker_name);
        writes.extend([
            (paths.quote.clone(), quote.to_string().into_bytes()),
            (paths.trade.clone(), trade.to_string().into_bytes()),
            (paths.companion.clone(), bindings.to_string().into_bytes()),
        ]);
        backend
    } else {
        existing_backend(name, &api, paths)?
    };
    Ok(Prepared {
        api,
        backend,
        writes,
        previous,
    })
}
fn write_fix(doc: &mut DocumentMut, name: &str, config: &FixFields<'_>) {
    doc[name]["host"] = value(config.host);
    doc[name]["port"] = value(i64::from(config.port));
    doc[name]["sender_comp_id"] = value(config.sender);
    doc[name]["target_comp_id"] = value(config.target);
    doc[name]["sender_sub_id"] = value(config.session);
    doc[name]["password"] = value(config.password);
}
struct FixFields<'a> {
    host: &'a str,
    port: u16,
    sender: &'a str,
    target: &'a str,
    session: &'a str,
    password: &'a str,
}
fn existing_backend(
    name: &str,
    api: &ctrader_api::Config,
    paths: &Paths,
) -> Result<broker_ctrader::access::Profile> {
    let doc = files::read_document(&paths.companion, true)?;
    let mut bindings: BTreeMap<String, broker_ctrader::access::CompanionBinding> =
        toml::from_str(&doc.to_string())
            .map_err(|_| eyre!("invalid cTrader companion bindings"))?;
    if let Some(binding) = bindings.remove(name) {
        let parse_quote = files::read_document(&paths.quote, true)?;
        let parse_trade = files::read_document(&paths.trade, true)?;
        let mut quotes: BTreeMap<String, ctrader_fix::Config> =
            toml::from_str(&parse_quote.to_string())
                .map_err(|_| eyre!("invalid private FIX QUOTE profiles"))?;
        let mut trades: BTreeMap<String, ctrader_fix::TradeConfig> =
            toml::from_str(&parse_trade.to_string())
                .map_err(|_| eyre!("invalid private FIX TRADE profiles"))?;
        return Ok(broker_ctrader::access::Profile::Hybrid {
            open_api: api.clone(),
            quote: quotes
                .remove(name)
                .ok_or_else(|| eyre!("companion QUOTE credentials missing"))?,
            trade: trades
                .remove(name)
                .ok_or_else(|| eyre!("companion TRADE credentials missing"))?,
            binding,
        });
    }
    Ok(broker_ctrader::access::Profile::OpenApi(api.clone()))
}

pub(super) fn metadata_pin(p: &broker_ctrader::access::Profile) -> Result<String> {
    use broker_ctrader::access::Profile;
    match p {
        Profile::OpenApi(api) => Ok(api.account_id.to_string()),
        Profile::Hybrid { quote, .. } | Profile::Fix(quote) | Profile::FixTrading { quote, .. } => {
            Ok(quote.username()?.to_owned())
        }
    }
}

/// Preserve metadata-only registration for existing quotes/management profiles.
/// An invalid OAuth or companion profile must never silently fall back to FIX.
pub(super) fn existing_fix(
    name: &str,
    paths: &Paths,
) -> Result<Option<broker_ctrader::access::Profile>> {
    let doc = files::read_document(&paths.oauth, true)?;
    let oauth: BTreeMap<String, ctrader_api::Config> =
        toml::from_str(&doc.to_string()).map_err(|_| eyre!("invalid private cTrader profiles"))?;
    if oauth.contains_key(name) {
        return Ok(None);
    }
    let doc = files::read_document(&paths.companion, true)?;
    let bindings: BTreeMap<String, broker_ctrader::access::CompanionBinding> =
        toml::from_str(&doc.to_string()).map_err(|_| eyre!("invalid companion profiles"))?;
    ensure!(
        !bindings.contains_key(name),
        "FIX companion binding requires its Open API profile"
    );
    let doc = files::read_document(&paths.quote, true)?;
    let mut quotes: BTreeMap<String, ctrader_fix::Config> = toml::from_str(&doc.to_string())
        .map_err(|_| eyre!("invalid private FIX QUOTE profiles"))?;
    let Some(quote) = quotes.remove(name) else {
        return Ok(None);
    };
    let doc = files::read_document(&paths.trade, true)?;
    let mut trades: BTreeMap<String, ctrader_fix::TradeConfig> =
        toml::from_str(&doc.to_string())
            .map_err(|_| eyre!("invalid private FIX TRADE profiles"))?;
    Ok(Some(match trades.remove(name) {
        Some(trade) => broker_ctrader::access::Profile::FixTrading { quote, trade },
        None => broker_ctrader::access::Profile::Fix(quote),
    }))
}
