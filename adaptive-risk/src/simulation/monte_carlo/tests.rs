use super::*;
use tracing_subscriber::prelude::*;
#[test]
fn circular_blocks_do_not_overweight_journal_endpoints() -> Result<()> {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let c = MonteCarloConfig {
        trades: 100,
        block_size: 2,
        ..Default::default()
    };
    let records = vec![
        TradeRecord {
            at: c.start,
            net_r: Decimal::NEGATIVE_ONE,
        },
        TradeRecord {
            at: c.start + Duration::hours(1),
            net_r: Decimal::from(2),
        },
    ];
    let sampled = sample(
        &c,
        &RiskPolicy::default(),
        &mut ChaCha8Rng::seed_from_u64(42),
        Some(&records),
    )?;
    assert_eq!(
        sampled.iter().filter(|r| r.net_r > Decimal::ZERO).count(),
        50
    );
    Ok(())
}
