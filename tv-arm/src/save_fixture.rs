//! `--save-fixture` — the one-flag corpus capture.
//!
//! Capturing a trade for the corpus is six flags spread across two commands
//! (`--spec-out`, `--save-matrix`, then `replay --save <name> --simulate true`),
//! and five of them are the same every time. The odd one out is the fixture
//! **name**, which is exactly the part a human shouldn't have to invent 291
//! times.
//!
//! So this flag derives the name from what the setup already knows and fills in
//! the rest. It is pure argv rewriting: everything it does could be typed by
//! hand, and anything the operator *did* type wins.

use crate::args::Args;

/// The fixture name for a setup: `<instrument>-<granularity>-<YYYY-MM-DD>`.
///
/// Deterministic on purpose. Re-capturing the same setup on the same day
/// overwrites its own fixtures instead of growing `eurusd-h1-2`, `-3`, `-4` —
/// a re-capture is nearly always a *correction* (a line was wrong, a level was
/// missing), and the corrected one is the one you want.
///
/// The date is the **arm cursor** (`--start`) when there is one, not the wall
/// clock: a journaling re-arm of a June trade belongs to June. Without a cursor
/// (a live arm) there is no cursor date to use, so the caller supplies today's.
pub fn derive_name(instrument: &str, granularity: &str, date: &str) -> String {
    let slug = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
    };
    // Collapse the runs of `-` that separators like `XAU_XAG` / `GBP/AUD` /
    // `UK 100` produce, so the name stays readable as a directory.
    let joined = format!("{}-{}-{}", slug(instrument), slug(granularity), slug(date));
    let mut out = String::with_capacity(joined.len());
    let mut prev_dash = false;
    for c in joined.chars() {
        if c == '-' {
            if !prev_dash {
                out.push(c);
            }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}

/// Whether the operator's passthrough already sets `flag` (as `--flag` or
/// `--flag=value`), in which case we must not inject our default over it.
fn sets(passthrough: &[String], flag: &str) -> bool {
    passthrough
        .iter()
        .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
}

/// Fill in the `replay` passthrough for a `--save-fixture` capture.
///
/// Injects `--save <name>`, `--simulate true` and (when given) `--message`,
/// each only if absent. Returns the augmented token list.
pub fn replay_tokens(name: &str, message: Option<&str>, passthrough: &[String]) -> Vec<String> {
    let mut out = passthrough.to_vec();
    if !sets(passthrough, "--save") {
        out.push("--save".to_string());
        out.push(name.to_string());
    }
    if !sets(passthrough, "--simulate") {
        out.push("--simulate".to_string());
        out.push("true".to_string());
    }
    if let Some(msg) = message
        && !sets(passthrough, "--message")
    {
        out.push("--message".to_string());
        out.push(msg.to_string());
    }
    out
}

/// The spec path for a capture: `<name>.spec.json` beside the fixtures.
///
/// Kept next to the corpus rather than in a separate tree so a fixture and the
/// spec that can regenerate it travel together — losing the spec turns a free
/// offline re-run back into a manual chart session.
pub fn spec_path(fixtures_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    fixtures_dir.join(format!("{name}.spec.json"))
}

/// Resolve the fixture name for this run: the operator's explicit
/// `--save-fixture <name>`, or one derived from the setup.
///
/// A blank `--fixture-name` means "derive it" rather than erroring: an empty
/// value is a slip, and the derived name is always a usable answer.
pub fn resolve_name(args: &Args, instrument: &str, granularity: &str, date: &str) -> String {
    match args.fixture_name.as_deref() {
        Some(explicit) if !explicit.trim().is_empty() => explicit.trim().to_string(),
        _ => derive_name(instrument, granularity, date),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_names_are_readable_directory_slugs() {
        // The separators real broker symbols use must all collapse to single
        // dashes — these become directory names.
        assert_eq!(
            derive_name("EUR_USD", "h1", "2026-07-20"),
            "eur-usd-h1-2026-07-20"
        );
        assert_eq!(
            derive_name("GBP/AUD", "h1", "2026-06-19"),
            "gbp-aud-h1-2026-06-19"
        );
        assert_eq!(
            derive_name("UK 100", "h1", "2026-07-21"),
            "uk-100-h1-2026-07-21"
        );
        assert_eq!(
            derive_name("XAU_XAG", "h1", "2026-07-21"),
            "xau-xag-h1-2026-07-21"
        );
    }

    #[test]
    fn derived_names_never_have_doubled_or_edge_dashes() {
        // A doubled dash is legal in a path but reads as a typo; a leading or
        // trailing one is worse (`-eurusd` sorts oddly and looks like a flag).
        for (i, g, d) in [
            ("EUR__USD", "h1", "2026-07-20"),
            (" EUR/USD ", "h1", "2026-07-20"),
        ] {
            let name = derive_name(i, g, d);
            assert!(!name.contains("--"), "{name} has a doubled dash");
            assert!(
                !name.starts_with('-') && !name.ends_with('-'),
                "{name} has an edge dash"
            );
        }
    }

    #[test]
    fn the_same_setup_on_the_same_day_derives_the_same_name() {
        // Load-bearing: a re-capture must overwrite, not accumulate. If this
        // ever gains a timestamp or a counter, 291 trades become 291 piles.
        assert_eq!(
            derive_name("EUR_USD", "h1", "2026-07-20"),
            derive_name("EUR_USD", "h1", "2026-07-20")
        );
    }

    #[test]
    fn injects_the_save_and_simulate_defaults() {
        let got = replay_tokens("eur-usd-h1-2026-07-20", None, &[]);
        assert_eq!(
            got,
            vec!["--save", "eur-usd-h1-2026-07-20", "--simulate", "true"]
        );
    }

    #[test]
    fn the_operators_own_flags_win() {
        // The flag fills in defaults; it must never override an explicit choice.
        let passthrough: Vec<String> = ["--save", "my-name", "--simulate", "false"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let got = replay_tokens("derived", None, &passthrough);
        assert_eq!(
            got, passthrough,
            "nothing should be injected over explicit flags"
        );
    }

    #[test]
    fn an_equals_form_flag_also_counts_as_set() {
        // `--save=x` is the same choice as `--save x`; missing this would
        // inject a second `--save` and let replay-candles reject the run.
        let passthrough = vec!["--save=my-name".to_string()];
        let got = replay_tokens("derived", None, &passthrough);
        assert!(
            !got.iter().any(|t| t == "derived"),
            "must not inject a second --save: {got:?}"
        );
    }

    #[test]
    fn a_message_is_passed_through_to_the_fixture_meta() {
        let got = replay_tokens("n", Some("pins the S/R close"), &[]);
        let i = got
            .iter()
            .position(|t| t == "--message")
            .expect("--message injected");
        assert_eq!(got[i + 1], "pins the S/R close");
    }

    #[test]
    fn no_message_means_no_message_flag() {
        let got = replay_tokens("n", None, &[]);
        assert!(!got.iter().any(|t| t == "--message"));
    }

    #[test]
    fn the_spec_sits_beside_the_fixture_it_regenerates() {
        let p = spec_path(std::path::Path::new("/corpus"), "eur-usd-h1-2026-07-20");
        assert_eq!(
            p,
            std::path::PathBuf::from("/corpus/eur-usd-h1-2026-07-20.spec.json")
        );
    }
}
