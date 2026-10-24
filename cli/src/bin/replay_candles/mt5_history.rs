//! Offline MT5 estimates never enter the genuine paired candle cache.
use super::granularity::ReplayGranularity;
use crate::replay_args::Mt5HistoryMode;
use candle_model::CandleData;
use chrono::{DateTime, Utc};
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};
use mt5_data_source::cached_feed::{CachedFeed, History, HistoryMode};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use trade_control_core::broker::BidAskCandle;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EstimatedSpread {
    pub profile: String,
    pub median_pips: f64,
    pub pip_size: f64,
}
impl EstimatedSpread {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.median_pips.is_finite() && self.median_pips > 0.0,
            "invalid saved replay spread"
        );
        ensure!(
            self.pip_size.is_finite() && self.pip_size > 0.0,
            "invalid saved replay pip size"
        );
        ensure!(
            (self.median_pips * self.pip_size).is_finite(),
            "replay spread overflow"
        );
        Ok(())
    }
    fn resolve(account: &str, symbol: &str, pip_size: f64) -> Result<Self> {
        ensure!(
            pip_size.is_finite() && pip_size > 0.0,
            "invalid replay pip size"
        );
        let median_pips = candle_cache::spread_profiles::mt5_median_pips(account, symbol)
            .ok_or_else(|| eyre!("no reviewed baked spread for MT5 account {account}, instrument {symbol}; cannot estimate ask/mid"))?;
        Ok(Self {
            profile: format!("mt5:{account}:{symbol}"),
            median_pips,
            pip_size,
        })
    }
    fn candle(&self, bid: &CandleData) -> BidAskCandle {
        let spread = self.median_pips * self.pip_size;
        BidAskCandle {
            time: bid.timestamp.with_timezone(&Utc),
            o: bid.open + spread / 2.0,
            h: bid.high + spread / 2.0,
            l: bid.low + spread / 2.0,
            c: bid.close + spread / 2.0,
            bid_o: bid.open,
            bid_h: bid.high,
            bid_l: bid.low,
            bid_c: bid.close,
            ask_o: bid.open + spread,
            ask_h: bid.high + spread,
            ask_l: bid.low + spread,
            ask_c: bid.close + spread,
        }
    }
    pub fn notice(&self) -> String {
        format!(
            "Price data: ESTIMATED ask/mid from native BID candles; baked 90-day median {:.3} pips ({}, pip size {}). This is a replay approximation, not the measured historical spread.\n",
            self.median_pips, self.profile, self.pip_size
        )
    }
}

