//! Evidence for pending-entry distance rejections and bounded market recovery.
use super::{EntryError, EntryRequest, Quote};
use crate::intent::{Direction, EntryLevelVeto, ResolvedEntry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingEntryKind {
    Stop,
    Limit,
}

/// The broker's local validation snapshot, before any transmission.
#[derive(Debug, Clone, Copy)]
pub struct PendingEntryDistance {
    pub kind: PendingEntryKind,
    pub direction: Direction,
    pub trigger_price: f64,
    pub quote: Quote,
    pub minimum_distance: f64,
}

impl PendingEntryDistance {
    pub fn executable_price(self) -> f64 {
        match self.direction {
            Direction::Long => self.quote.ask,
            Direction::Short => self.quote.bid,
        }
    }

    pub fn signed_distance(self) -> f64 {
        let rising = matches!(
            (self.kind, self.direction),
            (PendingEntryKind::Stop, Direction::Long) | (PendingEntryKind::Limit, Direction::Short)
        );
        if rising {
            self.trigger_price - self.executable_price()
        } else {
            self.executable_price() - self.trigger_price
        }
    }

    pub fn is_valid(self) -> bool {
        self.trigger_price.is_finite()
            && self.trigger_price > 0.0
            && self.quote.bid.is_finite()
            && self.quote.bid > 0.0
            && self.quote.ask.is_finite()
            && self.quote.ask >= self.quote.bid
            && self.minimum_distance.is_finite()
            && self.minimum_distance >= 0.0
    }

    pub fn is_rejected(self) -> bool {
        self.is_valid()
            && (self.signed_distance() <= 0.0
                || self.signed_distance() + self.tolerance() < self.minimum_distance)
    }

    pub fn is_near(self) -> bool {
        self.is_valid()
            && self.minimum_distance > 0.0
            && (self.trigger_price - self.executable_price()).abs() + self.tolerance()
                < self.minimum_distance
    }

    pub fn matches(self, kind: PendingEntryKind, direction: Direction, trigger: f64) -> bool {
        self.is_near()
            && self.kind == kind
            && self.direction == direction
            && (self.trigger_price - trigger).abs() <= self.tolerance()
    }

    fn tolerance(self) -> f64 {
        self.trigger_price.abs().max(self.executable_price().abs()) * f64::EPSILON * 8.0
    }
}

impl core::fmt::Display for PendingEntryDistance {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:?} {:?} at {} blocked before submission: bid={} ask={} distance={} minimum={}",
            self.direction,
            self.kind,
            super::trim_float(self.trigger_price),
            super::trim_float(self.quote.bid),
            super::trim_float(self.quote.ask),
            format_args!("{:.8}", self.signed_distance()),
            super::trim_float(self.minimum_distance),
        )
    }
}

/// Absolute price bound for a third attempt. Check against the broker's own
/// fresh executable quote, after account reads and before sizing/submission.
#[derive(Debug, Clone)]
pub struct MarketEntryBand {
    pub trigger_price: f64,
    pub max_distance: f64,
    pub min_r: f64,
    pub entry_level_vetos: Vec<EntryLevelVeto>,
}

