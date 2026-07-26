//! How a replay ended, as something a *driver* can act on.
//!
//! Batch sweeps run `replay-candles` hundreds of times and scrape stdout for
//! the `Net R:` summary. That works right up until a run dies before it can
//! print anything — and then "the process crashed" and "the engine ran and no
//! trade filled" look identical, because both produce no `Net R:`. A sweep
//! silently loses a random subset of its cells and still looks complete; the
//! grid shifts and nothing announces it.
//!
//! Two things fix that, and both live here:
//!
//! 1. **A terminal line is always printed**, success or failure, so its
//!    *absence* unambiguously means "died in a way nobody handled".
//! 2. **The exit code says what kind of failure it was**, so a driver can tell
//!    "retry this" (infrastructure) from "record this result" (ran fine) from
//!    "fix your input" (bad arguments).

use std::fmt;

/// Process exit code: the replay ran to completion. The trading outcome is
/// whatever it was — including a legitimate 0R with no fill. **Record it.**
pub const EXIT_OK: i32 = 0;

/// Process exit code: the replay could not run because something in the
/// environment was broken — candle cache unreachable, broker auth/network,
/// TradingView unavailable. The inputs were fine and nothing was measured.
/// **Retry it.**
pub const EXIT_INFRASTRUCTURE: i32 = 3;

/// Process exit code: the replay could not run because the *request* was
/// malformed — unparseable window, missing plan, no such fixture. Retrying
/// verbatim will fail identically. **Fix the input.**
pub const EXIT_BAD_INPUT: i32 = 4;

/// Process exit code: `--check` ran fine and the fixture's outcome **did not
/// match** `expected.json`. A regression verdict, not a fault — the run worked
/// exactly as asked and the answer was "different". Kept distinct from
/// [`EXIT_INFRASTRUCTURE`] so CI doesn't retry a genuine regression forever.
pub const EXIT_CHECK_MISMATCH: i32 = 5;

/// Why a replay failed, coarse enough that a driver can branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The environment let us down; the inputs were fine.
    Infrastructure,
    /// The request was malformed; the environment was fine.
    BadInput,
    /// `--check` ran and the fixture disagreed with `expected.json`.
    CheckMismatch,
}

impl FailureKind {
    /// The exit code a driver should see for this failure.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Infrastructure => EXIT_INFRASTRUCTURE,
            Self::BadInput => EXIT_BAD_INPUT,
            Self::CheckMismatch => EXIT_CHECK_MISMATCH,
        }
    }

    /// Short machine-readable tag for the terminal line's `error:` field.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Infrastructure => "infrastructure",
            Self::BadInput => "bad-input",
            Self::CheckMismatch => "check-mismatch",
        }
    }

    /// Classify a failure by looking for a [`BadInput`] marker in its chain.
    ///
    /// **Typed, not textual.** An earlier version substring-matched the
    /// lowercased error chain, and it was wrong in both directions:
    ///
    /// - *Infrastructure read as bad input* — the dangerous direction. A plan on
    ///   a dropped NFS mount reports `No such file or directory`, and any broker
    ///   HTTP body containing "is required" matched too. Exit 4 tells a driver
    ///   "retrying will fail identically", so a transient mount blip got
    ///   recorded as a permanent result — the silent cell-loss this module
    ///   exists to prevent.
    /// - *Bad input read as infrastructure* — a typo'd instrument yields
    ///   "unsupported instrument", but the marker list said "unknown
    ///   instrument", so it exited 3 and a driver retried the typo forever.
    ///
    /// The chain is *data* — it includes remote text we don't control — so it
    /// can't carry the verdict. Now only a call site that KNOWS the input was
    /// malformed says so, by attaching [`BadInput`] via [`bad_input`].
    /// Everything else is infrastructure: a wrong guess there costs one retry,
    /// the other way loses a cell.
    pub fn classify(err: &color_eyre::Report) -> Self {
        if err.chain().any(|c| c.is::<CheckMismatch>()) {
            return Self::CheckMismatch;
        }
        if err.chain().any(|c| c.is::<BadInput>()) {
            return Self::BadInput;
        }
        Self::Infrastructure
    }
}

/// Marker for an error whose cause is the *request*, not the environment — a
/// typo'd instrument, an unparseable window, a missing `--fixture`. Retrying
/// such a run verbatim always fails the same way.
///
/// Deliberately a type rather than a phrase: see [`FailureKind::classify`].
/// It carries the message so it can sit at the HEAD of the chain — `wrap_err`
/// erases the context's concrete type, so a marker added that way would be
/// invisible to `chain().any(|c| c.is::<BadInput>())`.
#[derive(Debug)]
pub struct BadInput(String);

impl fmt::Display for BadInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadInput {}

/// Tag an error as operator-supplied bad input, so it exits 4 ("fix it")
/// rather than 3 ("retry it").
///
/// Use at the point a check *knows* the input was malformed:
/// `return Err(bad_input(eyre!("unsupported granularity {g:?}")));`
pub fn bad_input(report: color_eyre::Report) -> color_eyre::Report {
    color_eyre::Report::new(BadInput(format!("{report:#}")))
}

/// Marker for a `--check` fixture mismatch: the run succeeded, the answer
/// differed. See [`EXIT_CHECK_MISMATCH`].
#[derive(Debug)]
pub struct CheckMismatch(String);

impl fmt::Display for CheckMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CheckMismatch {}

