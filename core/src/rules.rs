//! Rhai-based rules engine for `Tunable<T>` script evaluation.
//!
//! Holds three pieces:
//!
//! 1. [`build_engine`] — a single configured [`rhai::Engine`] with
//!    sandboxing limits and our helper functions (`pct`, `pips`)
//!    registered. The engine is cheap to reuse across many script
//!    evaluations within one request.
//! 2. [`bind_shell_anchors`] — pushes the Shell's 12 anchors into a
//!    fresh [`rhai::Scope`]. Phase 1 of the three-phase scope build.
//! 3. [`eval_script`] — generic `Script<T>` evaluator. Parses the
//!    script source, runs it against the supplied scope, and casts
//!    the result back to `T`.
//!
//! Phases 2 (derived intent geometry) and 3 (Tunable resolution) land
//! with the C-tunable-fields and C-allow-entry sub-steps. They build
//! on the surface here.

use rhai::{Dynamic, Engine, EvalAltResult, ParseError, Scope};

use crate::intent::{Shell, SignalKind};
use crate::tunable::CompiledScript;

/// Maximum number of Rhai opcodes a single script may execute. Real
/// `allow_entry` / sizing scripts run a few comparisons and arithmetic
/// ops — well under 50. 1000 is a wide margin while still blowing up
/// any runaway loop within milliseconds. The worker maps the resulting
/// error to a 412.
pub const MAX_OPS: u64 = 1_000;

/// Maximum nesting depth for parsed scripts (call levels + expression
/// nesting). Trading rules are flat; 16 is plenty.
pub const MAX_EXPR_DEPTH: usize = 16;

/// Bind name for shell fields that are absent at evaluation time
/// (`Option::None`). Scripts that reference a missing anchor get a
/// typed Rhai error rather than a silent zero / false. Surfaced via
/// [`RuleError::MissingAnchor`] when the engine raises a variable-
/// undefined error — but most of the time we just bind `Dynamic::UNIT`
/// so the script can do an `if signal_kind == ()` check itself.
fn unit() -> Dynamic {
    Dynamic::UNIT
}

#[derive(Debug)]
pub enum RuleError {
    /// Script source failed to parse.
    Parse(String),
    /// Script parsed but evaluation failed (runtime error, op-count
    /// exceeded, missing variable, type mismatch in a helper, etc.).
    Eval(String),
    /// Script returned a value of the wrong type for the field it
    /// drives — e.g. an `allow_entry: Tunable<bool>` script that
    /// returned `1.0`.
    WrongType {
        expected: &'static str,
        got: &'static str,
    },
}

impl core::fmt::Display for RuleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Parse(msg) => write!(f, "rhai parse error: {msg}"),
            Self::Eval(msg) => write!(f, "rhai eval error: {msg}"),
            Self::WrongType { expected, got } => {
                write!(f, "rhai script returned {got}, expected {expected}")
            }
        }
    }
}

impl std::error::Error for RuleError {}

impl From<ParseError> for RuleError {
    fn from(e: ParseError) -> Self {
        Self::Parse(e.to_string())
    }
}

impl From<Box<EvalAltResult>> for RuleError {
    fn from(e: Box<EvalAltResult>) -> Self {
        // `eval_expression_with_scope` reports syntax errors as
        // `ErrorParsing` wrapped inside an EvalAltResult — surface
        // those as Parse, not Eval, so callers can distinguish.
        if matches!(*e, EvalAltResult::ErrorParsing(_, _)) {
            Self::Parse(e.to_string())
        } else {
            Self::Eval(e.to_string())
        }
    }
}

/// Build a sandboxed Rhai engine with our helpers registered. Cheap to
/// build per request — Rhai's `Engine` is a couple of Vecs and a
/// HashMap, not a JIT.
pub fn build_engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(MAX_OPS);
    engine.set_max_expr_depths(MAX_EXPR_DEPTH, MAX_EXPR_DEPTH);
    // No file IO, no module imports — keep the engine flush against
    // pure expression / arithmetic territory.
    engine.set_max_modules(0);

    // pct(a, b) — a as a percent of b. Returns 0.0 when b == 0.0 so
    // scripts can write `pct(signal_range, tp_distance) >= 10.0`
    // without guarding against a zero TP distance.
    engine.register_fn("pct", |a: f64, b: f64| -> f64 {
        if b == 0.0 { 0.0 } else { a / b * 100.0 }
    });

    // pips(distance, pip_size) — convert a price distance to pips.
    // pip_size of 0 is the same defensive zero-return as pct.
    engine.register_fn("pips", |distance: f64, pip_size: f64| -> f64 {
        if pip_size == 0.0 {
            0.0
        } else {
            distance / pip_size
        }
    });

    engine
}

