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
//! 3. [`bind_intent_derived`] — pushes the resolved entry / SL / TP
//!    geometry (`entry_price`, `sl_distance`, `tp_distance`,
//!    `r_multiple`, `pip_size`, `instrument`, `direction`) into the
//!    scope. Phase 2 of the three-phase build; called after Phase 1
//!    so scripts can mix shell anchors with derived distances.
//! 4. [`eval_script`] — generic `CompiledScript` evaluator. Parses
//!    the script source, runs it against the supplied scope, and
//!    casts the result back to `T`.
//! 5. [`resolve_tunable`] — Phase 3. Resolves a [`Tunable<T>`] field
//!    against an already-built scope: `Static(v)` returns `v` as-is;
//!    `Script(s)` runs through [`eval_script`]. This is the call
//!    sites use when reading per-field tunables (`risk_pct`,
//!    `max_retries`, `allow_entry`, etc.).

use rhai::{Dynamic, Engine, EvalAltResult, ParseError, Scope};

use crate::intent::{Direction, Resolved, ResolvedEntry, Shell, SignalKind};
use crate::tunable::{CompiledScript, Tunable};

// Re-export the two types call sites need to build and use a scope. The
// worker only ever touches the engine via `build_engine` + the helpers
// here — keeping the re-export contained means downstream crates don't
// need to depend on `rhai` directly.
pub use rhai::Scope as RhaiScope;

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

