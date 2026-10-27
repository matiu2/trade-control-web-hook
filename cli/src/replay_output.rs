//! Captured corpus reports, with optional CLI terminal output.

use color_eyre::eyre::Result;
use serde::Serialize;
use serde_json::Value;

use crate::replay_candles::outcome;

/// A fixture or batch replay's measurements and explicit completion status.
#[derive(Debug, Clone, Serialize)]
pub struct CorpusReplayResult {
    pub report: String,
    pub summary: Option<Value>,
    pub baseline_diff: Option<Value>,
    pub success: bool,
    pub error: Option<String>,
    pub exit_code: i32,
}

#[derive(Default)]
pub(crate) struct ReplayOutput {
    report: String,
    emit: bool,
    pub summary: Option<Value>,
    pub baseline_diff: Option<Value>,
}

impl ReplayOutput {
    pub fn terminal() -> Self {
        Self {
            emit: true,
            ..Self::default()
        }
    }

    pub fn write(&mut self, text: &str) {
        if self.emit {
            print!("{text}");
        }
        self.report.push_str(text);
    }

    pub fn write_stderr(&mut self, text: &str) {
        if self.emit {
            eprint!("{text}");
        }
        self.report.push_str(text);
    }

    pub fn finish(self, result: Result<()>) -> CorpusReplayResult {
        let exit_code = result.as_ref().err().map_or(outcome::EXIT_OK, |error| {
            outcome::FailureKind::classify(error).exit_code()
        });
        CorpusReplayResult {
            report: self.report,
            summary: self.summary,
            baseline_diff: self.baseline_diff,
            success: result.is_ok(),
            error: result.err().map(|error| format!("{error:#}")),
            exit_code,
        }
    }
}
