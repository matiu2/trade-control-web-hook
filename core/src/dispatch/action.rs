//! The broker-dispatch match: route a verified intent to its handler.

use super::action_result::ActionResult;
use super::close::run_close;
use super::enter::run_enter;
use super::invalidate::run_invalidate;
use super::veto::run_veto_with_broker;
use crate::broker::Broker;
use crate::dispatch_config::DispatchConfig;
use crate::incoming;
use crate::intent::Action;
use crate::state::StateStore;

/// Dispatch `Enter` / `Close` / `Invalidate` / escalated `Veto` against an
/// authenticated broker. Status / Unlock / Prep / `stop-next-entry` Veto /
/// Clear-* are handled before this function and never reach it.
pub async fn run_action<B: Broker, S: StateStore>(
    broker: &B,
    store: &S,
    verified: &incoming::Verified,
    cfg: &DispatchConfig,
    now: chrono::DateTime<chrono::Utc>,
    raw_body: &str,
) -> ActionResult {
    match verified.intent.action {
        Action::Enter => run_enter(broker, store, verified, cfg, now, Some(raw_body), None).await,
        Action::Close => run_close(broker, store, verified, now).await,
        Action::Invalidate => run_invalidate(broker, store, verified, now).await,
        Action::Veto => run_veto_with_broker(broker, store, verified, now).await,
        Action::Status
        | Action::Unlock
        | Action::Prep
        | Action::PrepExpire
        | Action::ClearPrep
        | Action::ClearVeto
        | Action::Pause
        | Action::Resume
        | Action::NewsStart
        | Action::NewsEnd
        | Action::Register
        | Action::PlanList
        | Action::PlanShow
        | Action::PlanTimeline
        | Action::PlanDelete
        | Action::PlanPurge
        | Action::PurgeOlderThan
        // MarketInfo needs the concrete TradeNation broker (its `market_info`
        // is not on the generic `Broker` trait), so it's dispatched in the
        // broker-acquire section before this generic function — never here.
        | Action::MarketInfo => {
            // Handled before broker dispatch; never reached here.
            unreachable!("non-broker actions handled before broker dispatch")
        }
    }
}
