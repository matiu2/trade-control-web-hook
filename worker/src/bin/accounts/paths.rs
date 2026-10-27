use color_eyre::eyre::{Result, eyre};
use std::path::{Path, PathBuf};
pub struct Paths {
    pub oauth: PathBuf,
    pub quote: PathBuf,
    pub trade: PathBuf,
    pub companion: PathBuf,
    pub risk: PathBuf,
    pub state_root: PathBuf,
    pub registry: PathBuf,
}
impl Paths {
    pub fn in_root(root: &Path) -> Self {
        Self {
            oauth: root.join("ctrader-accounts.toml"),
            quote: root.join("ctrader-fix-accounts.toml"),
            trade: root.join("ctrader-fix-trade-accounts.toml"),
            companion: root.join("ctrader-fix-companions.toml"),
            risk: root.join("prop-risk-accounts.toml"),
            state_root: root.join("prop-risk-state"),
            registry: root.join("prop-risk-registry"),
        }
    }
    pub fn resolve() -> Result<Self> {
        let home = PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| eyre!("HOME required for account profile paths"))?,
        );
        let root = home.join(".config/trade-control");
        let mut p = Self::in_root(&root);
        p.oauth = override_path("CTRADER_ACCOUNTS_FILE", p.oauth)?;
        p.quote = override_path("CTRADER_FIX_ACCOUNTS_FILE", p.quote)?;
        p.trade = override_path("CTRADER_FIX_TRADE_ACCOUNTS_FILE", p.trade)?;
        p.companion = override_path("CTRADER_FIX_COMPANIONS_FILE", p.companion)?;
        p.risk = override_path("PROP_RISK_ACCOUNTS_FILE", p.risk)?;
        p.state_root = home.join(".local/state/trade-control/prop-risk");
        p.registry = override_path(
            "PROP_RISK_REGISTRY_DIR",
            home.join(".local/state/trade-control/prop-risk-registry"),
        )?;
        Ok(p)
    }
}
fn override_path(variable: &str, default: PathBuf) -> Result<PathBuf> {
    Ok(std::path::absolute(
        std::env::var_os(variable)
            .map(PathBuf::from)
            .unwrap_or(default),
    )?)
}
