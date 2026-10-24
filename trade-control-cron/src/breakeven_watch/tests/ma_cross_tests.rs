use super::*;
use trade_control_core::intent::BreakevenTrigger;

fn cross_path() -> Vec<Candle> {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let start = ts("2026-06-01T00:00:00Z");
    (0..117)
        .map(|i| {
            let c = if i < 110 {
                100.0 - i as f64 * 0.1
            } else {
                89.0 + (i - 110) as f64
            };
            Candle {
                time: start + Duration::hours(i),
                o: c,
                h: c + 0.1,
                l: c - 0.1,
                c,
            }
        })
        .collect()
}
fn cross_attempt() -> EntryAttempt {
    attempt_with_be(
        "EUR_USD",
        Direction::Short,
        None,
        Some("POS-REAL"),
        Some(BreakevenSnapshot {
            rule: Breakeven {
                trigger: BreakevenTrigger::MaCrossOnly,
                ..Default::default()
            },
            entry_price: 101.0,
            take_profit: 60.0,
            granularity: Granularity::H1,
        }),
    )
}
fn cross_position(path: &[Candle]) -> OpenPosition {
    OpenPosition {
        entry_price: Some(100.0),
        opened_at: Some(path[105].time),
        stop_loss: Some(110.0),
        ..alias_position()
    }
}
fn run_cross(broker: &SpyBroker, position: &OpenPosition, path: &[Candle]) {
    pollster::block_on(watch_one(
        broker,
        None,
        &[cross_attempt()],
        position,
        path.last().unwrap().time + Duration::hours(1),
    ));
}
#[test]
fn ma_cross_live_moves_to_actual_fill_without_percentage_progress() {
    let path = cross_path();
    let broker = SpyBroker::with(path.clone());
    run_cross(&broker, &cross_position(&path), &path);
    assert_eq!(*broker.amends.borrow(), vec![100.0]);
}
#[test]
fn ma_cross_live_refuses_losing_quote_better_stop_and_missing_fill_data() {
    let path = cross_path();
    for quote in [Some(101.0), Some(100.0), None] {
        let mut broker = SpyBroker::with(path.clone());
        broker.exit_price = quote;
        run_cross(&broker, &cross_position(&path), &path);
        assert!(broker.amends.borrow().is_empty());
    }
    for position in [
        OpenPosition {
            stop_loss: Some(99.0),
            ..cross_position(&path)
        },
        OpenPosition {
            entry_price: None,
            ..cross_position(&path)
        },
        OpenPosition {
            opened_at: None,
            ..cross_position(&path)
        },
        OpenPosition {
            opened_at: Some(path[116].time),
            ..cross_position(&path)
        },
    ] {
        let broker = SpyBroker::with(path.clone());
        run_cross(&broker, &position, &path);
        assert!(broker.amends.borrow().is_empty());
    }
}
#[test]
fn ma_cross_live_ignores_warmup_and_favourable_crosses() {
    let path = cross_path();
    let broker = SpyBroker::with(path.clone());
    let pos = OpenPosition {
        direction: Direction::Long,
        stop_loss: Some(60.0),
        ..cross_position(&path)
    };
    let mut attempt = cross_attempt();
    attempt.direction = Direction::Long;
    pollster::block_on(watch_one(
        &broker,
        None,
        &[attempt],
        &pos,
        path.last().unwrap().time + Duration::hours(1),
    ));
    assert!(broker.amends.borrow().is_empty());
}

#[test]
fn ma_cross_live_never_banks_a_losing_exit_book_cross_for_a_later_recovery() {
    let path = cross_path();
    let mut broker = SpyBroker::with(path.clone());
    let cross = (0..path.len())
        .find(|&i| trade_control_core::ma_cross::adverse_cross_at(&path, i, Direction::Short))
        .unwrap();
    let books = broker.cross_books.as_mut().unwrap();
    books[cross].ask_c = 101.0; // Mid is profitable, but a short exits at the losing ASK.
    broker.exit_price = Some(99.0); // A later recovery must not bank that old cross.
    run_cross(&broker, &cross_position(&path), &path);
    assert!(broker.amends.borrow().is_empty());
    broker.cross_books = None;
    run_cross(&broker, &cross_position(&path), &path);
    assert!(
        broker.amends.borrow().is_empty(),
        "missing books cannot be replaced by mid history"
    );
}
