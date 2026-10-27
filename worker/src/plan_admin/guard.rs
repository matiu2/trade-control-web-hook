//! Refuse old signed webhook mutations after an operator changes plan ownership.

use trade_control_core::{
    intent::Intent,
    state::{StateError, StateStore},
};

pub(crate) async fn blocked<S: StateStore>(
    store: &S,
    intent: &Intent,
) -> Result<Option<&'static str>, StateError> {
    let Some(trade_id) = intent.trade_id.as_deref() else {
        return Ok(None);
    };
    let active = store.list_all_trade_plans().await?;
    let matching: Vec<_> = active
        .iter()
        .filter(|stored| stored.plan.trade_id == trade_id)
        .collect();
    if !matching.is_empty() {
        if matching.iter().any(|stored| stored.plan.shadow) {
            return Ok(Some(
                "plan is paper/observe-only; broker mutation suppressed",
            ));
        }
        if matching.len() != 1 || matching[0].account != intent.account {
            return Ok(Some(
                "plan account changed; old signed broker mutation suppressed",
            ));
        }
        let plan = &matching[0].plan;
        if plan.instrument != intent.instrument
            || plan
                .rules
                .iter()
                .any(|rule| rule.intent.broker != intent.broker)
        {
            return Ok(Some(
                "plan broker or instrument changed; old signed broker mutation suppressed",
            ));
        }
        return Ok(None);
    }
    let archived = store.list_all_archived_plans().await?;
    let archived: Vec<_> = archived
        .iter()
        .filter(|stored| stored.plan.trade_id == trade_id)
        .collect();
    if archived.iter().any(|stored| {
        stored.plan.shadow
            || stored
                .final_state
                .fired
                .contains("operator:closed-manually")
    }) {
        return Ok(Some(
            "plan was retired or observed on paper; broker mutation suppressed",
        ));
    }
    if !archived.is_empty()
        && archived
            .iter()
            .all(|stored| stored.account != intent.account)
    {
        return Ok(Some(
            "plan account changed; old signed broker mutation suppressed",
        ));
    }
    Ok(None)
}

/// A stale register command must not recreate the detached old account or
/// replace an operator's observe-only plan with its original live definition.
pub(crate) async fn register_blocked<S: StateStore>(
    store: &S,
    intent: &Intent,
) -> Result<Option<&'static str>, StateError> {
    let Some(plan) = intent.trade_plan.as_ref() else {
        return Ok(None);
    };
    let active = store.list_all_trade_plans().await?;
    if active
        .iter()
        .filter(|stored| stored.plan.trade_id == plan.trade_id)
        .any(|stored| stored.account != intent.account || stored.plan.shadow != plan.shadow)
    {
        return Ok(Some(
            "plan administration changed its account or mode; delete explicitly before re-registering",
        ));
    }
    let archived = store.list_all_archived_plans().await?;
    if archived
        .iter()
        .filter(|stored| stored.plan.trade_id == plan.trade_id)
        .any(|stored| {
            stored.plan.shadow
                || stored.account != intent.account
                || stored
                    .final_state
                    .fired
                    .contains("operator:closed-manually")
        })
    {
        return Ok(Some(
            "plan was closed, moved or observed on paper; delete explicitly before re-registering",
        ));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_admin::mapping::tests::plan;
    use trade_control_core::state::MemStateStore;

    #[tokio::test]
    async fn old_webhook_cannot_trade_after_observe_or_account_switch() {
        let store = MemStateStore::default();
        let mut plan = plan();
        let old_intent = plan.rules[0].intent.clone();
        plan.shadow = true;
        store
            .put_trade_plan(Some("reversals"), &plan)
            .await
            .expect("store");
        assert!(blocked(&store, &old_intent).await.expect("guard").is_some());
        plan.shadow = false;
        store
            .put_trade_plan(Some("reversals"), &plan)
            .await
            .expect("store");
        assert!(blocked(&store, &old_intent).await.expect("guard").is_some());
        let mut current = old_intent;
        current.account = Some("reversals".into());
        assert!(blocked(&store, &current).await.expect("guard").is_none());
    }
    #[tokio::test]
    async fn archived_paper_and_manual_close_block_old_signed_register() {
        use trade_control_core::plan_state::{Phase, PlanState};
        let store = MemStateStore::default();
        let original = plan();
        let mut carrier = original.rules[0].intent.clone();
        carrier.trade_plan = Some(original.clone());
        let mut paper = original.clone();
        paper.shadow = true;
        let now = chrono::Utc::now();
        store
            .archive_plan(
                Some("reversals"),
                &paper,
                &PlanState::seed(Phase::Done, now),
                now,
                None,
            )
            .await
            .expect("archive");
        assert!(
            register_blocked(&store, &carrier)
                .await
                .expect("guard")
                .is_some()
        );
        store
            .clear_archived_plan(Some("reversals"), &original.trade_id)
            .await
            .expect("clear");
        let mut manual = PlanState::seed(Phase::Done, now);
        manual.fired.insert("operator:closed-manually".into());
        store
            .archive_plan(Some("expired"), &original, &manual, now, None)
            .await
            .expect("manual archive");
        assert!(
            register_blocked(&store, &carrier)
                .await
                .expect("guard")
                .is_some()
        );
        assert!(
            blocked(&store, &carrier)
                .await
                .expect("broker guard")
                .is_some()
        );
    }
}
