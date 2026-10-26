use super::{
    args::Args,
    evidence::Report,
    geometry::{self, Kind},
    verify,
};
use broker_ctrader::{CtraderBroker, InstrumentMetadata, access::Profile};
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};
use tokio::time::sleep;
use trade_control_core::broker::{Broker, EntryRequest};

pub async fn run(args: &Args) -> Result<()> {
    ensure!(
        args.account == "ctrader-demo",
        "integration probes are restricted to the explicit ctrader-demo account"
    );
    ensure!(
        args.instrument == "EURUSD",
        "this integration probe is restricted to EURUSD"
    );
    ensure!(args.max_open_positions > 0, "position cap must be positive");
    let profile = Profile::load(&args.account)?;
    ensure!(
        matches!(&profile,Profile::Hybrid{open_api,..} if matches!(open_api.environment,ctrader_api::Environment::Demo)),
        "integration probes require the verified demo FIX/Open API hybrid profile"
    );
    profile.validate(false, None)?;
    let mut report = Report::create(&args.report, &args.account, &args.instrument, args.execute)?;
    let result = connected_run(args, profile, &mut report).await;
    match &result {
        Ok(()) => report.finish(if args.execute { "passed" } else { "planned" })?,
        Err(error) => {
            report.value["error"] = json!(format!("{error:#}"));
            report.finish("failed")?;
        }
    }
    println!(
        "{}",
        json!({"status":report.value["status"],"account":args.account,"report":args.report,"executed":args.execute})
    );
    result
}

async fn connected_run(args: &Args, profile: Profile, report: &mut Report) -> Result<()> {
    let broker = CtraderBroker::connect_profile(&args.account, profile).await?;
    let result = probes(args, &broker, report).await;
    if result.is_err() {
        // Evidence only: an error never triggers another order, cancel or close.
        match broker.diagnostic_snapshot().await {
            Ok(snapshot) => report.step("failure-snapshot", snapshot)?,
            Err(error) => report.step("failure-snapshot-error", json!(format!("{error:#}")))?,
        }
    }
    result
}

struct Baseline {
    orders: BTreeSet<i64>,
    positions: BTreeSet<i64>,
}

async fn probes(args: &Args, broker: &CtraderBroker, report: &mut Report) -> Result<()> {
    let meta = broker.instrument_metadata(&args.instrument).await?;
    let native = broker.diagnostic_instrument(&args.instrument).await?;
    let snapshot = broker.diagnostic_snapshot().await?;
    let baseline = Baseline {
        orders: verify::ids(&snapshot, "order", "orderId")?,
        positions: verify::ids(&snapshot, "position", "positionId")?,
    };
    report.step("preflight",json!({"account_id":broker.account_id(),"native_instrument":native,"snapshot":snapshot,"minimum_units":meta.min_units,"volume_step":meta.step_units,"pip_size":meta.pip_size,"digits":meta.digits}))?;
    // Refuse any possible netting into a user's existing EURUSD position.
    ensure!(
        !verify::rows(&snapshot, "position")?
            .iter()
            .any(|p| verify::integer(&p["tradeData"], "symbolId").ok() == Some(meta.id)),
        "an existing EURUSD position prevents an isolated integration probe"
    );
    for kind in [Kind::Stop, Kind::Limit, Kind::Market] {
        let quote = broker.diagnostic_quote(&meta.name).await?;
        let req = geometry::request(&meta, quote, kind, args.pending_distance_pips)?;
        report.step("probe-request",json!({"kind":kind.name(),"bid":quote.bid,"ask":quote.ask,"units":meta.min_units,"entry":req.entry.reference_price(),"stop_loss":req.stop_loss,"take_profit":req.take_profit,"execute":args.execute}))?;
        if args.execute {
            execute_probe(args, broker, report, &baseline, &meta, kind, &req).await?;
        }
    }
    final_snapshot(broker, report, &baseline).await?;
    Ok(())
}

