//! Parse the experimental spread recovery policy once, before signing entries.
use color_eyre::eyre::{Result, bail};
use trade_control_core::spread_retry::{RetryTiming, SpreadRetry};

pub(crate) fn policy(args: &crate::args::Args) -> Result<Option<SpreadRetry>> {
    let Some(mode) = args.spread_retry.as_deref() else {
        return Ok(None);
    };
    let timing = match mode {
        "immediate" => RetryTiming::Immediate,
        "adaptive" => RetryTiming::Adaptive,
        _ => bail!("unknown spread retry timing {mode}"),
    };
    let policy = SpreadRetry {
        timing,
        max_age_secs: args.spread_retry_max_age_secs,
        max_chase_r: args.spread_retry_max_chase_r,
        lookahead_secs: args.spread_retry_lookahead_secs,
        min_improvement_r: args.spread_retry_min_improvement_r,
    };
    if !policy.valid() {
        bail!(
            "spread retry: age must be 1–86400s, lookahead 1–3600s, chase and minimum improvement finite and within 0–2R"
        );
    }
    Ok(Some(policy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn recovery_is_opt_in_and_exposes_bounded_settings() {
        let args = crate::args::Args::try_parse_from(["tv-arm"]).unwrap();
        assert_eq!(policy(&args).unwrap(), None);
        let args = crate::args::Args::try_parse_from([
            "tv-arm",
            "--spread-retry",
            "adaptive",
            "--spread-retry-max-chase-r",
            "0.4",
        ])
        .unwrap();
        assert_eq!(policy(&args).unwrap().unwrap().max_chase_r, 0.4);
        let args = crate::args::Args::try_parse_from([
            "tv-arm",
            "--spread-retry",
            "immediate",
            "--spread-retry-max-chase-r",
            "NaN",
        ])
        .unwrap();
        assert!(policy(&args).is_err());
    }
}
