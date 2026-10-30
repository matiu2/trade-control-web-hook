use super::*;

fn setup(timing: RetryTiming) -> Recovery {
    crate::test_logging::init_tracing();
    let now = "2026-10-30T01:00:00Z".parse().unwrap();
    Recovery {
        policy: SpreadRetry {
            timing,
            max_age_secs: 3600,
            max_chase_r: 0.25,
            lookahead_secs: 60,
            min_improvement_r: 0.05,
        },
        direction: Direction::Short,
        ideal_entry: 100.0,
        stop_loss: 101.0,
        take_profit: 96.0,
        boundary: 102.0,
        min_r: 1.0,
        started_at: now,
        sampled_at: now,
        last_entry: 100.0,
        last_spread: 0.3,
        spread_decay: 0.0,
        adverse_speed: 0.0,
    }
}

fn quote(bid: f64, spread: f64) -> Quote {
    Quote {
        bid,
        ask: bid + spread,
    }
}

#[test]
fn a_prediction_cannot_override_a_stop_past_the_boundary() {
    let mut recovery = setup(RetryTiming::Adaptive);
    let now = recovery.started_at + chrono::Duration::seconds(60);
    assert_eq!(
        recovery.decide(&quote(100.0, 0.25), now),
        Decision::Wait("spread-still-past-veto")
    );
}

#[test]
fn immediate_retries_only_when_measured_spread_clears_the_boundary() {
    let mut recovery = setup(RetryTiming::Immediate);
    let now = recovery.started_at + chrono::Duration::seconds(60);
    assert_eq!(recovery.decide(&quote(100.0, 0.15), now), Decision::Ready);
}

#[test]
fn adaptive_waits_while_a_falling_spread_improves_r_faster_than_price_drift() {
    let mut recovery = setup(RetryTiming::Adaptive);
    let now = recovery.started_at + chrono::Duration::seconds(60);
    assert_eq!(
        recovery.decide(&quote(100.0, 0.15), now),
        Decision::Wait("spread-decaying-faster-than-price-cost")
    );
    assert!(recovery.spread_decay > 0.0);
}

#[test]
fn adaptive_retries_before_the_projected_move_exhausts_the_chase_budget() {
    let mut recovery = setup(RetryTiming::Adaptive);
    let now = recovery.started_at + chrono::Duration::seconds(60);
    assert_eq!(recovery.decide(&quote(99.8, 0.15), now), Decision::Ready);
}

#[test]
fn expired_or_runaway_signals_are_dropped_without_placement() {
    let mut recovery = setup(RetryTiming::Immediate);
    let now = recovery.started_at + chrono::Duration::seconds(60);
    assert_eq!(
        recovery.decide(&quote(99.7, 0.05), now),
        Decision::Drop("spread-retry-price-ran-away")
    );
    let deadline = recovery.started_at + chrono::Duration::seconds(3600);
    assert_eq!(
        recovery.decide(&quote(100.0, 0.05), deadline),
        Decision::Drop("spread-retry-expired")
    );
}

#[test]
fn long_recovery_uses_the_ask_and_mirrors_the_short_rules() {
    let mut recovery = setup(RetryTiming::Immediate);
    recovery.direction = Direction::Long;
    recovery.stop_loss = 99.0;
    recovery.take_profit = 104.0;
    recovery.boundary = 98.0;
    let now = recovery.started_at + chrono::Duration::seconds(60);
    assert_eq!(
        recovery.decide(&quote(100.1, 0.2), now),
        Decision::Drop("spread-retry-price-ran-away")
    );
    assert_eq!(recovery.decide(&quote(99.9, 0.1), now), Decision::Ready);
}

#[test]
fn invalid_quotes_never_release_a_recovery() {
    let mut recovery = setup(RetryTiming::Immediate);
    let now = recovery.started_at + chrono::Duration::seconds(60);
    assert_eq!(
        recovery.decide(&quote(f64::NAN, 0.1), now),
        Decision::Wait("no-valid-quote")
    );
}

#[test]
fn a_stop_breach_on_the_exit_quote_drops_a_waiting_signal() {
    let mut recovery = setup(RetryTiming::Immediate);
    let now = recovery.started_at + chrono::Duration::seconds(60);
    assert_eq!(
        recovery.decide(&quote(100.9, 0.2), now),
        Decision::Drop("spread-retry-bracket-consumed")
    );
    recovery.direction = Direction::Long;
    recovery.stop_loss = 99.0;
    recovery.take_profit = 104.0;
    recovery.boundary = 98.0;
    assert_eq!(
        recovery.decide(&quote(98.9, 0.2), now),
        Decision::Drop("spread-retry-bracket-consumed")
    );
}
