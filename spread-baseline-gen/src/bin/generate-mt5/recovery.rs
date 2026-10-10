//! Reconnect read-only calibration without advancing the sampling checkpoint.
use color_eyre::{
    Result,
    eyre::{Report, eyre},
};
use mt5_data_source::read_retry::is_retryable_read;
use std::time::Duration;

use super::mt5_calibration::{self, Args};

pub async fn run(args: Args) -> Result<()> {
    let mut resume = args.resume_path();
    for retry in 0..=args.reconnect_attempts() {
        let error = match mt5_calibration::attempt(&args, &mut resume).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let Some(delay) = retry_delay(&error, retry, args.reconnect_attempts()) else {
            return Err(error);
        };
        tracing::warn!(account=args.account(), retry=retry+1, limit=args.reconnect_attempts(),
            delay_seconds=delay.as_secs(), checkpoint=?resume, %error,
            "MT5 read connection interrupted; waiting before resuming checkpoint");
        tokio::time::sleep(delay).await;
    }
    Err(eyre!("MT5 reconnect retry budget exhausted"))
}

fn retry_delay(error: &Report, retry: u32, limit: u32) -> Option<Duration> {
    (retry < limit && is_retryable_read(error))
        .then(|| Duration::from_secs((15u64 << retry.min(5)).min(300)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt5_data_source::read_retry::ConnectionError;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    #[test]
    fn reconnect_waits_back_off_and_exhaust_without_retrying_invalid_accounts() {
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::fmt::layer())
            .try_init()
            .ok();
        let disconnected: Report = ConnectionError::Disconnected.into();
        assert_eq!(
            (0..7)
                .map(|n| retry_delay(&disconnected, n, 12).unwrap().as_secs())
                .collect::<Vec<_>>(),
            [15, 30, 60, 120, 240, 300, 300]
        );
        assert_eq!(retry_delay(&disconnected, 12, 12), None);
        assert_eq!(retry_delay(&disconnected, 0, 0), None);
        assert_eq!(
            retry_delay(&eyre!("MT5 account/server mismatch"), 0, 12),
            None
        );
        let login: Report = ConnectionError::CommandStatus {
            command: 28,
            status: 2,
        }
        .into();
        assert_eq!(retry_delay(&login, 0, 12), Some(Duration::from_secs(15)));
    }
}
