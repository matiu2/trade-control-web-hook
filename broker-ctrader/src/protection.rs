//! Account opt-in prop risk gate shared by Open API and hybrid strategy entries.
//! No profile is enabled or enrolled implicitly. FIX-only strategy entries already refuse.
use crate::*;
use adaptive_risk::{
    AccountSnapshot, Authorization, Decimal, ProposedTrade, RiskManager, RiskPolicy, RiskSession,
    Side, StandardLossRule, VolumeLimits,
};
use trade_control_core::intent::Direction;
mod binding;
mod config;
mod exposure;
mod history;
mod inventory;
mod numbers;
mod reconcile;
pub mod registration;
#[cfg(test)]
mod tests;
pub use config::{Costs, FeeMode, FirmEvidence, PropAccount};
use numbers::*;

pub(crate) struct Guard {
    session: RiskSession,
    profile: PropAccount,
    authorization: Authorization,
    trade: ProposedTrade,
    cap: Decimal,
    transport_started: bool,
}
impl Guard {
    pub(crate) fn mark_transport(&mut self) {
        self.transport_started = true;
    }
    pub(crate) fn price(&self) -> Result<f64> {
        float(self.trade.estimate.price)
    }
    pub(crate) fn setup_id(&self) -> &str {
        &self.trade.setup_id
    }
    pub(crate) fn id(&self) -> &str {
        self.authorization.request_id()
    }
    pub(crate) fn cents(&self) -> Result<i64> {
        Ok(
            times(self.authorization.decision().max_volume, Decimal::from(100))?
                .normalize()
                .to_string()
                .parse()?,
        )
    }
    pub(crate) fn max_stop_distance(&self) -> Result<f64> {
        let stop = self
            .trade
            .estimate
            .stop
            .ok_or_else(|| eyre!("approved stop missing"))?;
        float(minus(self.trade.estimate.price, stop)?.abs())
    }
    pub(crate) async fn before_submit(
        &mut self,
        b: &OpenApiBroker,
        req: &EntryRequest<'_>,
        symbol: &InstrumentMetadata,
    ) -> Result<()> {
        let revision = b.shared.risk_revision();
        let mut snapshot = reconcile::collect(b, &self.profile, &mut self.session).await?;
        let mut refreshed = proposal(
            b,
            &self.profile,
            req,
            symbol,
            ProposalTerms {
                id: self.trade.request_id.clone(),
                requested_volume: self.trade.requested_volume,
                cap: self.cap,
            },
            &mut snapshot,
        )
        .await?;
        ensure!(
            b.shared.risk_revision() == revision && b.shared.risk_healthy(),
            "account changed before submission"
        );
        ensure!(
            refreshed.requested_risk >= self.trade.requested_risk,
            "strategy/account risk cap decreased before submission"
        );
        refreshed.requested_risk = self.trade.requested_risk;
        self.session.before_submit(
            &self.authorization,
            &refreshed,
            &snapshot,
            &StandardLossRule,
            Utc::now(),
        )
    }
    pub(crate) fn outcome(&mut self, proven_absent: bool) -> Result<()> {
        if proven_absent {
            self.session.rejected_without_fill(&self.authorization)
        } else {
            self.session.disconnect()
        }
    }
}
pub(crate) async fn prepare(
    b: &OpenApiBroker,
    req: &EntryRequest<'_>,
    symbol: &InstrumentMetadata,
    volume: i64,
    cap: f64,
    client_id: Option<&str>,
) -> Result<Option<Guard>> {
    let Some(profile) = config::load(
        &b.account_name,
        b.account_id(),
        b.shared.client.environment(),
    )?
    else {
        return Ok(None);
    };
    profile.validate()?;
    ensure!(
        !req.dry_run,
        "protected dry runs require evaluate API without reserving live risk"
    );
    // Existing market-entry audit supplies request IDs. Pending Open API entries get
    // a unique ID here, persisted before transport and retained through ambiguity.
    let id = client_id
        .map(str::to_owned)
        .unwrap_or_else(|| format!("risk-{}-{}", b.account_id(), Utc::now().timestamp_micros()));
    let setup = req
        .setup_id
        .ok_or_else(|| eyre!("stable setup identity required for prop entry"))?;
    ensure!(
        !setup.is_empty()
            && setup.len() <= 95
            && setup
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':')),
        "setup identity must have 1..95 safe ASCII characters"
    );
    let mut session = profile.manager()?.begin()?;
    ensure!(
        session.state().initial_balance == profile.initial_balance,
        "initial reference differs from enrolled account"
    );
    let mut snapshot = reconcile::collect(b, &profile, &mut session).await?;
    let requested = divide(Decimal::from(volume), Decimal::from(100))?;
    let trade = proposal(
        b,
        &profile,
        req,
        symbol,
        ProposalTerms {
            id,
            requested_volume: requested,
            cap: decimal(cap)?,
        },
        &mut snapshot,
    )
    .await?;
    let authorization =
        session.authorize(&snapshot, trade.clone(), &StandardLossRule, Utc::now())?;
    Ok(Some(Guard {
        session,
        profile,
        authorization,
        trade,
        cap: decimal(cap)?,
        transport_started: false,
    }))
}
struct ProposalTerms {
    id: String,
    requested_volume: Decimal,
    cap: Decimal,
}
async fn proposal(
    b: &OpenApiBroker,
    p: &PropAccount,
    req: &EntryRequest<'_>,
    symbol: &InstrumentMetadata,
    terms: ProposalTerms,
    snapshot: &mut AccountSnapshot,
) -> Result<ProposedTrade> {
    let ProposalTerms {
        id,
        requested_volume: requested,
        cap,
    } = terms;
    let costs = p
        .costs
        .get(&symbol.name)
        .ok_or_else(|| eyre!("verified costs not configured for {}", symbol.name))?;
    let q = exposure::quote(b, symbol.id, p, &mut snapshot.quote_at).await?;
    let price = crate::entry::geometry::price(req, q).map_err(|e| eyre!("{e}"))?;
    let mut risk_price = decimal(price)?;
    if matches!(
        req.entry,
        trade_control_core::intent::ResolvedEntry::Market { .. }
    ) {
        let stop = decimal(req.stop_loss)?;
        let distance = minus(risk_price, stop)?.abs();
        let rounded = divide(
            times(distance, Decimal::from(100000))?.ceil(),
            Decimal::from(100000),
        )?;
        risk_price = if req.direction == Direction::Long {
            plus(stop, rounded)?
        } else {
            minus(stop, rounded)?
        };
    }
    let trader = metadata::trader(b).await?;
    let conversion = exposure::conversion(
        b,
        symbol.quote_asset,
        wire::integer(&trader, "depositAssetId")?,
        p,
        &mut snapshot.quote_at,
        false,
    )
    .await?;
    let estimate = exposure::estimate(
        if req.direction == Direction::Long {
            Side::Buy
        } else {
            Side::Sell
        },
        risk_price,
        decimal(req.stop_loss)?,
        conversion,
        costs,
        false,
    )?;
    // Existing worker/account cap and strategy risk request already bound requested
    // volume. The risk engine can reduce it further, never enlarge it.
    let account_cap = divide(times(snapshot.equity, cap)?, Decimal::from(100))?;
    let risk = match req.risk {
        trade_control_core::intent::RiskBudget::Percent(percent) => divide(
            times(snapshot.equity, decimal(percent)?)?,
            Decimal::from(100),
        )?,
        trade_control_core::intent::RiskBudget::Amount(amount) => decimal(amount)?,
        trade_control_core::intent::RiskBudget::Units(_) => {
            estimate.loss(requested, p.risk.execution.slippage_buffer_pct_of_risk)?
        }
    }
    .min(account_cap);
    Ok(ProposedTrade {
        instrument_id: symbol.id.to_string(),
        overlapping: costs.overlapping,
        request_id: id.clone(),
        strategy_id: "trade-control".into(),
        setup_id: req
            .setup_id
            .ok_or_else(|| eyre!("stable setup identity required for prop entry"))?
            .to_owned(),
        estimate,
        volume: VolumeLimits {
            min: divide(number(&symbol.raw, "minVolume")?, Decimal::from(100))?,
            max: divide(number(&symbol.raw, "maxVolume")?, Decimal::from(100))?,
            step: divide(number(&symbol.raw, "stepVolume")?, Decimal::from(100))?,
        },
        requested_volume: requested,
        requested_risk: risk,
    })
}
/// Read-only monitoring entry point: reconcile and latch stops even without a new signal.
/// Invoke from an account monitor; it never submits or liquidates positions.
pub async fn monitor(name: &str) -> Result<()> {
    let client_config = ctrader_api::Config::load(name)?;
    let Some(profile) = config::load(name, client_config.account_id, client_config.environment)?
    else {
        return Ok(());
    };
    let broker = OpenApiBroker::connect_config(name, client_config).await?;
    let mut session = profile.manager()?.begin()?;
    reconcile::collect(&broker, &profile, &mut session).await?;
    Ok(())
}
pub(crate) fn stop_tightens(source: &Value, new_stop: f64) -> Result<()> {
    let old = number(source, "stopLoss")?;
    let new = decimal(new_stop)?;
    let side = &source["tradeData"];
    ensure!(
        (wire::is_enum(side, "tradeSide", 1, "BUY") && new >= old)
            || (wire::is_enum(side, "tradeSide", 2, "SELL") && new <= old),
        "protected account stop widening requires a new risk authorization"
    );
    Ok(())
}
pub(crate) fn enabled(b: &OpenApiBroker) -> Result<bool> {
    Ok(config::load(
        &b.account_name,
        b.account_id(),
        b.shared.client.environment(),
    )?
    .is_some())
}