async fn execute_probe(
    args: &Args,
    broker: &CtraderBroker,
    report: &mut Report,
    baseline: &Baseline,
    meta: &InstrumentMetadata,
    kind: Kind,
    req: &EntryRequest<'_>,
) -> Result<()> {
    let native_volume = volume(meta.min_units)?;
    tracing::info!(
        kind = kind.name(),
        units = meta.min_units,
        "submitting one demo probe"
    );
    let placed = broker
        .place_entry(1.0, args.max_open_positions, req)
        .await
        .map_err(|e| eyre!("probe entry: {e}"))?;
    // Checkpoint the ID before doing any other read or mutation.
    report.step("entry-receipt",json!({"kind":kind.name(),"order_id":placed.order_id,"units":placed.size,"requested_price":placed.price}))?;
    let id = placed.order_id.parse::<i64>()?;
    verify::own(id, &baseline.orders)?;
    match kind {
        Kind::Stop | Kind::Limit => {
            pending_cycle(args, broker, report, meta, id, native_volume, req).await
        }
        Kind::Market => market_cycle(broker, report, baseline, meta, id, native_volume, req).await,
    }
}

async fn final_snapshot(
    broker: &CtraderBroker,
    report: &mut Report,
    baseline: &Baseline,
) -> Result<()> {
    let snapshot = broker.diagnostic_snapshot().await?;
    let orders = verify::ids(&snapshot, "order", "orderId")?;
    let positions = verify::ids(&snapshot, "position", "positionId")?;
    let steps = verify::rows(&report.value, "steps")?;
    let probe_orders: BTreeSet<_> = steps
        .iter()
        .filter(|s| s["step"] == "entry-receipt")
        .map(|s| {
            let id = s["evidence"]["order_id"]
                .as_str()
                .ok_or_else(|| eyre!("probe receipt has no order ID"))?;
            Ok::<_, color_eyre::Report>(id.parse::<i64>()?)
        })
        .collect::<Result<_>>()?;
    let probe_positions: BTreeSet<_> = steps
        .iter()
        .filter(|s| s["step"] == "close-submitting")
        .map(|s| verify::integer(&s["evidence"], "position_id"))
        .collect::<Result<_>>()?;
    let remaining_orders: Vec<_> = probe_orders.intersection(&orders).copied().collect();
    let remaining_positions: Vec<_> = probe_positions.intersection(&positions).copied().collect();
    report.step("final-snapshot",json!({"snapshot":snapshot,
        "baseline_orders_no_longer_pending":baseline.orders.difference(&orders).copied().collect::<Vec<_>>(),
        "baseline_positions_no_longer_open":baseline.positions.difference(&positions).copied().collect::<Vec<_>>(),
        "remaining_probe_orders":remaining_orders,"remaining_probe_positions":remaining_positions}))?;
    ensure!(
        remaining_orders.is_empty() && remaining_positions.is_empty(),
        "probe orders/positions remain after test cleanup"
    );
    Ok(())
}

fn volume(units: f64) -> Result<i64> {
    let native = units * 100.0;
    ensure!(
        native.is_finite()
            && native > 0.0
            && native < i64::MAX as f64
            && (native - native.round()).abs() < 1e-6,
        "native minimum volume is not a whole cent-unit"
    );
    Ok(native.round() as i64)
}

async fn pending_cycle(
    args: &Args,
    broker: &CtraderBroker,
    report: &mut Report,
    meta: &InstrumentMetadata,
    id: i64,
    volume: i64,
    req: &EntryRequest<'_>,
) -> Result<()> {
    observe(broker, report, id, "pending-read", |d, s| {
        verify::pending(d, s, id, meta.id, volume)?;
        verify::pending_geometry(d, req)
    })
    .await?;
    report.step("cancel-submitting", json!({"order_id":id}))?;
    broker
        .cancel_order(&args.account, &id.to_string())
        .await
        .map_err(|e| eyre!("probe cancellation: {e:?}; no write retry"))?;
    report.step("cancel-receipt", json!({"order_id":id,"acknowledged":true}))?;
    observe(broker, report, id, "cancelled-read", |d, s| {
        verify::cancelled(d, s, id)
    })
    .await?;
    Ok(())
}

