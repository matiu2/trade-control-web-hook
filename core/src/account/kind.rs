//! Demo vs live distinction. Stored on every account so the dispatch
//! can pick the right login path and apply kind-appropriate risk caps.

use serde::{Deserialize, Serialize};

/// Whether an account trades real money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    #[default]
    Demo,
    Live,
}

impl AccountKind {
    /// True for accounts that move real money. Used as the gate for
    /// stricter caps and louder logging.
    pub fn is_live(self) -> bool {
        matches!(self, AccountKind::Live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_demo() {
        assert_eq!(AccountKind::default(), AccountKind::Demo);
        assert!(!AccountKind::default().is_live());
    }

    #[test]
    fn live_round_trip_yaml() {
        let yaml = "live";
        let kind: AccountKind = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(kind, AccountKind::Live);
        assert_eq!(serde_yaml::to_string(&kind).unwrap().trim(), "live");
    }

    #[test]
    fn demo_round_trip_yaml() {
        let kind: AccountKind = serde_yaml::from_str("demo").unwrap();
        assert_eq!(kind, AccountKind::Demo);
    }

    #[test]
    fn rejects_unknown_variant() {
        let res: Result<AccountKind, _> = serde_yaml::from_str("paper");
        assert!(res.is_err());
    }
}