/// Phase 1 of scope building: bind every Shell anchor into the scope.
///
/// - Required fields (close/high/low) bind as `f64`.
/// - `time` binds as `i64` ms-epoch.
/// - The 8 signal_* / golden / atr fields bind as their value when
///   present, or [`Dynamic::UNIT`] (Rhai `()`) when absent.
/// - `signal_kind` binds as a lowercase snake_case string
///   (`"pinbar"`, `"tweezer"`, `"regular_engulfer"`,
///   `"floating_engulfer"`, `"double_tweezer"`) so scripts can write
///   `signal_kind == "pinbar"` ergonomically.
pub fn bind_shell_anchors(scope: &mut Scope, shell: &Shell) {
    scope.push_constant("close", shell.close);
    scope.push_constant("high", shell.high);
    scope.push_constant("low", shell.low);
    scope.push_constant("time", shell.time.timestamp_millis());

    push_opt_f64(scope, "signal_high", shell.signal_high);
    push_opt_f64(scope, "signal_low", shell.signal_low);
    push_opt_f64(scope, "signal_range", shell.signal_range);
    push_opt_i64(
        scope,
        "signal_start_time",
        shell.signal_start_time.map(|t| t.timestamp_millis()),
    );
    push_opt_kind(scope, shell.signal_kind);
    push_opt_bool(scope, "golden", shell.golden);
    push_opt_f64(scope, "atr", shell.atr);
    push_opt_bool(scope, "signal_confirmed", shell.signal_confirmed);
}

fn push_opt_f64(scope: &mut Scope, name: &'static str, v: Option<f64>) {
    match v {
        Some(x) => scope.push_constant(name, x),
        None => scope.push_constant_dynamic(name, unit()),
    };
}

fn push_opt_i64(scope: &mut Scope, name: &'static str, v: Option<i64>) {
    match v {
        Some(x) => scope.push_constant(name, x),
        None => scope.push_constant_dynamic(name, unit()),
    };
}

fn push_opt_bool(scope: &mut Scope, name: &'static str, v: Option<bool>) {
    match v {
        Some(x) => scope.push_constant(name, x),
        None => scope.push_constant_dynamic(name, unit()),
    };
}

fn push_opt_kind(scope: &mut Scope, v: Option<SignalKind>) {
    let name = "signal_kind";
    match v {
        Some(k) => scope.push_constant(name, signal_kind_to_str(k).to_string()),
        None => scope.push_constant_dynamic(name, unit()),
    };
}

fn signal_kind_to_str(k: SignalKind) -> &'static str {
    match k {
        SignalKind::Pinbar => "pinbar",
        SignalKind::Tweezer => "tweezer",
        SignalKind::RegularEngulfer => "regular_engulfer",
        SignalKind::FloatingEngulfer => "floating_engulfer",
        SignalKind::DoubleTweezer => "double_tweezer",
    }
}

/// Trait for "what Rhai return types can a `Tunable<T>` ultimately
/// resolve to". Implementing this for a `T` enables
/// [`eval_script::<T>`]. Kept small on purpose — the field types we
/// actually tune (bool, f64, u32) are the only ones that need it.
pub trait FromRhai: Sized {
    /// Display name used in [`RuleError::WrongType`] errors.
    const NAME: &'static str;
    /// Extract `Self` from a Rhai-evaluated [`Dynamic`].
    fn from_rhai(v: Dynamic) -> Result<Self, RuleError>;
}

impl FromRhai for bool {
    const NAME: &'static str = "bool";
    fn from_rhai(v: Dynamic) -> Result<Self, RuleError> {
        let type_name = v.type_name();
        v.try_cast::<bool>().ok_or(RuleError::WrongType {
            expected: Self::NAME,
            got: dyn_type_name(type_name),
        })
    }
}

