//! The local-chart output destination implied by a frozen spec input.

use local_chart_client::DEFAULT_LOCAL_CHART_URL;
use url::Url;

/// An HTTP spec belongs to its server; a file belongs to the default chart.
/// Invalid spec URLs are rejected by the spec reader before replay output.
pub(super) fn inferred_url(spec_url: Option<&str>) -> String {
    spec_url
        .and_then(|raw| Url::parse(raw).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|| DEFAULT_LOCAL_CHART_URL.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Args;
    use clap::Parser;
    use tracing_subscriber::prelude::*;

    fn parse(argv: &[&str]) -> Args {
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer())
            .with(tracing_error::ErrorLayer::default())
            .try_init()
            .ok();
        Args::try_parse_from(argv)
            .expect("valid command")
            .apply_aliases()
    }

    #[test]
    fn spec_file_implies_local_chart_and_preserves_replay_arguments() {
        let args = parse(&[
            "tv-arm-staging",
            "--spec-in",
            "trade.spec.json",
            "replay",
            "--warmup-bars",
            "400",
        ]);
        assert_eq!(args.new_tv_url(), Some(DEFAULT_LOCAL_CHART_URL));
        assert_eq!(args.replay_args(), ["--warmup-bars", "400"]);
    }

    #[test]
    fn spec_url_returns_output_to_the_source_server() {
        [
            (
                "http://127.0.0.1:8790/?instrument=AU200_AUD&tf=h1&broker=oanda",
                "http://127.0.0.1:8790",
            ),
            (
                "http://127.0.0.1:9999/arm-setup?mode=replay",
                "http://127.0.0.1:9999",
            ),
            (
                "https://charts.example.test/arm-setup?mode=replay",
                "https://charts.example.test",
            ),
        ]
        .into_iter()
        .for_each(|(source, expected)| {
            let args = parse(&["tv-arm", "--spec-url", source, "replay"]);
            assert_eq!(args.new_tv_url(), Some(expected));
        });
    }

    #[test]
    fn explicit_chart_destinations_win_before_and_after_replay() {
        ["--spec-in", "--spec-url"].into_iter().for_each(|flag| {
            let source = if flag == "--spec-in" {
                "trade.spec.json"
            } else {
                "http://source:8791/arm-setup"
            };
            let before = parse(&[
                "tv-arm",
                flag,
                source,
                "--new-tv=http://chosen:9000",
                "replay",
            ]);
            assert_eq!(before.new_tv_url(), Some("http://chosen:9000"));
            let after = parse(&[
                "tv-arm",
                flag,
                source,
                "replay",
                "--new-tv",
                "http://chosen:9000",
                "--annotate",
                "true",
            ]);
            assert_eq!(after.new_tv_url(), Some("http://chosen:9000"));
            assert_eq!(after.replay_args(), ["--annotate", "true"]);
        });
    }

    #[test]
    fn explicit_bare_new_tv_keeps_its_default_destination() {
        let args = parse(&[
            "tv-arm",
            "--spec-url",
            "http://source:9999/arm-setup",
            "replay",
            "--new-tv",
        ]);
        assert_eq!(args.new_tv_url(), Some(DEFAULT_LOCAL_CHART_URL));
        assert!(args.replay_args().is_empty());
    }

    #[test]
    fn live_chart_and_spec_output_do_not_imply_local_chart() {
        let live = parse(&["tv-arm", "replay"]);
        assert_eq!(live.new_tv_url(), None);
        let capture = parse(&["tv-arm", "--spec-out", "trade.spec.json", "replay"]);
        assert_eq!(capture.new_tv_url(), None);
    }

    #[test]
    fn inference_is_idempotent_and_preserves_invalid_spec_errors() {
        let args = parse(&["tv-arm", "--spec-url", "not a url", "replay"]);
        assert_eq!(args.spec_url.as_deref(), Some("not a url"));
        assert!(
            crate::spec_url::normalise("not a url", crate::spec_url::SpecMode::Replay).is_err()
        );
        let once = parse(&["tv-arm", "--spec-in", "trade.spec.json", "replay"]);
        let twice = once.clone().apply_aliases();
        assert_eq!(once.new_tv_url(), twice.new_tv_url());
    }
}
