use adaptive_risk::{simulation::*, *};
use chrono::{DateTime, Duration, Utc};
use color_eyre::Result;
use tracing_subscriber::prelude::*;
fn init() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}
fn at() -> DateTime<Utc> {
    "2026-01-05T22:00:00Z".parse().unwrap()
}
#[test]
fn chronological_replay_requires_dataset_provenance_and_orders() -> Result<()> {
    init();
    let policy = RiskPolicy::default();
    let trades = vec![
        TradeRecord {
            at: at(),
            net_r: Decimal::NEGATIVE_ONE,
        },
        TradeRecord {
            at: at() + Duration::hours(1),
            net_r: Decimal::NEGATIVE_ONE,
        },
        TradeRecord {
            at: at() + Duration::hours(2),
            net_r: Decimal::from(2),
        },
    ];
    let result = compare(
        &policy,
        &trades,
        Dataset::Replay,
        Decimal::from(10000),
        Decimal::from(10),
    )?;
    assert_eq!(result.len(), 4);
    assert!(result[0].report.max_drawdown_pct > result[1].report.max_drawdown_pct);
    assert!(result[0].report.average_risk_pct > result[1].report.average_risk_pct);
    let mut reversed = trades;
    reversed.reverse();
    assert!(
        compare(
            &policy,
            &reversed,
            Dataset::Replay,
            Decimal::from(10000),
            Decimal::from(10)
        )
        .is_err()
    );
    Ok(())
}
#[test]
fn seeds_reproduce_monte_carlo_and_model_timestamps() -> Result<()> {
    init();
    let config = MonteCarloConfig {
        simulations: 20,
        trades: 12,
        seed: 99,
        trades_per_day: vec![0, 2, 4],
        block_size: 3,
        ..Default::default()
    };
    let first = monte_carlo(&RiskPolicy::default(), &config, None)?;
    let second = monte_carlo(&RiskPolicy::default(), &config, None)?;
    assert_eq!(
        serde_json::to_value(&first)?,
        serde_json::to_value(&second)?
    );
    assert!(
        first
            .iter()
            .all(|s| s.funding_probability.lower >= 0.0 && s.funding_probability.upper <= 1.0)
    );
    let empty = MonteCarloConfig {
        trades_per_day: vec![0],
        ..config.clone()
    };
    assert!(monte_carlo(&RiskPolicy::default(), &empty, None).is_err());
    Ok(())
}

#[test]
fn realised_gap_losses_latch_daily_stop_and_resume_at_next_boundary() -> Result<()> {
    init();
    let trades = vec![
        TradeRecord {
            at: at(),
            net_r: Decimal::from(-4),
        },
        TradeRecord {
            at: at() + Duration::hours(1),
            net_r: Decimal::from(2),
        },
        TradeRecord {
            at: at() + Duration::days(1),
            net_r: Decimal::from(2),
        },
    ];
    let reports = compare(
        &RiskPolicy::default(),
        &trades,
        Dataset::SyntheticProvisional,
        Decimal::from(10000),
        Decimal::from(10),
    )?;
    let combined = &reports[3].report;
    assert_eq!(combined.skipped_trades, 1);
    assert_eq!(combined.daily_lockouts, 1);
    assert_eq!(combined.account_limit_breaches, 0);
    assert!(combined.average_risk_pct < reports[0].report.average_risk_pct);
    Ok(())
}
#[test]
fn fixed_replay_reports_known_net_return_and_actual_dataset_provenance() -> Result<()> {
    init();
    let trades = vec![
        TradeRecord {
            at: at(),
            net_r: Decimal::from(2),
        },
        TradeRecord {
            at: at() + Duration::hours(1),
            net_r: Decimal::NEGATIVE_ONE,
        },
    ];
    let mut policy = RiskPolicy::default();
    policy.execution.slippage_buffer_pct_of_risk = Decimal::ZERO;
    let reports = compare(
        &policy,
        &trades,
        Dataset::Live,
        Decimal::from(10000),
        Decimal::from(10),
    )?;
    assert!(reports.iter().all(|r| r.dataset == Dataset::Live));
    assert_eq!(reports[0].report.net_return_pct, Decimal::ONE);
    assert_eq!(reports[0].report.longest_losing_streak, 1);
    assert_eq!(reports[0].report.average_risk_pct, Decimal::ONE);
    Ok(())
}
#[test]
fn last_trade_of_day_or_horizon_counts_the_lockout_immediately() -> Result<()> {
    init();
    let first = TradeRecord {
        at: at(),
        net_r: Decimal::from(-4),
    };
    for records in [
        vec![first.clone()],
        vec![
            first.clone(),
            TradeRecord {
                at: at() + Duration::days(1),
                net_r: Decimal::from(2),
            },
        ],
    ] {
        let reports = compare(
            &RiskPolicy::default(),
            &records,
            Dataset::SyntheticProvisional,
            Decimal::from(10000),
            Decimal::from(10),
        )?;
        assert_eq!(reports[3].report.daily_lockouts, 1);
    }
    Ok(())
}
#[test]
fn realised_r_uses_approved_nominal_volume_and_leaves_reserve_unspent() -> Result<()> {
    init();
    let reports = compare(
        &RiskPolicy::default(),
        &[TradeRecord {
            at: at(),
            net_r: Decimal::NEGATIVE_ONE,
        }],
        Dataset::Replay,
        Decimal::from(10000),
        Decimal::from(10),
    )?;
    for result in reports {
        assert!(
            (result.report.net_return_pct + "0.83333333".parse::<Decimal>()?).abs()
                < Decimal::new(1, 6)
        );
        assert!(result.report.average_reserved_risk_pct > result.report.average_risk_pct);
    }
    Ok(())
}
