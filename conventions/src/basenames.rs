//! Canonical filenames for signed-alert YAMLs emitted by the
//! `trade-control` CLI. These doubles as routing keys: the
//! `tv_arm_hs` / `tv-arm` pipeline parses each manifest entry's
//! basename into an [`AlertBasename`] and dispatches to the right
//! TradingView alert shape (drawing-bound, value-bound, Pine, or
//! synthetic vertical line).

use alloc::borrow::Cow;
use alloc::format;
use alloc::string::String;

/// One of the canonical alert-file basenames. Stable across the
/// stack — basename is the wire format between the CLI's emitter,
/// the manifest, and the chart-arming side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertBasename {
    /// `01-veto-too-high` — short-trade invalidation (price runs back
    /// up past structure). Drawing-bound to the operator's horizontal
    /// line above the shoulder.
    VetoTooHigh,
    /// `01-veto-too-low` — long-trade invalidation (price runs back
    /// down past structure).
    VetoTooLow,
    /// `02-veto-trade-expiry` — drops the bundle at expiry. Bound to
    /// the operator's `trade-expiry` vertical line.
    VetoTradeExpiry,
    /// `03-prep-break-and-close` — neckline break-and-close prep.
    PrepBreakAndClose,
    /// `04-prep-retest` — neckline retest prep.
    PrepRetest,
    /// `05-enter` — Pine `Candle Signals` entry alert.
    Enter,
    /// `06-close-on-reversal` — Pine reversal close, gated on an
    /// active news window.
    CloseOnReversal,
    /// `07-close-on-sr-reversal` — Pine reversal close, gated on
    /// price sitting inside a chart-drawn support/resistance band.
    CloseOnSrReversal,
    /// `01-pause-<id>` — pause window start.
    PauseStart(String),
    /// `02-resume-<id>` — pause window end (resume entries).
    PauseResume(String),
    /// `01-news-start-<id>` — news window start.
    NewsStart(String),
    /// `02-news-end-<id>` — news window end.
    NewsEnd(String),
}

impl AlertBasename {
    /// The on-disk basename without the `.yaml` extension.
    pub fn as_str(&self) -> Cow<'static, str> {
        match self {
            Self::VetoTooHigh => Cow::Borrowed("01-veto-too-high"),
            Self::VetoTooLow => Cow::Borrowed("01-veto-too-low"),
            Self::VetoTradeExpiry => Cow::Borrowed("02-veto-trade-expiry"),
            Self::PrepBreakAndClose => Cow::Borrowed("03-prep-break-and-close"),
            Self::PrepRetest => Cow::Borrowed("04-prep-retest"),
            Self::Enter => Cow::Borrowed("05-enter"),
            Self::CloseOnReversal => Cow::Borrowed("06-close-on-reversal"),
            Self::CloseOnSrReversal => Cow::Borrowed("07-close-on-sr-reversal"),
            Self::PauseStart(id) => Cow::Owned(format!("01-pause-{id}")),
            Self::PauseResume(id) => Cow::Owned(format!("02-resume-{id}")),
            Self::NewsStart(id) => Cow::Owned(format!("01-news-start-{id}")),
            Self::NewsEnd(id) => Cow::Owned(format!("02-news-end-{id}")),
        }
    }

    /// Parse a basename (no `.yaml` extension) back into its enum
    /// shape. Round-trip: `parse(self.as_str()) == Some(self)` for
    /// every variant.
    ///
    /// Returns `None` for unrecognised shapes (e.g. a future
    /// basename this build doesn't know about).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "01-veto-too-high" => Some(Self::VetoTooHigh),
            "01-veto-too-low" => Some(Self::VetoTooLow),
            "02-veto-trade-expiry" => Some(Self::VetoTradeExpiry),
            "03-prep-break-and-close" => Some(Self::PrepBreakAndClose),
            "04-prep-retest" => Some(Self::PrepRetest),
            "05-enter" => Some(Self::Enter),
            "06-close-on-reversal" => Some(Self::CloseOnReversal),
            "07-close-on-sr-reversal" => Some(Self::CloseOnSrReversal),
            other => other
                .strip_prefix("01-pause-")
                .map(|id| Self::PauseStart(id.into()))
                .or_else(|| {
                    other
                        .strip_prefix("02-resume-")
                        .map(|id| Self::PauseResume(id.into()))
                })
                .or_else(|| {
                    other
                        .strip_prefix("01-news-start-")
                        .map(|id| Self::NewsStart(id.into()))
                })
                .or_else(|| {
                    other
                        .strip_prefix("02-news-end-")
                        .map(|id| Self::NewsEnd(id.into()))
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variants() -> [AlertBasename; 12] {
        [
            AlertBasename::VetoTooHigh,
            AlertBasename::VetoTooLow,
            AlertBasename::VetoTradeExpiry,
            AlertBasename::PrepBreakAndClose,
            AlertBasename::PrepRetest,
            AlertBasename::Enter,
            AlertBasename::CloseOnReversal,
            AlertBasename::CloseOnSrReversal,
            AlertBasename::PauseStart("cal-foo-1780034400-pause".into()),
            AlertBasename::PauseResume("cal-foo-1780034400-pause".into()),
            AlertBasename::NewsStart("cal-foo-1780034400-news".into()),
            AlertBasename::NewsEnd("cal-foo-1780034400-news".into()),
        ]
    }

    #[test]
    fn round_trip() {
        for v in variants() {
            let s = v.as_str();
            let parsed = AlertBasename::parse(&s).expect("parse roundtrip");
            assert_eq!(parsed, v, "round-trip failed for {s}");
        }
    }

    #[test]
    fn known_literal_strings() {
        assert_eq!(AlertBasename::VetoTooHigh.as_str(), "01-veto-too-high");
        assert_eq!(AlertBasename::VetoTooLow.as_str(), "01-veto-too-low");
        assert_eq!(
            AlertBasename::VetoTradeExpiry.as_str(),
            "02-veto-trade-expiry"
        );
        assert_eq!(
            AlertBasename::PrepBreakAndClose.as_str(),
            "03-prep-break-and-close"
        );
        assert_eq!(AlertBasename::PrepRetest.as_str(), "04-prep-retest");
        assert_eq!(AlertBasename::Enter.as_str(), "05-enter");
        assert_eq!(
            AlertBasename::CloseOnReversal.as_str(),
            "06-close-on-reversal"
        );
        assert_eq!(
            AlertBasename::CloseOnSrReversal.as_str(),
            "07-close-on-sr-reversal"
        );
    }

    #[test]
    fn pause_id_format() {
        let v = AlertBasename::PauseStart("xyz".into());
        assert_eq!(v.as_str(), "01-pause-xyz");
    }

    #[test]
    fn unknown_basename_returns_none() {
        assert!(AlertBasename::parse("99-future-thing").is_none());
        assert!(AlertBasename::parse("").is_none());
    }
}
