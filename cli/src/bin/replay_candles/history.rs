//! Account-pinned replay readers; cTrader always requires genuine historical books.
use super::{
    granularity::ReplayGranularity,
    mt5_history::{EstimatedSpread, Mt5Replay},
};
use candle_cache::ctrader::CachedFeed;
use chrono::{DateTime, Utc};
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};
use std::{collections::BTreeSet, sync::Arc};
use tokio::sync::OnceCell;
use trade_control_cli::replay_args::{CandleSource, Mt5HistoryMode};
use trade_control_core::intent::BrokerKind;
use trade_control_engine::{BidAskCandle, TradePlan};

pub struct SourceHistory<'a> {
    source: CandleSource,
    mt5: Mt5Replay<'a>,
    ctrader_account: Option<&'a str>,
    ctrader: OnceCell<Arc<CachedFeed>>,
}
impl<'a> SourceHistory<'a> {
    pub fn new(
        source: CandleSource,
        mt5_account: Option<&'a str>,
        ctrader_account: Option<&'a str>,
        mode: Mt5HistoryMode,
        pip_size: f64,
    ) -> Self {
        Self {
            source,
            mt5: Mt5Replay::new(mt5_account, mode, pip_size),
            ctrader_account,
            ctrader: OnceCell::new(),
        }
    }
    pub fn from_saved(
        source: CandleSource,
        account: Option<&'a str>,
        pip_size: f64,
        saved: Option<EstimatedSpread>,
    ) -> Result<Self> {
        ensure!(
            source == CandleSource::Mt5 || saved.is_none(),
            "estimated spread provenance is valid only for MT5 replay; cTrader requires genuine paired history"
        );
        Ok(Self {
            source,
            mt5: Mt5Replay::from_saved(account, pip_size, saved),
            ctrader_account: (source == CandleSource::Ctrader)
                .then_some(account)
                .flatten(),
            ctrader: OnceCell::new(),
        })
    }
    pub fn provenance(&self) -> Option<EstimatedSpread> {
        if self.source == CandleSource::Mt5 {
            self.mt5.provenance()
        } else {
            None
        }
    }
    pub fn account(&self) -> Option<&str> {
        match self.source {
            CandleSource::Ctrader => self.ctrader_account,
            CandleSource::Mt5 => self.mt5.account(),
            _ => None,
        }
    }
    pub async fn pull(
        &self,
        symbol: &str,
        granularity: ReplayGranularity,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<BidAskCandle>> {
        if self.source == CandleSource::Mt5 {
            return self.mt5.pull(symbol, granularity, from, to).await;
        }
        ensure!(
            self.source == CandleSource::Ctrader,
            "account history reader used for another source"
        );
        let account = self.ctrader_account.ok_or_else(|| eyre!("--source ctrader requires --ctrader-account or one unique named cTrader plan account"))?;
        let feed = self
            .ctrader
            .get_or_try_init(|| async {
                Ok::<_, color_eyre::Report>(Arc::new(
                    broker_ctrader::market_data::ChartData::configured_feed(account)?,
                ))
            })
            .await?;
        let bars = feed
            .range_bid_ask(
                symbol,
                from.fixed_offset(),
                to.fixed_offset(),
                granularity.candle_model(),
            )
            .await?;
        Ok(bars
            .candles
            .iter()
            .map(super::candles::to_engine_candle)
            .collect())
    }
}

/// Saved fixture provenance pins refetches; a conflicting flag cannot replace it.
pub fn resolve_account(
    source: CandleSource,
    explicit: Option<&str>,
    saved: Option<&str>,
    plan: &TradePlan,
) -> Result<Option<String>> {
    let kind = match source {
        CandleSource::Mt5 => BrokerKind::Mt5,
        CandleSource::Ctrader => BrokerKind::Ctrader,
        _ => return Ok(None),
    };
    explicit
        .into_iter()
        .chain(saved)
        .try_for_each(valid_account)?;
    if let Some(saved) = saved {
        ensure!(
            explicit.is_none_or(|name| name == saved),
            "replay account override conflicts with saved fixture account {saved}"
        );
        return Ok(Some(saved.into()));
    }
    if let Some(explicit) = explicit {
        return Ok(Some(explicit.into()));
    }
    infer_named(
        plan.rules
            .iter()
            .filter(|rule| rule.intent.broker == kind)
            .map(|rule| rule.intent.account.as_deref()),
    )
}
fn valid_account(account: &str) -> Result<()> {
    ensure!(
        !account.is_empty() && account.trim() == account,
        "replay account must be non-empty without surrounding whitespace"
    );
    Ok(())
}
fn infer_named<'a>(accounts: impl Iterator<Item = Option<&'a str>>) -> Result<Option<String>> {
    let accounts = accounts.map(|account| {
        let account = account.ok_or_else(|| eyre!("cannot infer replay account from an unnamed broker rule; specify the account explicitly"))?;
        valid_account(account)?;
        Ok(account)
    }).collect::<Result<BTreeSet<_>>>()?;
    ensure!(
        accounts.len() <= 1,
        "plan contains multiple broker accounts; specify the replay account explicitly"
    );
    Ok(accounts.into_iter().next().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn init_tracing() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    }
    #[test]
    fn inference_preserves_full_names_and_rejects_ambiguity() {
        init_tracing();
        assert_eq!(
            infer_named([Some("ctrader-demo"), Some("ctrader-demo")].into_iter())
                .unwrap()
                .as_deref(),
            Some("ctrader-demo")
        );
        assert!(infer_named([Some("ctrader-demo"), Some("another-broker")].into_iter()).is_err());
        assert!(infer_named([Some("ctrader-demo"), None].into_iter()).is_err());
        assert_eq!(infer_named(std::iter::empty()).unwrap(), None);
    }
    #[test]
    fn plan_inference_reads_every_rule_for_the_selected_broker() {
        init_tracing();
        let rule = |id: &str, broker: &str, account: &str| {
            serde_json::json!({
                "rule_id": id, "trigger": {"type":"time_reached","at_epoch":1}, "fire_mode":"once",
                "intent": {"v":1,"id":id,"not_after":"2027-01-01T00:00:00Z","action":"veto","instrument":"EURUSD","broker":broker,"account":account}
            })
        };
        let mut plan: TradePlan = serde_json::from_value(serde_json::json!({
            "trade_id":"test","instrument":"EURUSD","direction":"long","granularity":"h1","pip_size":0.0001,
            "rules":[rule("foreign","mt5","foreign-account"),rule("selected","ctrader","ctrader-demo")]
        })).unwrap();
        assert_eq!(
            resolve_account(CandleSource::Ctrader, None, None, &plan)
                .unwrap()
                .as_deref(),
            Some("ctrader-demo")
        );
        plan.rules
            .push(serde_json::from_value(rule("other", "ctrader", "another-account")).unwrap());
        assert!(resolve_account(CandleSource::Ctrader, None, None, &plan).is_err());
    }

    #[test]
    fn saved_fixture_account_pins_refetch_and_rejects_conflicting_override() {
        init_tracing();
        let plan: TradePlan = serde_json::from_str(r#"{"trade_id":"test","instrument":"EURUSD","direction":"long","granularity":"h1","pip_size":0.0001,"rules":[]}"#).unwrap();
        assert_eq!(
            resolve_account(CandleSource::Ctrader, None, Some("ctrader-demo"), &plan)
                .unwrap()
                .as_deref(),
            Some("ctrader-demo")
        );
        assert!(
            resolve_account(
                CandleSource::Ctrader,
                Some("wrong-account"),
                Some("ctrader-demo"),
                &plan
            )
            .is_err()
        );
        assert_eq!(
            resolve_account(
                CandleSource::Ctrader,
                Some("ctrader-oanda-demo"),
                None,
                &plan
            )
            .unwrap()
            .as_deref(),
            Some("ctrader-oanda-demo")
        );
    }
    #[test]
    fn c_trader_never_accepts_estimated_history() {
        init_tracing();
        let estimate = EstimatedSpread {
            profile: "mt5:demo:EURUSD".into(),
            median_pips: 1.0,
            pip_size: 0.0001,
        };
        assert!(
            SourceHistory::from_saved(
                CandleSource::Ctrader,
                Some("ctrader-demo"),
                0.0001,
                Some(estimate)
            )
            .is_err()
        );
        let history = SourceHistory::new(
            CandleSource::Ctrader,
            None,
            Some("ctrader-demo"),
            Mt5HistoryMode::Estimated,
            0.0001,
        );
        assert_eq!(history.account(), Some("ctrader-demo"));
        assert_eq!(history.provenance(), None);
    }
}
