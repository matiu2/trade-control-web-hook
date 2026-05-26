//! `Tunable<T>` — a YAML field that is either a static value or a Rhai script.
//!
//! Wire forms:
//! ```yaml
//! risk_pct: 1.0                              # Static
//! risk_pct: !script "if r >= 3.0 { 1.0 } else { 0.5 }"   # Script
//! ```
//!
//! `CompiledScript` carries the human-readable source. Rhai's `AST` is
//! not `Serialize`, so we don't try to bincode it on the CLI side; the
//! worker parses lazily (and can cache per request if it ever matters
//! — intents typically have at most a handful of scripts). The CLI's
//! sign-time validator catches parse errors before we ever ship a
//! signed intent, so worker-side parse failures are an honest 412 on
//! a malformed manual intent, not a hot-path concern.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// A wire-form Rhai script. Source-only by design (see module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledScript {
    pub source: String,
}

impl CompiledScript {
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
        }
    }
}

// Serialised as a YAML scalar tagged `!script`. We implement Serialize /
// Deserialize manually so `Tunable<T>` can use the tag to disambiguate.
impl Serialize for CompiledScript {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        // When written as a bare CompiledScript (rare), emit the source string;
        // Tunable handles the !script tag at its own serializer.
        ser.serialize_str(&self.source)
    }
}

impl<'de> Deserialize<'de> for CompiledScript {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Ok(Self::new(s))
    }
}

/// A value that is either statically embedded in the YAML or computed
/// at intent-evaluation time by a Rhai script.
#[derive(Debug, Clone, PartialEq)]
pub enum Tunable<T> {
    Static(T),
    Script(CompiledScript),
}

impl<T: Default> Default for Tunable<T> {
    fn default() -> Self {
        Self::Static(T::default())
    }
}

impl<T> Tunable<T> {
    pub fn from_static(value: T) -> Self {
        Self::Static(value)
    }

    pub fn from_script(source: impl Into<String>) -> Self {
        Self::Script(CompiledScript::new(source))
    }

    /// If the variant is `Static`, return the inner value; otherwise None.
    /// Useful in tests and back-compat paths that haven't been migrated
    /// to the evaluator yet.
    pub fn as_static(&self) -> Option<&T> {
        match self {
            Self::Static(v) => Some(v),
            Self::Script(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Serde via serde_yaml's tag system.
//
// We deserialize from a `serde_yaml::Value` and inspect its tag. A scalar
// tagged `!script` becomes Script(CompiledScript); anything else falls
// through to T's own deserializer (so a bare scalar `1.0` becomes
// Static(1.0)).
// ---------------------------------------------------------------------------

const SCRIPT_TAG: &str = "!script";

impl<T> Serialize for Tunable<T>
where
    T: Serialize,
{
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Static(v) => v.serialize(ser),
            Self::Script(s) => {
                let tagged = serde_yaml::value::TaggedValue {
                    tag: serde_yaml::value::Tag::new("script"),
                    value: serde_yaml::Value::String(s.source.clone()),
                };
                tagged.serialize(ser)
            }
        }
    }
}

impl<'de, T> Deserialize<'de> for Tunable<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let value = serde_yaml::Value::deserialize(de)?;
        if let serde_yaml::Value::Tagged(tagged) = &value {
            let tag = tagged.tag.to_string();
            if tag == SCRIPT_TAG {
                let source = tagged
                    .value
                    .as_str()
                    .ok_or_else(|| {
                        de::Error::custom("!script tag requires a string scalar source")
                    })?
                    .to_string();
                return Ok(Self::Script(CompiledScript::new(source)));
            } else {
                return Err(de::Error::custom(format!(
                    "unknown tag on Tunable: {tag} (expected !script or a plain value)"
                )));
            }
        }
        let inner = T::deserialize(value).map_err(de::Error::custom)?;
        Ok(Self::Static(inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    struct Wrap<T> {
        v: Tunable<T>,
    }

    #[test]
    fn static_f64_parses_as_static() {
        let y = "v: 1.5\n";
        let w: Wrap<f64> = serde_yaml::from_str(y).unwrap();
        assert_eq!(w.v, Tunable::Static(1.5));
    }

    #[test]
    fn static_u32_parses_as_static() {
        let y = "v: 3\n";
        let w: Wrap<u32> = serde_yaml::from_str(y).unwrap();
        assert_eq!(w.v, Tunable::Static(3));
    }

    #[test]
    fn static_bool_parses_as_static() {
        let y = "v: true\n";
        let w: Wrap<bool> = serde_yaml::from_str(y).unwrap();
        assert_eq!(w.v, Tunable::Static(true));
    }

    #[test]
    fn script_tag_parses_as_script() {
        let y = "v: !script \"pattern_confirmed\"\n";
        let w: Wrap<bool> = serde_yaml::from_str(y).unwrap();
        assert_eq!(w.v, Tunable::from_script("pattern_confirmed"));
    }

    #[test]
    fn script_tag_with_complex_expression() {
        let y = "v: !script \"if r >= 3.0 { 1.0 } else { 0.5 }\"\n";
        let w: Wrap<f64> = serde_yaml::from_str(y).unwrap();
        match w.v {
            Tunable::Script(s) => assert_eq!(s.source, "if r >= 3.0 { 1.0 } else { 0.5 }"),
            _ => panic!("expected Script variant"),
        }
    }

    #[test]
    fn unknown_tag_errors() {
        let y = "v: !bogus \"foo\"\n";
        let err = serde_yaml::from_str::<Wrap<f64>>(y).unwrap_err();
        assert!(err.to_string().contains("unknown tag"));
    }

    #[test]
    fn script_tag_with_non_string_errors() {
        let y = "v: !script 1.0\n";
        let err = serde_yaml::from_str::<Wrap<f64>>(y).unwrap_err();
        assert!(err.to_string().contains("string scalar"));
    }

    #[test]
    fn static_round_trip_emits_bare_scalar() {
        let w: Wrap<f64> = Wrap {
            v: Tunable::Static(1.5),
        };
        let y = serde_yaml::to_string(&w).unwrap();
        assert_eq!(y.trim(), "v: 1.5");
    }

    #[test]
    fn script_round_trip_emits_script_tag() {
        let w: Wrap<bool> = Wrap {
            v: Tunable::from_script("pattern_confirmed"),
        };
        let y = serde_yaml::to_string(&w).unwrap();
        // Exact form depends on serde_yaml's tag emission; pin only the salient bits.
        assert!(y.contains("!script"), "expected !script tag in output: {y}");
        assert!(
            y.contains("pattern_confirmed"),
            "expected source in output: {y}"
        );

        // And it must round-trip back to the same Tunable.
        let parsed: Wrap<bool> = serde_yaml::from_str(&y).unwrap();
        assert_eq!(parsed.v, w.v);
    }

    #[test]
    fn as_static_extracts_only_for_static() {
        let s: Tunable<f64> = Tunable::Static(2.5);
        assert_eq!(s.as_static(), Some(&2.5));

        let scripted: Tunable<f64> = Tunable::from_script("1.0 + 1.0");
        assert_eq!(scripted.as_static(), None);
    }
}