async fn market_cycle(
    broker: &CtraderBroker,
    report: &mut Report,
    baseline: &Baseline,
    meta: &InstrumentMetadata,
    id: i64,
    volume: i64,
    req: &EntryRequest<'_>,
) -> Result<()> {
    let (detail, snapshot) = observe(broker, report, id, "market-read", |d, s| {
        verify::filled_position(d, s, id, meta.id, volume, req.stop_loss, req.take_profit)
            .map(|_| ())
    })
    .await?;
    let pid = verify::filled_position(
        &detail,
        &snapshot,
        id,
        meta.id,
        volume,
        req.stop_loss,
        req.take_profit,
    )?;
    verify::own(pid, &baseline.positions)?;
    let client = verify::client_identity(&detail)?;
    report.step(
        "close-submitting",
        json!({"opening_order_id":id,"position_id":pid}),
    )?;
    let closed = broker
        .close_exact_position(&id.to_string(), &pid.to_string(), client)
        .await?;
    report.step("close-proof", closed.clone())?;
    ensure!(
        closed["confirmed_closed"] == true,
        "exact close is not confirmed"
    );
    let history = &closed["history"];
    let closing_id = verify::integer(&closed["closing_order"]["order"], "orderId")?.to_string();
    let closing_deals = verify::ids(&closed["closing_order"], "deal", "dealId")?;
    ensure!(
        history["broker_trade_id"].as_str() == Some(pid.to_string().as_str())
            && history["closed_at"].is_string()
            && history["exit_order_id"].as_str() == Some(closing_id.as_str())
            && history["transaction_id"]
                .as_str()
                .and_then(|id| id.parse::<i64>().ok())
                .is_some_and(|id| closing_deals.contains(&id)),
        "position closed, but closed-trade history reporting did not confirm matching IDs and time; no close retry"
    );
    let after = broker.diagnostic_snapshot().await?;
    report.step("closed-position-read", after.clone())?;
    ensure!(
        !verify::ids(&after, "position", "positionId")?.contains(&pid),
        "closed probe position is still present"
    );
    Ok(())
}

async fn observe(
    broker: &CtraderBroker,
    report: &mut Report,
    id: i64,
    name: &str,
    check: impl Fn(&Value, &Value) -> Result<()>,
) -> Result<(Value, Value)> {
    let mut last = String::new();
    for attempt in 1..=3 {
        let reads = async {
            Ok::<_, color_eyre::Report>((
                broker.diagnostic_order(&id.to_string()).await?,
                broker.diagnostic_snapshot().await?,
            ))
        }
        .await;
        match reads {
            Ok((detail, snapshot)) => {
                report.step(
                    name,
                    json!({"attempt":attempt,"order":detail,"snapshot":snapshot}),
                )?;
                match check(&detail, &snapshot) {
                    Ok(()) => return Ok((detail, snapshot)),
                    Err(error) => last = format!("{error:#}"),
                }
            }
            Err(error) => last = format!("{error:#}"),
        }
        report.step(
            "read-verification-error",
            json!({"operation":name,"attempt":attempt,"error":last}),
        )?;
        if attempt < 3 {
            sleep(Duration::from_millis(500)).await;
        }
    }
    Err(eyre!(
        "{name}: bounded read verification failed: {last}; no further writes were sent"
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_volume_never_assumes_a_lot() {
        crate::test_logging();
        assert_eq!(super::volume(1000.0).unwrap(), 100000);
        assert_eq!(super::volume(100000.0).unwrap(), 10000000);
        assert!(super::volume(0.001).is_err());
        assert!(super::volume(f64::NAN).is_err());
    }
}
