use broker_ctrader::InstrumentMetadata;
use color_eyre::{Result, eyre::ensure};
use trade_control_core::{
    broker::{EntryRequest, Quote},
    intent::{Direction, ResolvedEntry, RiskBudget},
};

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Stop,
    Limit,
    Market,
}
impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Limit => "limit",
            Self::Market => "market",
        }
    }
}

pub fn request<'a>(
    meta: &'a InstrumentMetadata,
    quote: Quote,
    kind: Kind,
    distance: f64,
) -> Result<EntryRequest<'a>> {
    ensure!(
        meta.min_units.is_finite()
            && meta.min_units > 0.0
            && meta.min_units <= meta.max_units
            && meta.step_units.is_finite()
            && meta.step_units > 0.0,
        "invalid native volume limits"
    );
    let (price, stop_loss, take_profit) =
        levels(quote, kind, distance, meta.pip_size, meta.digits)?;
    let entry = match kind {
        Kind::Stop => ResolvedEntry::Stop {
            trigger_price: price,
        },
        Kind::Limit => ResolvedEntry::Limit {
            trigger_price: price,
        },
        Kind::Market => ResolvedEntry::Market {
            reference_price: price,
        },
    };
    Ok(EntryRequest {
        instrument: &meta.name,
        direction: Direction::Long,
        entry,
        stop_loss,
        take_profit,
        risk: RiskBudget::Units(meta.min_units),
        dry_run: false,
        contract_multiplier: None,
    })
}

fn levels(
    quote: Quote,
    kind: Kind,
    distance: f64,
    pip: f64,
    digits: u32,
) -> Result<(f64, f64, f64)> {
    ensure!(
        quote.bid.is_finite() && quote.ask.is_finite() && quote.bid > 0.0 && quote.ask >= quote.bid,
        "invalid current quote"
    );
    ensure!(
        pip.is_finite() && pip > 0.0 && digits <= 8,
        "invalid native precision"
    );
    ensure!(
        distance.is_finite() && distance >= 100.0,
        "pending orders must be at least 100 pips away"
    );
    let grid = 10f64.powi(digits as i32);
    let snap = |v: f64| (v * grid).round() / grid;
    let price = snap(match kind {
        Kind::Stop => quote.ask + distance * pip,
        Kind::Limit => quote.bid - distance * pip,
        Kind::Market => quote.ask,
    });
    let stop = snap(price - 100.0 * pip);
    let target = snap(price + 150.0 * pip);
    ensure!(
        stop > 0.0 && stop < price && target > price,
        "probe bracket is invalid on native price grid"
    );
    Ok((price, stop, target))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_pips_and_digits_keep_pending_probes_far_away() {
        crate::test_logging();
        let q = Quote {
            bid: 1.12345,
            ask: 1.12349,
        };
        assert_eq!(
            levels(q, Kind::Stop, 500.0, 0.0001, 5).unwrap(),
            (1.17349, 1.16349, 1.18849)
        );
        assert_eq!(
            levels(q, Kind::Limit, 500.0, 0.0001, 5).unwrap(),
            (1.07345, 1.06345, 1.08845)
        );
        assert!(levels(q, Kind::Stop, 0.0, 0.0001, 5).is_err());
        assert!(levels(Quote { bid: 2.0, ask: 1.0 }, Kind::Market, 500.0, 0.0001, 5).is_err());
    }
}
