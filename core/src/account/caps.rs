//! Per-account risk caps. Override the worker-wide defaults so a live
//! account can run tighter than a demo on the same worker.

use serde::{Deserialize, Serialize};

/// Risk caps that apply to entries placed against an account. `None`
/// fields mean "fall back to the worker-wide default".
#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize, Serialize)]
pub struct AccountCaps {
    /// Maximum `risk_pct` allowed on any single entry against this
    /// account. Tighter than the worker-wide cap; never looser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_risk_pct: Option<f64>,
    /// Maximum simultaneous open positions for this account. Hit means
    /// reject new entries until something closes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_open_positions: Option<u32>,
}

impl AccountCaps {
    /// Resolve `max_risk_pct` against a worker-wide default. The
    /// account's cap is honoured iff it is *tighter* (smaller) than the
    /// worker-wide cap — an account record can never relax a global
    /// safety bound, only narrow it.
    pub fn resolve_max_risk_pct(&self, worker_default: f64) -> f64 {
        match self.max_risk_pct {
            Some(account) if account < worker_default => account,
            _ => worker_default,
        }
    }

    /// Resolve `max_open_positions` against a worker-wide default.
    /// Same "narrower-only" rule as [`resolve_max_risk_pct`].
    pub fn resolve_max_open_positions(&self, worker_default: u32) -> u32 {
        match self.max_open_positions {
            Some(account) if account < worker_default => account,
            _ => worker_default,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_caps_round_trip() {
        let caps = AccountCaps::default();
        assert!(caps.max_risk_pct.is_none());
        assert!(caps.max_open_positions.is_none());
    }

    #[test]
    fn account_cap_narrower_wins() {
        let caps = AccountCaps {
            max_risk_pct: Some(0.5),
            max_open_positions: Some(1),
        };
        assert_eq!(caps.resolve_max_risk_pct(1.0), 0.5);
        assert_eq!(caps.resolve_max_open_positions(3), 1);
    }

    #[test]
    fn account_cap_looser_is_ignored() {
        // Live cap claims 5% but worker default is 1% — the worker
        // default wins. An account record cannot weaken the global
        // safety bound, only tighten it.
        let caps = AccountCaps {
            max_risk_pct: Some(5.0),
            max_open_positions: Some(99),
        };
        assert_eq!(caps.resolve_max_risk_pct(1.0), 1.0);
        assert_eq!(caps.resolve_max_open_positions(3), 3);
    }

    #[test]
    fn missing_cap_falls_through_to_default() {
        let caps = AccountCaps::default();
        assert_eq!(caps.resolve_max_risk_pct(1.5), 1.5);
        assert_eq!(caps.resolve_max_open_positions(2), 2);
    }

    #[test]
    fn account_cap_equal_to_default_uses_default() {
        // Exactly equal: not strictly narrower, so the default applies.
        // Documented behaviour — keeps the "tighter wins" rule sharp.
        let caps = AccountCaps {
            max_risk_pct: Some(1.0),
            max_open_positions: Some(3),
        };
        assert_eq!(caps.resolve_max_risk_pct(1.0), 1.0);
        assert_eq!(caps.resolve_max_open_positions(3), 3);
    }

    #[test]
    fn yaml_omits_none_fields() {
        let caps = AccountCaps::default();
        let yaml = serde_yaml::to_string(&caps).unwrap();
        // No `max_risk_pct: null` / `max_open_positions: null` lines.
        assert!(!yaml.contains("max_risk_pct"));
        assert!(!yaml.contains("max_open_positions"));
    }

    #[test]
    fn yaml_serialises_present_fields() {
        let caps = AccountCaps {
            max_risk_pct: Some(0.25),
            max_open_positions: None,
        };
        let yaml = serde_yaml::to_string(&caps).unwrap();
        assert!(yaml.contains("max_risk_pct: 0.25"));
        assert!(!yaml.contains("max_open_positions"));
    }
}
