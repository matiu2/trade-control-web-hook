//! Chart-facing arming with explicit routing and structured results.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::{CommandFactory, FromArgMatches, Parser};
use color_eyre::eyre::{Result, eyre};
use serde::Serialize;
use serde_json::Value;

use crate::args::{Args, BrokerArg, Command};
use crate::frozen_setup::FrozenSetup;

/// Trusted server configuration. Never populated from browser option fields.
#[derive(Debug, Clone)]
pub struct ArmContext {
    pub worker_url: String,
    pub key_file: PathBuf,
    pub broker: BrokerArg,
    /// Pins named MT5/cTrader accounts; other feeds may use an account option.
    pub account_id: Option<String>,
}

pub struct FrozenArmRequest {
    pub setup: FrozenSetup,
    pub options: Vec<String>,
    pub action: ArmAction,
}

pub enum ArmAction {
    Register {
        replace: Option<String>,
        shadow: bool,
    },
    Replay {
        options: Vec<String>,
    },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ArmOutcome {
    pub success: bool,
    pub status: String,
    pub error: Option<String>,
    pub executions: Vec<ArmExecution>,
    pub report: String,
    pub positions: Option<Value>,
    pub replaced_trade_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ArmExecution {
    pub trade_id: String,
    pub instrument: String,
    pub broker: String,
    pub account: String,
    pub pattern: String,
    pub plan: Option<Value>,
    pub plan_path: Option<PathBuf>,
    pub output_dir: Option<PathBuf>,
    pub worker_response: Option<String>,
    pub replay_outcome: Option<Value>,
    pub report: Option<String>,
    pub positions: Option<Value>,
}

#[derive(Debug, Clone)]
pub(crate) struct Execution {
    pub context: ArmContext,
    outcome: Arc<Mutex<ArmOutcome>>,
}

impl Execution {
    pub(crate) fn new(context: ArmContext) -> Self {
        Self {
            context,
            outcome: Arc::new(Mutex::new(ArmOutcome::default())),
        }
    }

    pub(crate) fn record(&self, execution: ArmExecution) -> Result<()> {
        self.update(|outcome| outcome.executions.push(execution))
    }

    pub(crate) fn update(&self, update: impl FnOnce(&mut ArmOutcome)) -> Result<()> {
        let mut outcome = self
            .outcome
            .lock()
            .map_err(|_| eyre!("arm result lock poisoned"))?;
        update(&mut outcome);
        Ok(())
    }

    pub(crate) fn take_error(&self) -> Result<Option<String>> {
        self.outcome
            .lock()
            .map(|mut outcome| outcome.error.take())
            .map_err(|_| eyre!("arm result lock poisoned"))
    }

    fn finish(&self, result: Result<i32>) -> Result<ArmOutcome> {
        self.update(|outcome| match result {
            Ok(0) => {
                outcome.success = true;
                outcome.status = "complete".into();
            }
            Ok(code) => {
                outcome.status = "rejected".into();
                outcome
                    .error
                    .get_or_insert_with(|| format!("arm rejected (code {code})"));
            }
            Err(error) => {
                outcome.status = "failed".into();
                outcome.error = Some(format!("{error:#}"));
            }
        })?;
        self.outcome
            .lock()
            .map(|outcome| outcome.clone())
            .map_err(|_| eyre!("arm result lock poisoned"))
    }
}

/// Runs without a chart transport or process-global environment changes.
///
/// Call on a blocking thread (for example `tokio::task::spawn_blocking`).
/// `options` use the clap surface described by [`crate::option_schema::options`].
/// The server must construct those tokens from validated fields, and must pin
/// chart identity in `context`. Both command validation and setup validation run
/// before any registration. Operational failures preserve partial results.
pub fn run_frozen(request: FrozenArmRequest, context: ArmContext) -> Result<ArmOutcome> {
    validate_context(&context)?;
    let execution = Execution::new(context);
    let mut args = parse_options(request.options, request.action)?;
    args.broker = Some(execution.context.broker);
    if let Some(account) = execution.context.account_id.as_ref() {
        args.account_id = Some(account.clone());
    }
    args.execution = Some(execution.clone());
    let args = args.apply_aliases();
    args.validate()?;
    crate::position_entry::validate_command(&args)?;
    if let Some(Command::Replay { .. }) = args.command.as_ref() {
        let mut tokens = vec!["replay-candles".to_string()];
        tokens.extend(args.replay_args());
        let replay = trade_control_cli::replay_args::ReplayArgs::try_parse_from(tokens)
            .map_err(|error| eyre!("invalid replay options: {error}"))?;
        if replay.test_mode {
            return execution.finish(crate::replay::run_corpus(replay, &execution).map(|()| 0));
        }
    }
    execution.finish(crate::pipeline::run_frozen(args, request.setup))
}

/// Replay a saved fixture or batch without chart geometry, signing credentials,
/// or a broker connection. As with [`run_frozen`], call from a blocking thread.
pub fn run_corpus(options: Vec<String>, context: ArmContext) -> Result<ArmOutcome> {
    validate_context(&context)?;
    let execution = Execution::new(context);
    let args = parse_options(Vec::new(), ArmAction::Replay { options })?;
    let tokens = std::iter::once("replay-candles".to_string()).chain(args.replay_args());
    let parsed = trade_control_cli::replay_args::ReplayArgs::try_parse_from(tokens)
        .map_err(|error| eyre!("invalid corpus options: {error}"))?;
    if !parsed.test_mode {
        return Err(eyre!("corpus replay requires --test-mode"));
    }
    execution.finish(crate::replay::run_corpus(parsed, &execution).map(|()| 0))
}

fn parse_options(options: Vec<String>, action: ArmAction) -> Result<Args> {
    let mut tokens = vec!["tv-arm".to_string()];
    tokens.extend(options);
    match action {
        ArmAction::Register { replace, shadow } => {
            tokens.push("register".into());
            if let Some(target) = replace {
                tokens.push("--replace".into());
                if !target.is_empty() {
                    tokens.push(target);
                }
            }
            if shadow {
                tokens.push("--shadow".into());
            }
        }
        ArmAction::Replay { options } => {
            // Validate passthrough before reading credentials/calendar or writing
            // signed artifacts. Replay itself will validate the resolved argv.
            let mut replay = vec!["replay-candles".to_string()];
            replay.extend(options.iter().cloned());
            let parsed = trade_control_cli::replay_args::ReplayArgs::try_parse_from(replay)
                .map_err(|error| eyre!("invalid replay options: {error}"))?;
            if !parsed.test_mode && (parsed.check || parsed.rebless || parsed.fixture.is_some()) {
                return Err(eyre!(
                    "fixture, check and rebless options require --test-mode"
                ));
            }
            tokens.push("replay".into());
            tokens.extend(options);
        }
    }
    let matches = Args::command()
        .try_get_matches_from(tokens)
        .map_err(|error| eyre!("invalid arm options: {error}"))?;
    let mut args =
        Args::from_arg_matches(&matches).map_err(|error| eyre!("invalid arm options: {error}"))?;
    if matches.value_source("account_id") == Some(clap::parser::ValueSource::EnvVariable) {
        args.account_id = None;
    }
    if !matches!(
        args.command,
        Some(Command::Register { .. } | Command::Replay { .. })
    ) {
        return Err(eyre!("a chart arm requires register or replay"));
    }
    Ok(args)
}

fn validate_context(context: &ArmContext) -> Result<()> {
    let url = url::Url::parse(&context.worker_url)
        .map_err(|error| eyre!("invalid worker URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(eyre!("worker URL must be an explicit HTTP(S) endpoint"));
    }
    if context.key_file.as_os_str().is_empty() {
        return Err(eyre!("an explicit signing key file is required"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    fn init() {
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    }

    #[test]
    fn rejects_invalid_replay_options_before_side_effects() {
        init();
        let result = parse_options(
            vec![],
            ArmAction::Replay {
                options: vec!["--unknown-option".into()],
            },
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid replay options")
        );
    }

    #[test]
    fn rejection_keeps_its_message_and_never_claims_success() {
        init();
        let execution = Execution::new(ArmContext {
            worker_url: "http://127.0.0.1:8788".into(),
            key_file: "/tmp/unused-key".into(),
            broker: BrokerArg::Oanda,
            account_id: None,
        });
        execution
            .update(|outcome| outcome.error = Some("missing trade expiry".into()))
            .unwrap();
        let outcome = execution.finish(Ok(1)).unwrap();
        assert!(!outcome.success);
        assert_eq!(outcome.status, "rejected");
        assert_eq!(outcome.error.as_deref(), Some("missing trade expiry"));
    }

    #[test]
    fn failed_run_keeps_partial_registration_details() {
        init();
        let execution = Execution::new(ArmContext {
            worker_url: "http://127.0.0.1:8788".into(),
            key_file: "/tmp/unused-key".into(),
            broker: BrokerArg::Oanda,
            account_id: None,
        });
        execution
            .record(ArmExecution {
                trade_id: "test-id".into(),
                ..Default::default()
            })
            .unwrap();
        let result = execution
            .finish(Err(eyre!("worker rejected request")))
            .unwrap();
        assert!(!result.success);
        assert_eq!(result.executions[0].trade_id, "test-id");
        assert!(result.error.unwrap().contains("worker rejected"));
    }

    fn frozen_setup() -> FrozenSetup {
        FrozenSetup::capture(
            crate::plan_geometry::PlanGeometry::default(),
            "60".into(),
            "OANDA:EURUSD".into(),
            Some(1_700_000_000),
            None,
        )
    }

    #[test]
    fn geometry_rejection_returns_the_actual_reason_without_posting() {
        init();
        let directory = tempfile::tempdir().unwrap();
        let key_file = directory.path().join("key.hex");
        std::fs::write(&key_file, "00".repeat(trade_control_core::sig::KEY_LEN)).unwrap();
        let result = run_frozen(
            FrozenArmRequest {
                setup: frozen_setup(),
                options: vec!["--skip-calendar-bars".into()],
                action: ArmAction::Register {
                    replace: None,
                    shadow: true,
                },
            },
            ArmContext {
                worker_url: "http://127.0.0.1:1".into(),
                key_file,
                broker: BrokerArg::Oanda,
                account_id: Some("test-account".into()),
            },
        )
        .unwrap();
        assert!(!result.success);
        assert_eq!(result.status, "rejected");
        assert!(!result.error.unwrap().contains("code 1"));
        assert!(result.executions.is_empty());
    }

    #[test]
    fn corpus_failure_preserves_report_without_credentials_or_geometry() {
        init();
        let directory = tempfile::tempdir().unwrap();
        let mut setup = frozen_setup();
        setup.chart_symbol = "deliberately-invalid-symbol".into();
        let result = run_frozen(
            FrozenArmRequest {
                setup,
                options: vec![],
                action: ArmAction::Replay {
                    options: vec![
                        "--test-mode".into(),
                        "--fixture".into(),
                        "missing-fixture".into(),
                        "--fixtures-dir".into(),
                        directory.path().display().to_string(),
                        "--json".into(),
                    ],
                },
            },
            ArmContext {
                worker_url: "http://127.0.0.1:1".into(),
                key_file: directory.path().join("does-not-exist"),
                broker: BrokerArg::Oanda,
                account_id: None,
            },
        )
        .unwrap();
        assert!(!result.success);
        assert_eq!(result.status, "failed");
        assert!(result.report.contains("missing-fixture"));
        assert_eq!(
            result.executions[0].replay_outcome.as_ref().unwrap()["success"],
            false
        );
    }

    #[test]
    fn public_corpus_api_requires_fixture_mode_before_side_effects() {
        init();
        let error = run_corpus(
            Vec::new(),
            ArmContext {
                worker_url: "http://127.0.0.1:1".into(),
                key_file: "/missing-key".into(),
                broker: BrokerArg::Oanda,
                account_id: None,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("requires --test-mode"));
    }
}
