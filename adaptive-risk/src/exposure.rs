use crate::{Decimal, add, div, mul, pct, sub};
use color_eyre::eyre::{Result, ensure, eyre};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}
/// Price-to-account-value terms per broker volume unit. For tick-valued contracts,
/// contract_size = tick_value / tick_size; conversion is quote -> account currency.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LossEstimate {
    pub side: Side,
    /// Entry for new/pending orders; CURRENT liquidation mark for open positions.
    pub price: Decimal,
    pub stop: Option<Decimal>,
    pub contract_size: Decimal,
    pub conversion: Decimal,
    /// Only future/unreflected charges: do not include fees already in equity.
    pub costs_per_unit: Decimal,
    pub fixed_costs: Decimal,
    pub is_open_position: bool,
}
impl LossEstimate {
    pub fn unit_loss(&self, slippage_pct: Decimal) -> Result<Decimal> {
        let stop = self.stop.ok_or_else(|| eyre!("protective stop missing"))?;
        ensure!(
            self.price > Decimal::ZERO
                && stop > Decimal::ZERO
                && self.contract_size > Decimal::ZERO
                && self.conversion > Decimal::ZERO
                && self.costs_per_unit >= Decimal::ZERO
                && self.fixed_costs >= Decimal::ZERO,
            "invalid loss estimate"
        );
        let distance = match self.side {
            Side::Buy => sub(self.price, stop)?,
            Side::Sell => sub(stop, self.price)?,
        };
        ensure!(
            self.is_open_position || distance > Decimal::ZERO,
            "stop on wrong side of entry"
        );
        let price_risk = mul(
            mul(distance.max(Decimal::ZERO), self.contract_size)?,
            self.conversion,
        )?;
        // Open positions still require a nonzero gap allowance even when the stop is in profit.
        let slip_basis = price_risk.max(
            mul(mul(self.price, self.contract_size)?, self.conversion)? / Decimal::from(10000),
        );
        add(
            add(price_risk, self.costs_per_unit)?,
            pct(slip_basis, slippage_pct)?,
        )
    }
    pub fn loss(&self, volume: Decimal, slippage_pct: Decimal) -> Result<Decimal> {
        ensure!(volume >= Decimal::ZERO, "negative volume");
        add(
            mul(self.unit_loss(slippage_pct)?, volume)?,
            self.fixed_costs,
        )
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VolumeLimits {
    pub min: Decimal,
    pub max: Decimal,
    pub step: Decimal,
}
impl VolumeLimits {
    pub fn size(
        &self,
        budget: Decimal,
        estimate: &LossEstimate,
        slip: Decimal,
        requested: Decimal,
    ) -> Result<Decimal> {
        ensure!(
            self.min > Decimal::ZERO
                && self.max >= self.min
                && self.step > Decimal::ZERO
                && requested > Decimal::ZERO,
            "invalid volume limits"
        );
        let available = sub(budget, estimate.fixed_costs)?.max(Decimal::ZERO);
        let raw = div(available, estimate.unit_loss(slip)?)?
            .min(self.max)
            .min(requested);
        let volume = mul(div(raw, self.step)?.floor(), self.step)?;
        ensure!(volume >= self.min, "volume below broker minimum");
        ensure!(
            estimate.loss(volume, slip)? <= budget,
            "rounded risk exceeds budget"
        );
        Ok(volume)
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Exposure {
    pub broker_id: String,
    /// Links all partially filled position and pending fragments to the same request.
    pub request_id: Option<String>,
    pub volume: Decimal,
    pub estimate: LossEstimate,
    pub overlapping: bool,
}
impl Exposure {
    pub fn risk(&self, slip: Decimal, overlap: Decimal) -> Result<Decimal> {
        ensure!(
            self.volume > Decimal::ZERO && !self.broker_id.is_empty(),
            "invalid exposure identity/volume"
        );
        mul(
            self.estimate.loss(self.volume, slip)?,
            if self.overlapping {
                overlap
            } else {
                Decimal::ONE
            },
        )
    }
}
