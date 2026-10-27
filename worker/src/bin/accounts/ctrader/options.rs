use clap::Args;
use color_eyre::eyre::{Result, ensure, eyre};
use serde::Deserialize;
use std::io::{self, IsTerminal, Read};
#[derive(Args, Default, Clone)]
#[group(id = "CtraderCredentials")]
pub struct Options {
    /// Open API ctidTraderAccountId (different from the FIX terminal login).
    #[arg(long = "ctrader-account-id")]
    pub account_id: Option<i64>,
    /// Reuse another named profile's OAuth application credentials and access grant.
    #[arg(long = "ctrader-reuse")]
    pub reuse: Option<String>,
    /// Read a private JSON credential bundle from stdin, never from argv.
    #[arg(long = "ctrader-secrets-stdin")]
    pub secrets_stdin: bool,
    /// Prompt for FIX QUOTE/TRADE credentials and broker binding as well as OAuth.
    #[arg(long = "ctrader-fix")]
    pub fix: bool,
}
impl Options {
    pub fn is_present(&self) -> bool {
        self.account_id.is_some() || self.reuse.is_some() || self.secrets_stdin || self.fix
    }
    pub fn secrets(&self) -> Result<Secrets> {
        if self.secrets_stdin {
            return read_secrets(&mut io::stdin().lock());
        }
        ensure!(
            io::stdin().is_terminal(),
            "cTrader credentials need a terminal or --ctrader-secrets-stdin JSON input"
        );
        let (client_id, client_secret, access_token) = if self.reuse.is_some() {
            (String::new(), String::new(), String::new())
        } else {
            (
                prompt("cTrader Open API client ID: ")?,
                prompt("cTrader Open API client secret: ")?,
                prompt("cTrader Open API access token: ")?,
            )
        };
        let fix = if self.fix {
            Some(FixSecrets {
                quote: fix_quote()?,
                trade: fix_trade()?,
                broker_name: prompt("Exact Open API broker name for FIX binding: ")?,
            })
        } else {
            None
        };
        Ok(Secrets {
            client_id,
            client_secret,
            access_token,
            fix,
        })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    pub client_id: String,
    pub client_secret: String,
    pub access_token: String,
    pub fix: Option<FixSecrets>,
}
impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secrets([REDACTED])")
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixSecrets {
    pub quote: ctrader_fix::Config,
    pub trade: ctrader_fix::TradeConfig,
    pub broker_name: String,
}
fn prompt(label: &str) -> Result<String> {
    rpassword::prompt_password(label)
        .map_err(|_| eyre!("reading cTrader credential failed; values withheld"))
}
pub(super) fn read_secrets(input: &mut impl Read) -> Result<Secrets> {
    let mut bytes = Vec::new();
    input
        .take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|_| eyre!("reading cTrader secret input failed"))?;
    ensure!(bytes.len() <= 65_536, "cTrader secret input too large");
    serde_json::from_slice(&bytes)
        .map_err(|_| eyre!("invalid cTrader credential JSON; values withheld"))
}
fn fix_quote() -> Result<ctrader_fix::Config> {
    Ok(ctrader_fix::Config {
        host: prompt("FIX QUOTE host: ")?,
        port: prompt("FIX QUOTE SSL port: ")?
            .parse()
            .map_err(|_| eyre!("invalid FIX QUOTE port"))?,
        sender_comp_id: prompt("FIX sender comp ID (demo.broker.login): ")?,
        target_comp_id: "cServer".into(),
        sender_sub_id: "QUOTE".into(),
        password: prompt("FIX QUOTE password: ")?,
    })
}
fn fix_trade() -> Result<ctrader_fix::TradeConfig> {
    Ok(ctrader_fix::TradeConfig {
        host: prompt("FIX TRADE host: ")?,
        port: prompt("FIX TRADE SSL port: ")?
            .parse()
            .map_err(|_| eyre!("invalid FIX TRADE port"))?,
        sender_comp_id: prompt("FIX TRADE sender comp ID: ")?,
        target_comp_id: "cServer".into(),
        sender_sub_id: "TRADE".into(),
        password: prompt("FIX TRADE password: ")?,
    })
}

pub(super) fn account_id(previous: Option<i64>) -> Result<i64> {
    let id = match previous {
        Some(id) => id,
        None => {
            ensure!(
                io::stdin().is_terminal(),
                "new cTrader profile requires --ctrader-account-id (Open API ID, not terminal login)"
            );
            prompt("cTrader Open API account ID (not terminal login): ")?
                .parse()
                .map_err(|_| eyre!("invalid cTrader Open API account ID"))?
        }
    };
    ensure!(id > 0, "cTrader Open API account ID must be positive");
    Ok(id)
}