/// Phase 2 of scope building: push the resolved geometry into the scope.
///
/// Bindings:
/// - `entry_price: f64` — the resolved entry price. For [`ResolvedEntry::Market`]
///   it's the reference price (shell close); for `Stop` / `Limit` it's the
///   trigger price.
/// - `stop_loss: f64` — absolute SL price.
/// - `take_profit: f64` — absolute TP price.
/// - `sl_distance: f64` — `|entry_price - stop_loss|`, always positive.
/// - `tp_distance: f64` — `|take_profit - entry_price|`, always positive.
/// - `r_multiple: f64` — `tp_distance / sl_distance`, or `0.0` if SL distance
///   is zero (defensive — `Resolved` guards against degenerate geometry but
///   we don't want a script to panic on a future regression).
/// - `pip_size: f64` — instrument pip size as supplied by the caller.
/// - `instrument: String` — e.g. `"EUR_USD"`.
/// - `direction: String` — `"long"` or `"short"`. Mirrors the
///   [`SignalKind`] string-binding convention so scripts can write
///   `direction == "long"`.
///
/// Must be called after [`bind_shell_anchors`] — Phase 2 layers on top
/// of Phase 1 in the same scope.
pub fn bind_intent_derived(scope: &mut Scope, resolved: &Resolved, pip_size: f64) {
    let entry_price = match resolved.entry {
        ResolvedEntry::Market { reference_price } => reference_price,
        ResolvedEntry::Stop { trigger_price } | ResolvedEntry::Limit { trigger_price } => {
            trigger_price
        }
    };
    let sl_distance = (entry_price - resolved.stop_loss).abs();
    let tp_distance = (resolved.take_profit - entry_price).abs();
    let r_multiple = if sl_distance == 0.0 {
        0.0
    } else {
        tp_distance / sl_distance
    };
    let direction = match resolved.direction {
        Direction::Long => "long",
        Direction::Short => "short",
    };

    scope.push_constant("entry_price", entry_price);
    scope.push_constant("stop_loss", resolved.stop_loss);
    scope.push_constant("take_profit", resolved.take_profit);
    scope.push_constant("sl_distance", sl_distance);
    scope.push_constant("tp_distance", tp_distance);
    scope.push_constant("r_multiple", r_multiple);
    scope.push_constant("pip_size", pip_size);
    scope.push_constant("instrument", resolved.instrument.clone());
    scope.push_constant("direction", direction.to_string());
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

/// Resolve a [`Tunable<T>`] against an already-built scope.
///
/// - [`Tunable::Static`] returns the embedded value as-is (cloned). No
///   engine call, no allocation past the clone.
/// - [`Tunable::Script`] dispatches to [`eval_script`], which runs the
///   script and casts the result to `T` via [`FromRhai`].
///
/// Call sites typically look like:
///
/// ```ignore
/// let engine = build_engine();
/// let mut scope = Scope::new();
/// bind_shell_anchors(&mut scope, shell);
/// bind_intent_derived(&mut scope, &resolved, pip_size);
/// let risk_pct: f64 = resolve_tunable(&engine, &mut scope, &intent.risk_pct)?;
/// ```
///
/// The `Clone` bound covers the `Static` branch — for the small,
/// numeric / boolean field types we promote to `Tunable<T>` it's free.
pub fn resolve_tunable<T: FromRhai + Clone>(
    engine: &Engine,
    scope: &mut Scope,
    tunable: &Tunable<T>,
) -> Result<T, RuleError> {
    match tunable {
        Tunable::Static(v) => Ok(v.clone()),
        Tunable::Script(s) => eval_script(engine, scope, s),
    }
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

    // ---- Phase 2: bind_intent_derived ----

    fn resolved_long_market() -> Resolved {
        // entry=1.1000, SL=1.0978, TP=1.1044 → sl=0.0022, tp=0.0044, R=2.0
        Resolved {
            id: "t1".into(),
            not_after: "2026-05-13T20:00:00Z".parse().unwrap(),
            instrument: "EUR_USD".into(),
            direction: Direction::Long,
            entry: ResolvedEntry::Market {
                reference_price: 1.1000,
            },
            stop_loss: 1.0978,
            take_profit: 1.1044,
            risk: crate::intent::RiskBudget::Percent(0.5),
            dry_run: false,
        }
    }

    #[test]
    fn derived_market_entry_binds_reference_price() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        bind_intent_derived(&mut scope, &resolved_long_market(), 0.0001);
        let v: f64 = eval_script(&engine, &mut scope, &CompiledScript::new("entry_price")).unwrap();
        assert!((v - 1.1000).abs() < 1e-9);
    }

    #[test]
    fn derived_stop_entry_binds_trigger_price() {
        let engine = build_engine();
        let mut scope = Scope::new();
        let mut r = resolved_long_market();
        r.entry = ResolvedEntry::Stop {
            trigger_price: 1.1022,
        };
        bind_shell_anchors(&mut scope, &shell_full());
        bind_intent_derived(&mut scope, &r, 0.0001);
        let v: f64 = eval_script(&engine, &mut scope, &CompiledScript::new("entry_price")).unwrap();
        assert!((v - 1.1022).abs() < 1e-9);
    }

    #[test]
    fn derived_limit_entry_binds_trigger_price() {
        let engine = build_engine();
        let mut scope = Scope::new();
        let mut r = resolved_long_market();
        r.entry = ResolvedEntry::Limit {
            trigger_price: 1.0985,
        };
        bind_shell_anchors(&mut scope, &shell_full());
        bind_intent_derived(&mut scope, &r, 0.0001);
        let v: f64 = eval_script(&engine, &mut scope, &CompiledScript::new("entry_price")).unwrap();
        assert!((v - 1.0985).abs() < 1e-9);
    }

    #[test]
    fn derived_distances_always_positive_for_long() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        bind_intent_derived(&mut scope, &resolved_long_market(), 0.0001);
        let sl: f64 =
            eval_script(&engine, &mut scope, &CompiledScript::new("sl_distance")).unwrap();
        let tp: f64 =
            eval_script(&engine, &mut scope, &CompiledScript::new("tp_distance")).unwrap();
        assert!((sl - 0.0022).abs() < 1e-9, "got {sl}");
        assert!((tp - 0.0044).abs() < 1e-9, "got {tp}");
    }

    #[test]
    fn derived_distances_always_positive_for_short() {
        // Short: entry above SL, TP below entry. Both distances must
        // still bind as positive magnitudes.
        let engine = build_engine();
        let mut scope = Scope::new();
        let r = Resolved {
            id: "t1".into(),
            not_after: "2026-05-13T20:00:00Z".parse().unwrap(),
            instrument: "EUR_USD".into(),
            direction: Direction::Short,
            entry: ResolvedEntry::Market {
                reference_price: 1.1000,
            },
            stop_loss: 1.1022,
            take_profit: 1.0956,
            risk: crate::intent::RiskBudget::Percent(0.5),
            dry_run: false,
        };
        bind_shell_anchors(&mut scope, &shell_full());
        bind_intent_derived(&mut scope, &r, 0.0001);
        let sl: f64 =
            eval_script(&engine, &mut scope, &CompiledScript::new("sl_distance")).unwrap();
        let tp: f64 =
            eval_script(&engine, &mut scope, &CompiledScript::new("tp_distance")).unwrap();
        assert!((sl - 0.0022).abs() < 1e-9, "got {sl}");
        assert!((tp - 0.0044).abs() < 1e-9, "got {tp}");
    }

    #[test]
    fn derived_r_multiple_computes_correctly() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        bind_intent_derived(&mut scope, &resolved_long_market(), 0.0001);
        let v: f64 = eval_script(&engine, &mut scope, &CompiledScript::new("r_multiple")).unwrap();
        assert!((v - 2.0).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn derived_r_multiple_zero_sl_distance_returns_zero() {
        // Degenerate geometry — `Resolved` won't normally produce this
        // (the resolver rejects EntryOutsideRange) but the script
        // surface should be defensive.
        let engine = build_engine();
        let mut scope = Scope::new();
        let mut r = resolved_long_market();
        r.stop_loss = 1.1000; // == entry_price
        bind_shell_anchors(&mut scope, &shell_full());
        bind_intent_derived(&mut scope, &r, 0.0001);
        let v: f64 = eval_script(&engine, &mut scope, &CompiledScript::new("r_multiple")).unwrap();
        assert_eq!(v, 0.0);
    }

    #[test]
    fn derived_pip_size_visible() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        bind_intent_derived(&mut scope, &resolved_long_market(), 0.0001);
        let v: f64 = eval_script(&engine, &mut scope, &CompiledScript::new("pip_size")).unwrap();
        assert!((v - 0.0001).abs() < 1e-12);
    }

    #[test]
    fn derived_instrument_binds_as_string() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        bind_intent_derived(&mut scope, &resolved_long_market(), 0.0001);
        let v: bool = eval_script(
            &engine,
            &mut scope,
            &CompiledScript::new("instrument == \"EUR_USD\""),
        )
        .unwrap();
        assert!(v);
    }

    #[test]
    fn derived_direction_binds_long() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        bind_intent_derived(&mut scope, &resolved_long_market(), 0.0001);
        let v: bool = eval_script(
            &engine,
            &mut scope,
            &CompiledScript::new("direction == \"long\""),
        )
        .unwrap();
        assert!(v);
    }

    #[test]
    fn derived_direction_binds_short() {
        let engine = build_engine();
        let mut scope = Scope::new();
        let mut r = resolved_long_market();
        r.direction = Direction::Short;
        // Flip geometry so the binding is the only thing under test —
        // bind_intent_derived doesn't validate the geometry itself.
        r.stop_loss = 1.1022;
        r.take_profit = 1.0956;
        bind_shell_anchors(&mut scope, &shell_full());
        bind_intent_derived(&mut scope, &r, 0.0001);
        let v: bool = eval_script(
            &engine,
            &mut scope,
            &CompiledScript::new("direction == \"short\""),
        )
        .unwrap();
        assert!(v);
    }

    #[test]
    fn shell_and_derived_compose_in_same_scope() {
        // Canonical allow_entry-style script that mixes Phase 1
        // (signal_range) with Phase 2 (tp_distance) bindings.
        let engine = build_engine();
        let mut scope = Scope::new();
        let mut shell = shell_full();
        shell.signal_range = Some(0.0006);
        shell.signal_confirmed = Some(false);
        bind_shell_anchors(&mut scope, &shell);
        bind_intent_derived(&mut scope, &resolved_long_market(), 0.0001);
        // signal_range 0.0006 / tp_distance 0.0044 = ~13.6%
        let s = CompiledScript::new(
            "signal_confirmed == true || pct(signal_range, tp_distance) >= 10.0",
        );
        let v: bool = eval_script(&engine, &mut scope, &s).unwrap();
        assert!(v);
    }

    #[test]
    fn pips_helper_works_with_bound_pip_size() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        bind_intent_derived(&mut scope, &resolved_long_market(), 0.0001);
        // sl_distance = 0.0022; 0.0022 / 0.0001 = 22 pips
        let v: f64 = eval_script(
            &engine,
            &mut scope,
            &CompiledScript::new("pips(sl_distance, pip_size)"),
        )
        .unwrap();
        assert!((v - 22.0).abs() < 1e-9, "got {v}");
    }

    // ---- Phase 3: resolve_tunable ----

    #[test]
    fn resolve_tunable_static_f64_returns_value() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let t: Tunable<f64> = Tunable::Static(0.5);
        let v: f64 = resolve_tunable(&engine, &mut scope, &t).unwrap();
        assert_eq!(v, 0.5);
    }

    #[test]
    fn resolve_tunable_static_bool_returns_value() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let t: Tunable<bool> = Tunable::Static(true);
        let v: bool = resolve_tunable(&engine, &mut scope, &t).unwrap();
        assert!(v);
    }

    #[test]
    fn resolve_tunable_static_u32_returns_value() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let t: Tunable<u32> = Tunable::Static(3);
        let v: u32 = resolve_tunable(&engine, &mut scope, &t).unwrap();
        assert_eq!(v, 3);
    }

    #[test]
    fn resolve_tunable_script_evaluates_against_scope() {
        // Canonical wait-for-confirmation gate.
        let engine = build_engine();
        let mut scope = Scope::new();
        let mut shell = shell_full();
        shell.signal_confirmed = Some(true);
        bind_shell_anchors(&mut scope, &shell);
        let t: Tunable<bool> = Tunable::from_script("signal_confirmed == true");
        let v: bool = resolve_tunable(&engine, &mut scope, &t).unwrap();
        assert!(v);
    }

    #[test]
    fn resolve_tunable_script_sees_derived_geometry() {
        // Script reads r_multiple (Phase 2) — proves the three phases
        // compose end-to-end.
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        bind_intent_derived(&mut scope, &resolved_long_market(), 0.0001);
        // R = 2.0 on the fixture; 1.5 risk if R >= 2 else 0.5.
        let t: Tunable<f64> = Tunable::from_script("if r_multiple >= 2.0 { 1.5 } else { 0.5 }");
        let v: f64 = resolve_tunable(&engine, &mut scope, &t).unwrap();
        assert!((v - 1.5).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn resolve_tunable_script_parse_error_surfaces() {
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let t: Tunable<bool> = Tunable::from_script("if if if");
        let err = resolve_tunable::<bool>(&engine, &mut scope, &t).unwrap_err();
        assert!(matches!(err, RuleError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn resolve_tunable_script_wrong_type_surfaces() {
        // Script returns f64 for a bool-typed Tunable.
        let engine = build_engine();
        let mut scope = Scope::new();
        bind_shell_anchors(&mut scope, &shell_full());
        let t: Tunable<bool> = Tunable::from_script("1.5");
        let err = resolve_tunable::<bool>(&engine, &mut scope, &t).unwrap_err();
        match err {
            RuleError::WrongType { expected, got } => {
                assert_eq!(expected, "bool");
                assert_eq!(got, "f64");
            }
            other => panic!("expected WrongType, got {other:?}"),
        }
    }

    #[test]
    fn resolve_tunable_static_no_engine_dependency() {
        // Sanity: a Static tunable resolves even against an empty scope.
        // The engine is required by the signature but never called on the
        // Static branch.
        let engine = build_engine();
        let mut scope = Scope::new();
        let t: Tunable<f64> = Tunable::Static(42.0);
        let v: f64 = resolve_tunable(&engine, &mut scope, &t).unwrap();
        assert_eq!(v, 42.0);
    }
}