impl FromRhai for f64 {
    const NAME: &'static str = "f64";
    fn from_rhai(v: Dynamic) -> Result<Self, RuleError> {
        let type_name = v.type_name();
        // Accept i64 → f64 coercion: scripts that compute a whole-
        // number result (e.g. `if r >= 3 { 1 } else { 0 }` for an
        // f64 field like risk_pct) shouldn't be a wrong-type error.
        if let Some(i) = v.clone().try_cast::<i64>() {
            return Ok(i as f64);
        }
        v.try_cast::<f64>().ok_or(RuleError::WrongType {
            expected: Self::NAME,
            got: dyn_type_name(type_name),
        })
    }
}

impl FromRhai for u32 {
    const NAME: &'static str = "u32";
    fn from_rhai(v: Dynamic) -> Result<Self, RuleError> {
        let type_name = v.type_name();
        let i = v.try_cast::<i64>().ok_or(RuleError::WrongType {
            expected: Self::NAME,
            got: dyn_type_name(type_name),
        })?;
        if !(0..=i64::from(u32::MAX)).contains(&i) {
            return Err(RuleError::Eval(format!("value {i} out of range for u32")));
        }
        Ok(i as u32)
    }
}

/// Static lookup for the handful of Rhai built-in type names we'll
/// surface — keeps the `&'static str` requirement of
/// [`RuleError::WrongType::got`] satisfiable without leaking a `String`.
fn dyn_type_name(name: &str) -> &'static str {
    match name {
        "bool" => "bool",
        "i64" => "i64",
        "f64" => "f64",
        "()" => "unit",
        "string" => "string",
        "char" => "char",
        _ => "other",
    }
}