pub struct Mt5Replay<'a> {
    account: Option<&'a str>,
    mode: Mt5HistoryMode,
    pip_size: f64,
    estimate: RefCell<Option<EstimatedSpread>>,
}
impl<'a> Mt5Replay<'a> {
    pub fn new(account: Option<&'a str>, mode: Mt5HistoryMode, pip_size: f64) -> Self {
        Self {
            account,
            mode,
            pip_size,
            estimate: RefCell::new(None),
        }
    }
    pub fn from_saved(
        account: Option<&'a str>,
        pip_size: f64,
        saved: Option<EstimatedSpread>,
    ) -> Self {
        let mode = if saved.is_some() {
            Mt5HistoryMode::Estimated
        } else {
            Mt5HistoryMode::Paired
        };
        Self {
            account,
            mode,
            pip_size,
            estimate: RefCell::new(saved),
        }
    }
    pub fn account(&self) -> Option<&str> {
        self.account
    }
    pub fn provenance(&self) -> Option<EstimatedSpread> {
        self.estimate.borrow().clone()
    }
    fn policy(&self) -> HistoryMode {
        match self.mode {
            Mt5HistoryMode::Paired => HistoryMode::Paired,
            Mt5HistoryMode::Estimated => HistoryMode::NativeBid,
            Mt5HistoryMode::Auto if self.estimate.borrow().is_some() => HistoryMode::NativeBid,
            Mt5HistoryMode::Auto => HistoryMode::PreferPaired,
        }
    }
    pub async fn pull(
        &self,
        symbol: &str,
        granularity: ReplayGranularity,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<BidAskCandle>> {
        let account = self.account.ok_or_else(|| {
            eyre!("--source mt5 requires --mt5-account or a plan with one named MT5 account")
        })?;
        let source = CachedFeed::new(mt5_data_source::account_config::AccountConfig::load(
            account,
        )?);
        let history = source
            .range(symbol, from, to, granularity.candle_model(), self.policy())
            .await;
        match history {
            Ok(History::Paired(data)) => Ok(data.candles.iter().map(super::candles::to_engine_candle).collect()),
            Ok(History::NativeBid(data)) => {
                let estimate = match self.provenance() {
                    Some(saved) => saved,
                    None => EstimatedSpread::resolve(account, symbol, self.pip_size)?,
                };
                ensure!(estimate.profile == format!("mt5:{account}:{symbol}"), "saved spread profile does not match replay account/instrument");
                estimate.validate()?;
                tracing::warn!(profile = %estimate.profile, spread_pips = estimate.median_pips, candles = data.candles.len(), "MT5 replay uses estimated ask/mid from native bid history; recent calibration is not the historical spread");
                let bars = data.candles.iter().map(|c| estimate.candle(c)).collect();
                *self.estimate.borrow_mut() = Some(estimate);
                Ok(bars)
            }
            Err(error) if mt5_data_source::is_history_unavailable(&error) => Err(error.wrap_err("genuine MT5 historical ask/mid unavailable; use --mt5-history estimated for a labelled replay approximation from cached bid candles")),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn init() {
        use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    }
    #[test]
    fn estimate_preserves_bid_and_shifts_ask_and_mid_by_baked_median() {
        init();
        let estimate = EstimatedSpread::resolve("the5ers-competition", "AUDCAD", 0.0001).unwrap();
        let bid = CandleData::new(
            1.0,
            1.2,
            0.9,
            1.1,
            100.0,
            "2007-03-04T21:00:00Z".parse().unwrap(),
        )
        .unwrap();
        let bar = estimate.candle(&bid);
        assert_eq!(
            (bar.bid_o, bar.bid_h, bar.bid_l, bar.bid_c),
            (1.0, 1.2, 0.9, 1.1)
        );
        let spread = estimate.median_pips * 0.0001;
        assert_eq!(bar.ask_h, bid.high + spread);
        assert_eq!(bar.c, bid.close + spread / 2.0);
        assert!(estimate.notice().contains("ESTIMATED"));
        assert!(
            estimate
                .notice()
                .contains("not the measured historical spread")
        );
    }
    #[test]
    fn missing_account_profile_or_invalid_pip_size_cannot_invent_books() {
        init();
        assert!(EstimatedSpread::resolve("other-supplier", "AUDCAD", 0.0001).is_err());
        assert!(EstimatedSpread::resolve("the5ers-competition", "UNKNOWN", 0.0001).is_err());
        assert!(EstimatedSpread::resolve("the5ers-competition", "AUDCAD", f64::NAN).is_err());
        let mut saved = EstimatedSpread::resolve("the5ers-competition", "AUDCAD", 0.0001).unwrap();
        saved.median_pips = -1.0;
        assert!(saved.validate().is_err());
        saved.median_pips = f64::MAX;
        saved.pip_size = f64::MAX;
        assert!(saved.validate().is_err());
    }
    #[test]
    fn saved_estimate_pins_profile_and_bypasses_ea_on_refetch() {
        init();
        let saved = EstimatedSpread::resolve("the5ers-competition", "AUDCAD", 0.0001).unwrap();
        let replay =
            Mt5Replay::from_saved(Some("the5ers-competition"), 0.0001, Some(saved.clone()));
        assert!(matches!(replay.policy(), HistoryMode::NativeBid));
        assert_eq!(replay.provenance(), Some(saved));
        assert!(matches!(
            Mt5Replay::new(None, Mt5HistoryMode::Paired, 0.0001).policy(),
            HistoryMode::Paired
        ));
        assert!(matches!(
            Mt5Replay::new(None, Mt5HistoryMode::Auto, 0.0001).policy(),
            HistoryMode::PreferPaired
        ));
    }
}
