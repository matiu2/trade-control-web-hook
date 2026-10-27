//! Nominal cold workload: no tick pagination, one seed read per side/window.
use chrono::{DateTime, Utc};
pub(super) struct Workload {
    pub windows: i64,
    pub requests: i64,
}
pub(super) fn estimate(from: DateTime<Utc>, to: DateTime<Utc>, window_hours: u32) -> Workload {
    let minutes = (to - from).num_minutes();
    let size = i64::from(window_hours) * 60;
    let full = minutes / size;
    let tail = minutes % size;
    let windows = full + i64::from(tail > 0);
    let native = full * ((size + 499) / 500) + (tail + 499) / 500;
    Workload {
        windows,
        requests: native + 4 * windows,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn daily_windows_reduce_seed_requests_without_hiding_native_pagination() {
        crate::ctrader::test_logging();
        let from = "2026-07-01T00:00:00Z".parse().unwrap();
        let to = from + chrono::Duration::days(90);
        let daily = estimate(from, to, 24);
        assert_eq!(daily.windows, 90);
        assert_eq!(daily.requests, 630);
        assert_eq!(estimate(from, to, 2).requests, 5400);
        let tail = estimate(from, from + chrono::Duration::minutes(61), 1);
        assert_eq!(tail.windows, 2);
        assert_eq!(tail.requests, 10);
    }
}