/// Evaluate a [`CompiledScript`] against `scope`, casting the result
/// to `T`.
pub fn eval_script<T: FromRhai>(
    engine: &Engine,
    scope: &mut Scope,
    script: &CompiledScript,
) -> Result<T, RuleError> {
    let value: Dynamic = engine.eval_expression_with_scope(scope, &script.source)?;
    T::from_rhai(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_full() -> Shell {
        Shell {
            close: 1.2345,
            high: 1.2350,
            low: 1.2340,
            time: "2026-05-26T10:00:00Z".parse().unwrap(),
            signal_high: Some(1.2348),
            signal_low: Some(1.2342),
            signal_range: Some(0.0006),
            signal_start_time: Some("2026-05-26T09:00:00Z".parse().unwrap()),
            signal_kind: Some(SignalKind::Pinbar),
            golden: Some(true),
            atr: Some(0.0012),
            signal_confirmed: Some(false),
        }
    }

    fn shell_minimal() -> Shell {
        Shell {
            close: 1.0,
            high: 1.0,
            low: 1.0,
            time: "2026-05-26T10:00:00Z".parse().unwrap(),
            signal_high: None,
            signal_low: None,
            signal_range: None,
            signal_start_time: None,
            signal_kind: None,
            golden: None,
            atr: None,
            signal_confirmed: None,
        }
    }

    #[test]
    fn anchors_required_bind_correctly() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let s = CompiledScript::new("close");
        let v: f64 = eval_script(&engine, &mut scope, &s).unwrap();
        assert!((v - 1.2345).abs() < 1e-9);
    }

    #[test]
    fn signal_kind_binds_as_lowercase_string() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let s = CompiledScript::new("signal_kind == \"pinbar\"");
        let v: bool = eval_script(&engine, &mut scope, &s).unwrap();
        assert!(v);
    }

    #[test]
    fn signal_kind_variants_map_to_expected_strings() {
        // Sweep every variant — guards against the to_str map and
        // SignalKind enum drifting apart.
        for (kind, expected) in [
            (SignalKind::Pinbar, "pinbar"),
            (SignalKind::Tweezer, "tweezer"),
            (SignalKind::RegularEngulfer, "regular_engulfer"),
            (SignalKind::FloatingEngulfer, "floating_engulfer"),
            (SignalKind::DoubleTweezer, "double_tweezer"),
        ] {
            let mut shell = shell_full();
            shell.signal_kind = Some(kind);
            let engine = build_engine();
            let mut scope = Scope::new();
            bind_shell_anchors(&mut scope, &shell);
            let src = format!("signal_kind == \"{expected}\"");
            let v: bool = eval_script(&engine, &mut scope, &CompiledScript::new(src)).unwrap();
            assert!(v, "{kind:?} should bind as {expected:?}");
        }
    }

    #[test]
    fn golden_bool_visible() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let s = CompiledScript::new("golden");
        let v: bool = eval_script(&engine, &mut scope, &s).unwrap();
        assert!(v);
    }

    #[test]
    fn missing_field_binds_as_unit() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_minimal());
        // `signal_kind == ()` returns true when signal_kind was None.
        let s = CompiledScript::new("signal_kind == ()");
        let v: bool = eval_script(&engine, &mut scope, &s).unwrap();
        assert!(v);
    }

    #[test]
    fn pct_helper_returns_percentage() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        // signal_range / 0.0060 == 0.0006 / 0.0060 = 10%
        let s = CompiledScript::new("pct(signal_range, 0.0060)");
        let v: f64 = eval_script(&engine, &mut scope, &s).unwrap();
        assert!((v - 10.0).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn pct_helper_zero_denominator_returns_zero() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let s = CompiledScript::new("pct(1.0, 0.0)");
        let v: f64 = eval_script(&engine, &mut scope, &s).unwrap();
        assert_eq!(v, 0.0);
    }

    #[test]
    fn pips_helper_converts_distance() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        // 0.0050 / 0.0001 = 50
        let s = CompiledScript::new("pips(0.0050, 0.0001)");
        let v: f64 = eval_script(&engine, &mut scope, &s).unwrap();
        assert!((v - 50.0).abs() < 1e-9);
    }

    #[test]
    fn parse_error_surfaces() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let s = CompiledScript::new("if if if");
        let err = eval_script::<bool>(&engine, &mut scope, &s).unwrap_err();
        assert!(matches!(err, RuleError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn wrong_return_type_surfaces() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let s = CompiledScript::new("1.5");
        let err = eval_script::<bool>(&engine, &mut scope, &s).unwrap_err();
        match err {
            RuleError::WrongType { expected, got } => {
                assert_eq!(expected, "bool");
                assert_eq!(got, "f64");
            }
            other => panic!("expected WrongType, got {other:?}"),
        }
    }

    #[test]
    fn i64_coerces_to_f64() {
        // `if ... { 1 } else { 0 }` returns i64; f64 fields should
        // accept it without a WrongType error.
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let s = CompiledScript::new("if golden { 1 } else { 0 }");
        let v: f64 = eval_script(&engine, &mut scope, &s).unwrap();
        assert_eq!(v, 1.0);
    }

    #[test]
    fn u32_from_rhai_accepts_in_range() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let s = CompiledScript::new("3");
        let v: u32 = eval_script(&engine, &mut scope, &s).unwrap();
        assert_eq!(v, 3);
    }

    #[test]
    fn u32_from_rhai_rejects_negative() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let s = CompiledScript::new("-1");
        let err = eval_script::<u32>(&engine, &mut scope, &s).unwrap_err();
        assert!(matches!(err, RuleError::Eval(_)), "got {err:?}");
    }

    #[test]
    fn op_limit_blows_up_runaway_loop() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        // eval_expression rejects block statements, so go through
        // run_with_scope to exercise the op-counter.
        let src = "let i = 0; while i < 1000000 { i += 1; } i > 0";
        let err = engine.run_with_scope(&mut scope, src).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("operations") || msg.contains("Operation"),
            "expected op-limit error, got: {msg}"
        );
    }

    #[test]
    fn allow_entry_style_script_evaluates() {
        // The canonical wait-for-confirmation pattern.
        let engine = build_engine();
        let mut scope = Scope::new();
        let mut shell = shell_full();
        shell.signal_confirmed = Some(true);
        bind_shell_anchors(&mut scope, &shell);
        let s = CompiledScript::new("signal_confirmed == true");
        let v: bool = eval_script(&engine, &mut scope, &s).unwrap();
        assert!(v);
    }

    #[test]
    fn allow_entry_compound_script_evaluates() {
        // signal_confirmed || range-as-pct-of-tp >= 10 — the candle-
        // size override the operator described.
        let engine = build_engine();
        let mut scope = Scope::new();
        let mut shell = shell_full();
        shell.signal_confirmed = Some(false);
        shell.signal_range = Some(0.0006);
        bind_shell_anchors(&mut scope, &shell);
        // tp_distance not in scope yet (Phase 2). Use a literal for now.
        let s = CompiledScript::new("signal_confirmed == true || pct(signal_range, 0.006) >= 10.0");
        let v: bool = eval_script(&engine, &mut scope, &s).unwrap();
        assert!(v);
    }
}
