use crate::*;
use color_eyre::Result;
#[derive(Default)]
pub(super) struct Allocation {
    pub nominal: Decimal,
    pub reserved: Decimal,
}
pub(super) fn proposed(
    reference: Decimal,
    base_pct: Decimal,
    sequence: u64,
) -> Result<ProposedTrade> {
    Ok(ProposedTrade {
        instrument_id: "simulation".into(),
        overlapping: false,
        request_id: format!("sim-{sequence}"),
        strategy_id: "simulation".into(),
        setup_id: format!("setup-{sequence}"),
        estimate: LossEstimate {
            side: Side::Buy,
            price: Decimal::from(100),
            stop: Some(Decimal::from(99)),
            contract_size: Decimal::ONE,
            conversion: Decimal::ONE,
            costs_per_unit: Decimal::ZERO,
            fixed_costs: Decimal::ZERO,
            is_open_position: false,
        },
        volume: VolumeLimits {
            min: Decimal::new(1, 6),
            max: Decimal::from(1000000000),
            step: Decimal::new(1, 6),
        },
        requested_volume: Decimal::from(1000000000),
        requested_risk: pct(reference, base_pct)?,
    })
}
pub(super) fn at_volume(t: &ProposedTrade, volume: Decimal, p: &RiskPolicy) -> Result<Allocation> {
    // Synthetic contract loses exactly one account-currency unit per volume at
    // its initial stop. Historical NET R uses this nominal stop-risk denominator.
    Ok(Allocation {
        nominal: volume,
        reserved: t
            .estimate
            .loss(volume, p.execution.slippage_buffer_pct_of_risk)?,
    })
}
pub(super) fn size(t: &ProposedTrade, budget: Decimal, p: &RiskPolicy) -> Result<Allocation> {
    if budget
        < t.estimate
            .loss(t.volume.min, p.execution.slippage_buffer_pct_of_risk)?
    {
        return Ok(Allocation::default());
    }
    let volume = t.volume.size(
        budget,
        &t.estimate,
        p.execution.slippage_buffer_pct_of_risk,
        t.requested_volume,
    )?;
    at_volume(t, volume, p)
}