/// Tag a fixture-comparison failure so it exits [`EXIT_CHECK_MISMATCH`]
/// instead of being mistaken for a retryable infrastructure fault.
pub fn check_mismatch(report: color_eyre::Report) -> color_eyre::Report {
    color_eyre::Report::new(CheckMismatch(format!("{report:#}")))
}

/// The always-printed terminal line for a failed run.
///
/// Mirrors the success summary's `Done: … | … | Net R: …` shape so one regex
/// can scrape both, but reports `Net R: n/a` — *not* `+0.00`, which would be
/// indistinguishable from a real no-fill replay and would quietly corrupt a
/// sweep's average.
pub struct FailureLine<'a> {
    pub kind: FailureKind,
    pub detail: &'a str,
}

impl fmt::Display for FailureLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Done: false  |  error: {}  |  detail: {}  |  Net R: n/a",
            self.kind.tag(),
            // Keep it to one line: a driver reads this line-wise, and eyre
            // details are frequently multi-line.
            self.detail.replace('\n', " ")
        )
    }
}

/// Render the loud banner for an infrastructure failure reaching the candle
/// cache. A generic sqlx/pool error buried in noisy stderr is easy to miss when
/// scripted; this is meant to be unmissable.
pub fn cache_unreachable_banner(url: &str, cause: &str) -> String {
    format!("CANNOT REACH CANDLE CACHE at {url}: {cause}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use color_eyre::eyre::eyre;

    /// Only a `bad_input`-tagged error is non-retryable, and the tag survives
    /// further `wrap_err` context added on the way up the stack.
    #[test]
    fn tagged_errors_are_bad_input() {
        let tagged = bad_input(eyre!("unsupported granularity \"3h\""));
        assert_eq!(FailureKind::classify(&tagged), FailureKind::BadInput);

        let wrapped = bad_input(eyre!("unsupported instrument \"xyz\"")).wrap_err("pull candles");
        assert_eq!(
            FailureKind::classify(&wrapped),
            FailureKind::BadInput,
            "the marker must survive added context"
        );
    }

    /// Real infrastructure text — including phrases an earlier substring-based
    /// classifier wrongly matched as bad input. Getting these wrong is the
    /// dangerous direction: exit 4 tells a driver not to retry, so a transient
    /// fault would be recorded as a permanent result.
    #[test]
    fn untagged_errors_are_infrastructure() {
        for text in [
            "storage error: postgresql error: connection refused",
            "pool timed out while waiting for an open connection",
            // ENOENT on a dropped mount — contains "no such file"
            "read plan /mnt/nfs/p.json: No such file or directory (os error 2)",
            // a broker response body that happens to contain "is required"
            "tradenation api 400: field \"instrument\" is required",
            // a truncated read of a plan being written concurrently
            "parse plan JSON /tmp/p.json: EOF while parsing a value",
            "something nobody has ever seen before",
        ] {
            assert_eq!(
                FailureKind::classify(&eyre!("{text}")),
                FailureKind::Infrastructure,
                "{text:?} must stay retryable"
            );
        }
    }

    /// Guards the wiring, not the wording: these call the REAL error
    /// constructors rather than hand-written strings, so a message reworded at
    /// its source (or a `bad_input` tag dropped) fails here. The previous
    /// version asserted a marker list against itself and could not fail.
    #[test]
    fn real_bad_input_constructors_classify_as_bad_input() {
        use crate::replay_candles::{granularity, instrument};
        use trade_control_cli::replay_args::CandleSource;

        let cases: Vec<(&str, color_eyre::Report)> = vec![
            (
                "unsupported granularity",
                granularity::parse("3h").expect_err("3h is not a granularity"),
            ),
            (
                "unsupported instrument",
                instrument::resolve_for("totally-not-real", CandleSource::TradeNation)
                    .expect_err("not a catalog instrument"),
            ),
            (
                "unparseable datetime",
                crate::parse_start_end("not-a-date").expect_err("not a datetime"),
            ),
        ];

        for (what, err) in cases {
            assert_eq!(
                FailureKind::classify(&err),
                FailureKind::BadInput,
                "{what} must exit {EXIT_BAD_INPUT} (fix-the-input), not \
                 {EXIT_INFRASTRUCTURE} (retry-forever); got: {err}"
            );
        }
    }

    #[test]
    fn exit_codes_are_distinct() {
        assert_ne!(EXIT_OK, EXIT_INFRASTRUCTURE);
        assert_ne!(EXIT_OK, EXIT_BAD_INPUT);
        assert_ne!(
            FailureKind::Infrastructure.exit_code(),
            FailureKind::BadInput.exit_code()
        );
    }

    /// `Net R: n/a` — never `+0.00`, which a sweep would average in as a real
    /// zero-return trade.
    #[test]
    fn failure_line_reports_na_not_zero() {
        let line = FailureLine {
            kind: FailureKind::Infrastructure,
            detail: "cache unreachable",
        }
        .to_string();
        assert!(line.contains("Net R: n/a"), "got: {line}");
        assert!(!line.contains("+0.00"), "got: {line}");
        assert!(line.contains("error: infrastructure"), "got: {line}");
    }

    /// The line must survive being scraped line-wise, so a multi-line eyre
    /// detail gets flattened.
    #[test]
    fn failure_line_is_a_single_line() {
        let line = FailureLine {
            kind: FailureKind::BadInput,
            detail: "first\nsecond\nthird",
        }
        .to_string();
        assert!(!line.contains('\n'), "got: {line}");
    }
}
