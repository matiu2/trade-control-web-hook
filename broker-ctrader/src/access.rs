//! Resolve account-scoped credentials without mistaking FIX quotes for execution.
use color_eyre::eyre::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

mod companions;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompanionBinding {
    pub open_api_account_id: i64,
    pub fix_sender_comp_id: String,
    pub broker_name: String,
}

impl CompanionBinding {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.open_api_account_id > 0,
            "companion Open API account ID must be positive"
        );
        let parts: Vec<_> = self.fix_sender_comp_id.split('.').collect();
        ensure!(
            parts.len() == 3
                && matches!(parts[0], "demo" | "live")
                && !parts[1].is_empty()
                && parts[1].bytes().all(|byte| (0x21..=0x7e).contains(&byte))
                && !parts[2].is_empty()
                && parts[2].bytes().all(|byte| byte.is_ascii_digit()),
            "invalid companion FIX account identity"
        );
        ensure!(
            !self.broker_name.trim().is_empty()
                && self.broker_name.len() <= 256
                && self
                    .broker_name
                    .chars()
                    .all(|character| !character.is_control()),
            "companion broker name must be printable and between 1 and 256 bytes"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    pub quotes: bool,
    #[serde(default)]
    pub management: bool,
    pub execution: bool,
    pub history: bool,
    pub readiness: String,
    pub reason: String,
}

impl Capabilities {
    pub fn unavailable(reason: String) -> Self {
        Self {
            quotes: false,
            management: false,
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
    FixTrading {
        quote: ctrader_fix::Config,
        trade: ctrader_fix::TradeConfig,
    },
    Hybrid {
        open_api: ctrader_api::Config,
        quote: ctrader_fix::Config,
        trade: ctrader_fix::TradeConfig,
        binding: CompanionBinding,
    },
}

impl Profile {
    /// Invalid OAuth configuration must never silently select another transport.
    pub fn load(name: &str) -> Result<Self> {
        Self::resolve_bound(
            companions::load(name),
            ctrader_api::Config::load_optional(name),
            || ctrader_fix::Config::load_named_optional(name),
            || ctrader_fix::TradeConfig::load_named_optional(name),
        )
    }

    fn resolve_bound(
        binding: Result<Option<CompanionBinding>>,
        open_api: Result<Option<ctrader_api::Config>>,
        fix: impl FnOnce() -> Result<Option<ctrader_fix::Config>>,
        trade: impl FnOnce() -> Result<Option<ctrader_fix::TradeConfig>>,
    ) -> Result<Self> {
        let Some(binding) = binding? else {
            return Self::resolve(open_api, fix, trade);
        };
        let open_api = open_api?.ok_or_else(|| {
            color_eyre::eyre::eyre!("FIX companion binding requires a named Open API profile")
        })?;
        let quote = fix()?.ok_or_else(|| {
            color_eyre::eyre::eyre!("FIX companion binding requires a named QUOTE profile")
        })?;
        let trade = trade()?.ok_or_else(|| {
            color_eyre::eyre::eyre!("FIX companion binding requires a named TRADE profile")
        })?;
        let profile = Self::Hybrid {
            open_api,
            quote,
            trade,
            binding,
        };
        profile.validate(false, None)?;
        Ok(profile)
    }

    fn resolve(
        open_api: Result<Option<ctrader_api::Config>>,
        fix: impl FnOnce() -> Result<Option<ctrader_fix::Config>>,
        trade: impl FnOnce() -> Result<Option<ctrader_fix::TradeConfig>>,
    ) -> Result<Self> {
        if let Some(config) = open_api? {
            return Ok(Self::OpenApi(config));
        }
        if let Some(config) = fix()? {
            if let Some(trade) = trade()? {
                return Ok(Self::FixTrading {
                    quote: config,
                    trade,
                });
            }
            return Ok(Self::Fix(config));
        }
        bail!(
            "No named cTrader profile; configure Open API in ctrader-accounts.toml or FIX QUOTE in ctrader-fix-accounts.toml"
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
            Self::FixTrading { quote, trade } => {
                quote.validate()?;
                trade.validate()?;
                ensure!(
                    quote.sender_comp_id == trade.sender_comp_id,
                    "FIX QUOTE and TRADE account identities differ"
                );
                ensure!(
                    quote.target_comp_id == trade.target_comp_id,
                    "FIX QUOTE and TRADE target identities differ"
                );
                ensure!(
                    quote.sender_comp_id.starts_with("demo."),
                    "FIX management currently supports demo accounts only"
                );
                (
                    false,
                    metadata_id.is_none_or(|id| quote.username().ok() == Some(id)),
                )
            }
            Self::Hybrid {
                open_api,
                quote,
                trade,
                binding,
            } => {
                binding.validate()?;
                open_api.validate()?;
                Self::FixTrading {
                    quote: quote.clone(),
                    trade: trade.clone(),
                }
                .validate(is_live, metadata_id)?;
                ensure!(
                    matches!(open_api.environment, ctrader_api::Environment::Demo),
                    "FIX companion requires matching demo environments"
                );
                ensure!(
                    binding.open_api_account_id == open_api.account_id,
                    "companion binding Open API account identity differs"
                );
                ensure!(
                    binding.fix_sender_comp_id == quote.sender_comp_id,
                    "companion binding FIX account identity differs"
                );
                (
                    false,
                    metadata_id.is_none_or(|id| quote.username().ok() == Some(id)),
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
            Self::OpenApi(_) => Capabilities { quotes: true, management: true, execution: true, history: true, readiness: "configured-open-api".into(), reason: "Open API configured; broker connectivity not checked; market entries remain disabled".into() },
            Self::Fix(_) => Capabilities { quotes: true, management: false, execution: false, history: false, readiness: "quotes-only".into(), reason: "FIX QUOTE supports live quotes only; configure Open API OAuth for execution, history and paper plan evaluation".into() },
            Self::FixTrading { .. } => Capabilities { quotes: true, management: true, execution: false, history: false, readiness: "fix-management".into(), reason: "FIX demo supports quotes, position reads, closes and cancellations; protected strategy entries require Open API equity, symbol constraints and native SL/TP; FIX has no historical candles or full settlement history".into() },
            Self::Hybrid { .. } => Capabilities { quotes: true, management: true, execution: true, history: true, readiness: "configured-fix-open-api".into(), reason: "Bound FIX and Open API companion configured; broker identity and connectivity not yet verified; protected entries, risk data and historical candles use Open API".into() },
        }
    }

    pub fn require_execution(self) -> Result<ctrader_api::Config> {
        if matches!(self, Self::Hybrid { .. }) {
            self.validate(false, None)?;
        }
        match self {
            Self::OpenApi(config) => Ok(config),
            Self::Hybrid { open_api, .. } => Ok(open_api),
            Self::Fix(_) => bail!(
                "cTrader FIX profile is quotes-only; configure named Open API OAuth credentials for trading, position management and candle history"
            ),
            Self::FixTrading { .. } => bail!(
                "cTrader FIX management does not support protected strategy entries; configure Open API equity, symbol constraints, native SL/TP and historical candles"
            ),
        }
    }
}

#[cfg(test)]
mod companion_tests;
#[cfg(test)]
mod tests;
