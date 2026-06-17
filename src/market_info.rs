//! `market-info` query handler: return TradeNation's per-instrument
//! market details (trading session hours, spread, margin, guaranteed-stop
//! terms, expiry) for the intent's `instrument`.
//!
//! Read-only. Unlike the KV-only control actions (`status`, `unlock`, …)
//! this needs a live TradeNation broker — `broker.market_info` is not on
//! the generic [`Broker`](trade_control_core::broker::Broker) trait — so
//! it's dispatched from the broker-acquire section of `lib.rs::main`
//! rather than the early control block, and returns its own `Response`
//! directly (it is not an `ActionResult`, so it skips
//! `record_dispatcher_outcome`). It records `seen` like the other control
//! handlers, for `status` visibility, since it is a state-free idempotent
//! query (replaying it is harmless).
//!
//! TradeNation-only: there is no OANDA equivalent yet, so a non-TN intent
//! is rejected with a clear 400 before any broker work.

use worker::{Env, Response, Result};

use crate::state::KvStateStore;
use trade_control_core::incoming;
use trade_control_core::intent::BrokerKind;

/// Pure guard: is this broker supported by `market-info`? Only TradeNation
/// exposes per-instrument market info today; OANDA has no equivalent. Kept
/// separate from the I/O wrapper so the one branch with logic is unit
/// testable without an `Env` / live broker.
pub(crate) fn broker_supported(broker: BrokerKind) -> bool {
    matches!(broker, BrokerKind::TradeNation)
}

/// Handle a verified `market-info` intent. Acquire the TradeNation broker
/// for the intent's account, resolve `instrument` (a TN MarketName) to a
/// numeric market id via the same `resolve_market` path the order code
/// uses, fetch the market info, and return it serialised as YAML.
pub(crate) async fn handle_market_info(
    env: &Env,
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    // TradeNation-only. The CLI builder sets `broker: tradenation`, but a
    // hand-crafted intent could ask for OANDA — reject clearly rather than
    // silently doing nothing.
    if !broker_supported(verified.intent.broker) {
        rlog!(
            "market-info rejected: broker={:?} not supported (TradeNation only) id={}",
            verified.intent.broker,
            verified.intent.id
        );
        return Response::error("market-info: TradeNation only", 400);
    }

    let instrument = &verified.intent.instrument;
    let account = verified.intent.account.as_deref();

    // Reuse the worker's standard TN broker acquisition (handles the
    // session-secret read + KV session caching). `None` means login
    // failed — same 503 the trade path returns.
    let Some(broker) = crate::acquire_tn_broker(env, account).await else {
        return Response::error(
            "tradenation login failed (missing account, bad credentials, or expired \
             session — check worker logs)",
            503,
        );
    };

    // Name → market_id, exactly as the quote / candle paths do
    // (`tradenation_adapter.rs`). Do NOT reinvent the resolver.
    let market = match tradenation_api::resolve_market(
        broker.client(),
        broker.session(),
        instrument,
    )
    .await
    {
        Ok(m) => m,
        Err(err) => {
            rlog_err!("market-info resolve_market({instrument}): {err:?}");
            return Response::error("market-info: instrument not found at broker", 502);
        }
    };

    let info = match broker.market_info(market.market_id).await {
        Ok(info) => info,
        Err(err) => {
            rlog_err!(
                "market-info({instrument}, market_id={}): {err:?}",
                market.market_id
            );
            return Response::error("market-info: broker query failed", 502);
        }
    };

    rlog!(
        "market-info ok instrument={instrument} market_id={} session={:?}",
        market.market_id,
        info.trade_session.raw_london
    );

    // Idempotent read — record seen so a replayed query is visible in
    // `status` (and harmlessly deduped, like the other control actions).
    crate::record_seen(store, verified, now, "market-info").await;

    let body = match serde_yaml::to_string(&info) {
        Ok(s) => s,
        Err(err) => {
            rlog_err!("market-info serialise: {err}");
            return Response::error("internal error", 500);
        }
    };
    Response::ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tradenation_is_supported() {
        assert!(broker_supported(BrokerKind::TradeNation));
        assert!(!broker_supported(BrokerKind::Oanda));
    }
}