pub(crate) use adaptive_risk::decision::fresh as quote_fresh;

pub(crate) fn start_monitor(broker: &OpenApiBroker) -> Result<()> {
    let Some(profile) = config::load(
        &broker.account_name,
        broker.account_id(),
        broker.shared.client.environment(),
    )?
    else {
        return Ok(());
    };
    profile.validate()?;
    if broker
        .shared
        .risk_monitor_started
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return Ok(());
    }
    let shared = std::sync::Arc::downgrade(&broker.shared);
    let name = broker.account_name.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let Some(shared) = shared.upgrade() else {
                break;
            };
            if shared.client.is_closed() {
                break;
            }
            let broker = OpenApiBroker {
                shared,
                account_name: name.clone(),
            };
            let operation = async {
                let mut session = profile.manager()?.begin()?;
                reconcile::collect(&broker, &profile, &mut session).await?;
                Ok::<(), color_eyre::Report>(())
            }
            .await;
            if let Err(error) = operation {
                tracing::warn!(account=%name,%error,"prop account monitor did not reconcile; new entries require successful reconciliation");
            }
        }
    });
    Ok(())
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.transport_started
            && let Err(error) = self.session.rejected_without_fill(&self.authorization)
        {
            tracing::error!(request_id=%self.authorization.request_id(),%error,"pre-submission risk release failed; reservation remains conservative");
        }
    }
}
