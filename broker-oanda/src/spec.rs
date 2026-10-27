//! Current account order rules; baked definitions are not an execution override.
use oanda_client::instruments::InstrumentDetails;
use trade_control_core::broker::EntryError;

pub(crate) struct Spec {
    pub price_places: usize,
    pub unit_places: usize,
    pub minimum: f64,
    pub maximum: f64,
}

impl Spec {
    pub fn from_details(details: &InstrumentDetails) -> Result<Self, EntryError> {
        let parse = |value: &str| value.parse::<f64>().map_err(|_| EntryError::OrderRejected);
        let minimum = parse(&details.minimum_trade_size)?;
        let maximum = parse(&details.maximum_order_units)?;
        if !(0..=8).contains(&details.display_precision)
            || !(0..=8).contains(&details.trade_units_precision)
            || !minimum.is_finite()
            || minimum <= 0.0
            || !maximum.is_finite()
            || maximum < minimum
        {
            return Err(EntryError::OrderRejected);
        }
        Ok(Self {
            price_places: details.display_precision as usize,
            unit_places: details.trade_units_precision as usize,
            minimum,
            maximum,
        })
    }

    pub fn price(&self, value: f64) -> String {
        format!("{value:.places$}", places = self.price_places)
    }

    pub fn units(&self, raw: f64) -> Result<f64, EntryError> {
        let scale = 10f64.powi(self.unit_places as i32);
        let units = (raw * scale).floor() / scale;
        if !units.is_finite() || units < self.minimum {
            return Err(EntryError::UnitsBelowMinimum);
        }
        if units > self.maximum {
            return Err(EntryError::OrderRejectedWithDetail(
                "Size exceeds OANDA maximumOrderUnits".into(),
            ));
        }
        Ok(units)
    }

    pub fn signed_units(&self, units: f64, long: bool) -> String {
        let signed = if long { units } else { -units };
        format!("{signed:.places$}", places = self.unit_places)
    }
}

#[cfg(test)]
mod tests;
