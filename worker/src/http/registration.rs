//! Reject plans whose selected account cannot supply their data or entries.
use broker_ctrader::access::Capabilities;
use trade_control_core::{
    account::MetadataStore, dispatch::ControlResult, incoming::Verified, intent::BrokerKind,
};

use super::AppState;

pub(super) async fn unavailable(state: &AppState, verified: &Verified) -> Option<ControlResult> {
    let plan = verified.intent.trade_plan.as_ref()?;
    if verified.intent.broker != BrokerKind::Ctrader
        && !plan
            .rules
            .iter()
            .any(|rule| rule.intent.broker == BrokerKind::Ctrader)
    {
        return None;
    }
    let Some(name) = verified.intent.account.as_deref() else {
        return Some(ControlResult::error(
            "cTrader plan registration requires an account",
            422,
        ));
    };
    let metadata = match state.accounts.get(name).await {
        Ok(metadata) => metadata,
        Err(error) => {
            return Some(ControlResult::error(
                format!("cTrader account: {error}"),
                422,
            ));
        }
    };
    if metadata.broker != BrokerKind::Ctrader {
        return Some(ControlResult::error(
            "cTrader plan account broker differs from the intent",
            422,
        ));
    }
    let capabilities = crate::account_capabilities::capabilities(&metadata);
    refusal(&capabilities, plan.shadow).map(|reason| ControlResult::error(reason, 422))
}

fn refusal(capabilities: &Capabilities, paper: bool) -> Option<String> {
    (!capabilities.history || (!paper && !capabilities.execution)).then(|| {
        format!(
            "account unavailable for plan registration: {}",
            capabilities.reason
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    fn init() {
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    }

    fn capabilities(history: bool, execution: bool) -> Capabilities {
        Capabilities {
            quotes: true,
            management: true,
            history,
            execution,
            readiness: "test".into(),
            reason: "missing data".into(),
        }
    }

    #[test]
    fn fix_management_cannot_register_live_or_paper_plans_without_history() {
        init();
        let caps = capabilities(false, false);
        assert!(refusal(&caps, false).is_some());
        assert!(refusal(&caps, true).is_some());
    }

    #[test]
    fn paper_needs_history_while_live_also_needs_protected_entries() {
        init();
        assert!(refusal(&capabilities(true, false), true).is_none());
        assert!(refusal(&capabilities(true, false), false).is_some());
        assert!(refusal(&capabilities(true, true), false).is_none());
        assert!(refusal(&Capabilities::unavailable("bad config".into()), true).is_some());
    }
}
