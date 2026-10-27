//! A third market attempt requires positive proximity evidence from BOTH
//! preceding stop and limit attempts. Generic wrong-side errors cannot opt in.
use crate::broker::{
    Broker, EntryError, EntryRequest, MarketEntryBand, PendingEntryDistance, PendingEntryKind,
    Placement,
};
use crate::intent::{EntryLevelVeto, RecoverEntryAction, Resolved, ResolvedEntry};

pub(super) struct RecoveredEntry {
    pub placement: Placement,
    pub entry: ResolvedEntry,
}

pub(super) async fn recover<B: Broker>(
    broker: &B,
    resolved: &Resolved,
    stop: PendingEntryDistance,
    entry_level_vetos: &[EntryLevelVeto],
    cap: f64,
    positions: u32,
    detail: &mut String,
) -> Result<RecoveredEntry, EntryError> {
    *detail = format!("stop rejected: {stop}");
    let trigger = resolved.entry.reference_price();
    if !matches!(resolved.entry, ResolvedEntry::Stop { .. })
        || !resolved
            .recover_entry
            .is_some_and(|r| r.action == RecoverEntryAction::Limit)
        || !stop.matches(PendingEntryKind::Stop, resolved.direction, trigger)
    {
        detail.push_str("; market not attempted: stop rejection does not establish proximity");
        return Err(EntryError::EntryWithinMinimumDistance(stop));
    }

    let limit_entry = ResolvedEntry::Limit {
        trigger_price: trigger,
    };
    let limit = match broker
        .place_entry(cap, positions, &request(resolved, limit_entry.clone()))
        .await
    {
        Ok(placement) => {
            detail.push_str(if resolved.dry_run {
                "; limit preview passed; market not attempted; no order submitted"
            } else {
                "; limit accepted; market not attempted"
            });
            return Ok(RecoveredEntry {
                placement,
                entry: limit_entry,
            });
        }
        Err(EntryError::EntryWithinMinimumDistance(limit)) => {
            detail.push_str(&format!("; limit rejected: {limit}"));
            limit
        }
        Err(error) => {
            detail.push_str(&format!(
                "; limit failed: {error}; market not attempted: failure is not verified proximity"
            ));
            return Err(error);
        }
    };
    if !limit.matches(PendingEntryKind::Limit, resolved.direction, trigger) {
        detail.push_str("; market not attempted: limit rejection does not establish proximity");
        return Err(EntryError::EntryWithinMinimumDistance(limit));
    }

    let band = market_band(resolved, stop, limit, entry_level_vetos).inspect_err(|error| {
        detail.push_str(&format!("; market not attempted: {error}"));
    })?;
    place_market(broker, resolved, band, cap, positions, detail).await
}

fn market_band(
    resolved: &Resolved,
    stop: PendingEntryDistance,
    limit: PendingEntryDistance,
    entry_level_vetos: &[EntryLevelVeto],
) -> Result<MarketEntryBand, EntryError> {
    let max_distance = stop.minimum_distance.min(limit.minimum_distance);
    let max_distance = match resolved.recover_entry.and_then(|r| r.max_slippage_price) {
        Some(bound) if bound.is_finite() && bound > 0.0 => bound.min(max_distance),
        Some(_) => {
            return Err(EntryError::MarketRecoveryDeclined(
                "invalid explicit slippage allowance".into(),
            ));
        }
        None => max_distance,
    };
    Ok(MarketEntryBand {
        trigger_price: resolved.entry.reference_price(),
        max_distance,
        min_r: resolved.min_r,
        entry_level_vetos: entry_level_vetos.to_vec(),
    })
}

async fn place_market<B: Broker>(
    broker: &B,
    resolved: &Resolved,
    band: MarketEntryBand,
    cap: f64,
    positions: u32,
    detail: &mut String,
) -> Result<RecoveredEntry, EntryError> {
    let quote = broker
        .get_quote(&resolved.instrument)
        .await
        .map_err(|error| {
            detail.push_str(&format!(
                "; market not attempted: fresh quote unavailable ({error})"
            ));
            EntryError::AccountFetch
        })?;
    if !quote.bid.is_finite() || quote.bid <= 0.0 || !quote.ask.is_finite() || quote.ask < quote.bid
    {
        detail.push_str("; market not attempted: fresh bid/ask quote is invalid");
        return Err(EntryError::MarketRecoveryDeclined(
            "invalid executable quote".into(),
        ));
    }
    let reference_price = match resolved.direction {
        crate::intent::Direction::Long => quote.ask,
        crate::intent::Direction::Short => quote.bid,
    };
    let market_entry = ResolvedEntry::Market { reference_price };
    let market_request = request(resolved, market_entry);
    band.check(&market_request, reference_price)
        .map_err(|error| {
            detail.push_str(&format!("; market not attempted: {error}"));
            error
        })?;
    detail.push_str(&format!(
        "; market fallback: fresh bid={} ask={} trigger={} maximum_distance={}",
        quote.bid, quote.ask, band.trigger_price, band.max_distance,
    ));
    tracing::info!(id = resolved.id, %detail, "pending entries too close; checking bounded market fallback");
    let placement = broker
        .place_market_entry_in_band(cap, positions, &market_request, band)
        .await
        .inspect_err(|error| {
            detail.push_str(&format!("; market result: {error}"));
            tracing::warn!(id = resolved.id, %error, %detail, "market fallback was not confirmed");
        })?;
    detail.push_str(if resolved.dry_run {
        "; market preview passed; no order submitted"
    } else {
        "; market entry accepted"
    });
    let entry = ResolvedEntry::Market {
        reference_price: placement.price.unwrap_or(reference_price),
    };
    Ok(RecoveredEntry { placement, entry })
}

fn request(resolved: &Resolved, entry: ResolvedEntry) -> EntryRequest<'_> {
    EntryRequest {
        setup_id: Some(&resolved.setup_id),
        instrument: &resolved.instrument,
        direction: resolved.direction,
        entry,
        stop_loss: resolved.stop_loss,
        take_profit: resolved.take_profit,
        risk: resolved.risk,
        dry_run: resolved.dry_run,
        contract_multiplier: resolved.contract_multiplier,
    }
}

pub(super) fn describe(outcome: String, detail: &str) -> String {
    if detail.is_empty() {
        outcome
    } else {
        format!("{outcome}; recovery: {detail}")
    }
}
