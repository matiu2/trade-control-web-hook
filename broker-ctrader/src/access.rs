//! Resolve account-scoped credentials without mistaking FIX quotes for execution.
use color_eyre::eyre::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    pub quotes: bool,
    pub execution: bool,
    pub history: bool,
    pub readiness: String,
    pub reason: String,
}

impl Capabilities {
    pub fn unavailable(reason: String) -> Self {
        Self {
            quotes: false,
            execution: false,
            history: false,
            readiness: "unavailable".into(),
            reason,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Profile {
    OpenApi(ctrader_api::Config),
    Fix(ctrader_fix::Config),
}

impl Profile {
    /// Invalid OAuth configuration must never silently select another transport.
    pub fn load(name: &str) -> Result<Self> {
        Self::resolve(ctrader_api::Config::load_optional(name), || {
            ctrader_fix::Config::load_named_optional(name)
        })
    }

    fn resolve(
        open_api: Result<Option<ctrader_api::Config>>,
        fix: impl FnOnce() -> Result<Option<ctrader_fix::Config>>,
    ) -> Result<Self> {
        if let Some(config) = open_api? {
            return Ok(Self::OpenApi(config));
        }
        if let Some(config) = fix()? {
            return Ok(Self::Fix(config));
        }
        bail!(
            "No named cTrader profile; configure Open API in ctrader-accounts.toml or quote-only FIX in ctrader-fix-accounts.toml"
        )
    }

    pub fn validate(&self, is_live: bool, metadata_id: Option<&str>) -> Result<()> {
        let (configured_live, matches_id) = match self {
            Self::OpenApi(config) => {
                config.validate()?;
                (
                    matches!(config.environment, ctrader_api::Environment::Live),
                    metadata_id.is_none_or(|id| id.parse::<i64>().ok() == Some(config.account_id)),
                )
            }
            Self::Fix(config) => {
                config.validate()?;
                (
                    config.sender_comp_id.starts_with("live."),
                    metadata_id.is_none_or(|id| config.username().ok() == Some(id)),
                )
            }
        };
        ensure!(
            configured_live == is_live,
            "metadata environment differs from the pinned cTrader environment"
        );
        ensure!(
            matches_id,
            "metadata account ID differs from the pinned cTrader account ID/login"
        );
        Ok(())
    }

    /// Configured capability, not a claim that a broker session has been verified.
    pub fn capabilities(&self) -> Capabilities {
        match self {
            Self::OpenApi(_) => Capabilities { quotes: true, execution: true, history: true, readiness: "configured-open-api".into(), reason: "Open API configured; broker connectivity not checked; market entries remain disabled".into() },
            Self::Fix(_) => Capabilities { quotes: true, execution: false, history: false, readiness: "quotes-only".into(), reason: "FIX QUOTE supports live quotes only; configure Open API OAuth for execution, history and paper plan evaluation".into() },
        }
    }

    pub fn require_execution(self) -> Result<ctrader_api::Config> {
        match self {
            Self::OpenApi(config) => Ok(config),
            Self::Fix(_) => bail!(
                "cTrader FIX profile is quotes-only; configure named Open API OAuth credentials for trading, position management and candle history"
            ),
        }
    }
}

#[cfg(test)]
mod tests;