impl MarketEntryBand {
    pub fn check(&self, req: &EntryRequest<'_>, price: f64) -> Result<(), EntryError> {
        if !matches!(req.entry, ResolvedEntry::Market { .. }) {
            return Err(EntryError::OrderRejected);
        }
        if !self.trigger_price.is_finite()
            || self.trigger_price <= 0.0
            || !self.max_distance.is_finite()
            || self.max_distance <= 0.0
            || !self.min_r.is_finite()
            || self.min_r < crate::intent::MIN_R_FLOOR
            || !price.is_finite()
            || price <= 0.0
            || (price - self.trigger_price).abs() >= self.max_distance
        {
            return Err(EntryError::MarketRecoveryDeclined(format!(
                "fresh entry price {price} is outside the market fallback band: trigger={} maximum_distance={}",
                self.trigger_price, self.max_distance,
            )));
        }
        if let Some(veto) = self
            .entry_level_vetos
            .iter()
            .find(|veto| !veto.level.is_finite() || veto.is_past(price))
        {
            return Err(EntryError::MarketRecoveryDeclined(format!(
                "entry veto {} blocks market price {price} at level {}",
                veto.name, veto.level,
            )));
        }
        let (risk, reward) = match req.direction {
            Direction::Long => (price - req.stop_loss, req.take_profit - price),
            Direction::Short => (req.stop_loss - price, price - req.take_profit),
        };
        if !risk.is_finite() || !reward.is_finite() || risk <= 0.0 || reward / risk < self.min_r {
            return Err(EntryError::MarketRecoveryDeclined(format!(
                "fresh entry price {price} fails SL/TP geometry or minimum R {}: SL={} TP={}",
                self.min_r, req.stop_loss, req.take_profit,
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logging() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    }

    fn sample(
        kind: PendingEntryKind,
        direction: Direction,
        bid: f64,
        ask: f64,
    ) -> PendingEntryDistance {
        logging();
        PendingEntryDistance {
            kind,
            direction,
            trigger_price: 110.138,
            quote: Quote { bid, ask },
            minimum_distance: 0.010,
        }
    }

    #[test]
    fn the_audjpy_quote_blocks_both_pending_types_inside_the_same_band() {
        [PendingEntryKind::Stop, PendingEntryKind::Limit]
            .into_iter()
            .for_each(|kind| {
                let distance = sample(kind, Direction::Long, 110.123, 110.133);
                assert!(distance.is_rejected());
                assert!(distance.is_near());
            });
    }

    #[test]
    fn a_far_wrong_side_price_does_not_prove_proximity() {
        let distance = sample(PendingEntryKind::Stop, Direction::Long, 110.498, 110.502);
        assert!(distance.is_rejected());
        assert!(!distance.is_near());
    }

    #[test]
    fn an_exact_minimum_distance_is_legal_despite_float_rounding() {
        let distance = sample(PendingEntryKind::Stop, Direction::Long, 110.124, 110.128);
        assert!(!distance.is_rejected());
        assert!(!distance.is_near());
    }

    #[test]
    fn short_orders_use_bid_and_invalid_snapshots_do_not_authorize_market() {
        let distance = sample(PendingEntryKind::Limit, Direction::Short, 110.143, 110.147);
        assert!(distance.is_near());
        [f64::NAN, f64::INFINITY, -0.01, 0.0]
            .into_iter()
            .for_each(|minimum_distance| {
                assert!(
                    !PendingEntryDistance {
                        minimum_distance,
                        ..distance
                    }
                    .is_near()
                );
            });
    }

    fn market_request() -> EntryRequest<'static> {
        logging();
        EntryRequest {
            instrument: "AUDJPY",
            direction: Direction::Long,
            entry: ResolvedEntry::Market {
                reference_price: 110.133,
            },
            stop_loss: 109.720,
            take_profit: 110.999,
            risk: crate::intent::RiskBudget::Percent(1.0),
            dry_run: true,
            contract_multiplier: None,
        }
    }

    fn band() -> MarketEntryBand {
        MarketEntryBand {
            trigger_price: 110.138,
            max_distance: 0.010,
            min_r: 1.0,
            entry_level_vetos: vec![],
        }
    }

    #[test]
    fn bounded_market_checks_absolute_distance_on_both_sides_and_invalid_prices() {
        let req = market_request();
        assert!(band().check(&req, 110.133).is_ok());
        assert!(band().check(&req, 110.143).is_ok());
        [110.120, 110.156, f64::NAN, f64::INFINITY, -1.0, 0.0]
            .into_iter()
            .for_each(|price| assert!(band().check(&req, price).is_err()));
    }

    #[test]
    fn bounded_market_checks_geometry_and_r_at_the_current_quote() {
        let req = market_request();
        [
            EntryRequest {
                stop_loss: 110.134,
                ..market_request()
            },
            EntryRequest {
                take_profit: 110.134,
                ..market_request()
            },
            EntryRequest {
                stop_loss: f64::NAN,
                ..market_request()
            },
            EntryRequest {
                take_profit: f64::INFINITY,
                ..market_request()
            },
        ]
        .into_iter()
        .for_each(|invalid| assert!(band().check(&invalid, 110.133).is_err()));
        assert!(
            MarketEntryBand {
                min_r: 3.0,
                ..band()
            }
            .check(&req, 110.133)
            .is_err()
        );
        assert!(
            band()
                .check(
                    &EntryRequest {
                        entry: ResolvedEntry::Stop {
                            trigger_price: 110.138
                        },
                        ..req
                    },
                    110.133
                )
                .is_err()
        );
    }

    #[test]
    fn invalid_market_bands_fail_closed() {
        let req = market_request();
        [f64::NAN, f64::INFINITY, -0.01, 0.0]
            .into_iter()
            .for_each(|max_distance| {
                assert!(
                    MarketEntryBand {
                        max_distance,
                        ..band()
                    }
                    .check(&req, 110.133)
                    .is_err()
                );
            });
        assert!(
            MarketEntryBand {
                min_r: 0.0,
                ..band()
            }
            .check(&req, 110.133)
            .is_err()
        );
    }
}
