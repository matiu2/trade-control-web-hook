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

/// Why a replay failed, coarse enough that a driver can branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The environment let us down; the inputs were fine.
    Infrastructure,
    /// The request was malformed; the environment was fine.
    BadInput,
}

impl FailureKind {
    /// The exit code a driver should see for this failure.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Infrastructure => EXIT_INFRASTRUCTURE,
            Self::BadInput => EXIT_BAD_INPUT,
        }
    }

    /// Short machine-readable tag for the terminal line's `error:` field.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Infrastructure => "infrastructure",
            Self::BadInput => "bad-input",
        }
    }

    /// Classify a failure from its error chain.
    ///
    /// Deliberately **fails toward `Infrastructure`**: an unrecognised error is
    /// far more likely to be a flaky environment than a malformed request (bad
    /// input is nearly always caught by clap or an explicit check, both of
    /// which produce phrasing we match below). Getting this wrong in the
    /// retryable direction costs a wasted retry; getting it wrong the other way
    /// silently drops a cell from the grid, which is the failure this whole
    /// module exists to prevent.
    pub fn classify(err: &color_eyre::Report) -> Self {
        let haystack = err
            .chain()
            .map(|cause| cause.to_string().to_lowercase())
            .collect::<Vec<_>>()
            .join(" | ");
        Self::classify_text(&haystack)
    }

    /// The matching itself, split out so it can be tested without building a
    /// real error chain.
    pub fn classify_text(haystack: &str) -> Self {
        // Bad-input phrasings. These are things the operator typed, so no
        // amount of retrying changes them.
        const BAD_INPUT_MARKERS: [&str; 15] = [
            "is required",
            "not valid rfc3339",
            "premature end of input",
            "input contains invalid characters",
            "parse --start",
            "parse --end",
            "must be after start",
            "no such file",
            "parse plan json",
            "is ambiguous in brisbane time",
            "does not match the plan",
            "unknown instrument",
            // A fixture that won't load or parse is bad input, not a flaky
            // environment: the bytes on disk are corrupt (or absent), so retrying
            // verbatim fails identically. `fixture::read_json` wraps these as
            // `parse <path>` / `read <path>`, and a batch folds every failing row's
            // reason into its summary error so one corrupt fixture in 291 still
            // classifies the run correctly.
            "/plan.json",
            "/candles.json",
            "/meta.json",
        ];
        if BAD_INPUT_MARKERS.iter().any(|m| haystack.contains(m)) {
            return Self::BadInput;
        }
        Self::Infrastructure
    }
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

    #[test]
    fn bad_input_phrasings_are_not_retryable() {
        for text in [
            "--plan is required (or use --test-mode --fixture <name>)",
            "parse --start: input contains invalid characters",
            "end (x) must be after start (y)",
            "parse plan json /tmp/p.json",
        ] {
            assert_eq!(
                FailureKind::classify_text(&text.to_lowercase()),
                FailureKind::BadInput,
                "{text:?} should be bad input"
            );
        }
    }

    /// A corrupt or missing fixture is **bad input**, not a flaky environment:
    /// the bytes on disk are wrong, so retrying verbatim fails identically. These
    /// are the real wrappings `fixture::read_json` produces.
    #[test]
    fn a_corrupt_fixture_is_bad_input_not_infrastructure() {
        for text in [
            "parse /repo/replay-fixtures/trade-124/candles.json: expected ident at line 1 column 2",
            "read /repo/replay-fixtures/trade-999/plan.json: no such file or directory (os error 2)",
            "parse /repo/replay-fixtures/trade-7/meta.json: missing field `granularity`",
        ] {
            assert_eq!(
                FailureKind::classify_text(&text.to_lowercase()),
                FailureKind::BadInput,
                "{text:?} should be bad input — retrying can't fix corrupt bytes"
            );
        }
    }

    /// A BATCH folds every failing row's reason into its summary error, so one
    /// corrupt fixture among many still classifies the whole run as bad input.
    /// (Without the fold, the chain would only carry the summary sentence and
    /// default to `Infrastructure` — costing a pointless retry of 291 fixtures.)
    #[test]
    fn a_batch_summary_classifies_from_the_folded_row_reasons() {
        let folded = "2 of 3 fixture(s) failed — see the rows above (net r +0.35 excludes them): \
             parse /repo/replay-fixtures/a/candles.json: expected ident | \
             read /repo/replay-fixtures/b/plan.json: no such file or directory";
        assert_eq!(
            FailureKind::classify_text(folded),
            FailureKind::BadInput,
            "a batch of corrupt fixtures must not read as retryable"
        );
    }

    /// But a batch whose rows failed for an *environmental* reason stays
    /// retryable — the fold must not blanket-classify everything as bad input.
    #[test]
    fn a_batch_of_infra_failures_stays_retryable() {
        let folded = "1 of 3 fixture(s) failed — see the rows above: \
             storage error: pool timed out while waiting for an open connection";
        assert_eq!(
            FailureKind::classify_text(folded),
            FailureKind::Infrastructure
        );
    }

    #[test]
    fn infrastructure_phrasings_are_retryable() {
        for text in [
            "storage error: postgresql error: connection refused",
            "database already open. cannot acquire lock.",
            "pool timed out while waiting for an open connection",
            "error sending request for url (https://api.tradenation...)",
        ] {
            assert_eq!(
                FailureKind::classify_text(&text.to_lowercase()),
                FailureKind::Infrastructure,
                "{text:?} should be infrastructure"
            );
        }
    }

    /// An error we've never seen must be retryable, not silently recorded as a
    /// result — see `classify`'s doc comment.
    #[test]
    fn unrecognised_errors_default_to_infrastructure() {
        assert_eq!(
            FailureKind::classify_text("something nobody has ever seen before"),
            FailureKind::Infrastructure
        );
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
