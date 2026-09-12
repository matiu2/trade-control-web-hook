//! The loud, coloured "which pattern did this arm actually build?" banner.
//!
//! Why this exists: `tv-arm ... register` picks H&S vs M/W *implicitly*, from
//! whether a path drawing is present on the chart
//! (`pipeline::arm_from_inputs`, `geom.mw_path.is_some()`). When the operator
//! meant to arm an H&S and a stray path drawing was left on the chart, the arm
//! silently builds an M/W instead — a completely different trade (different
//! geometry, different entry/SL/TP, no prep rules). The only prior evidence was
//! `pattern = M` buried in one `tracing` field among a page of INFO lines.
//!
//! So the family is printed as a banner, twice: once as soon as it is decided,
//! and once at the very end of the arm, on the theory that the operator's eye
//! lands on the top or the bottom of the scrollback but rarely the middle. The
//! two families get **different colours** as well as different words, so the
//! distinction survives being read at a glance: cyan for H&S, magenta for M/W.
//!
//! Written to **stderr**, deliberately — the same stream `tracing` uses, so the
//! banner keeps its position relative to the surrounding log lines. Anything
//! machine-readable this tool emits (plan JSON, matrix summary) goes to stdout
//! or a file and stays unpolluted.

use std::io::IsTerminal;

use trade_control_cli::TradePattern;

/// Which of the two pattern *families* an arm built.
///
/// The operator's confusion is family-level ("I asked for H&S and got M/W"),
/// not variant-level — `Hs`/`Ihs` differ only in direction, and so do `M`/`W`.
/// So the banner's colour and headline key on the family, and the specific
/// variant rides along in the detail line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatternFamily {
    /// Head & Shoulders / inverse Head & Shoulders.
    HeadAndShoulders,
    /// M-top / W-bottom.
    MW,
}

impl PatternFamily {
    pub(crate) fn of(pattern: TradePattern) -> Self {
        match pattern {
            TradePattern::Hs | TradePattern::Ihs => Self::HeadAndShoulders,
            TradePattern::M | TradePattern::W => Self::MW,
        }
    }

    /// The headline word. Short and unmistakable at a glance.
    fn headline(self) -> &'static str {
        match self {
            Self::HeadAndShoulders => "H&S",
            Self::MW => "M/W",
        }
    }

    /// Why this family was chosen — the actionable half when the banner is a
    /// surprise. The discriminant is a single chart drawing
    /// (`PlanGeometry.mw_path`, from `roles.mw_path`), so "you left a path
    /// drawing on the chart" is both the cause and the fix.
    fn reason(self) -> &'static str {
        match self {
            Self::HeadAndShoulders => "no path drawing on the chart",
            Self::MW => "a PATH drawing is on the chart (remove it for H&S)",
        }
    }

    /// SGR parameters: bold, plus a family-distinct foreground colour.
    ///
    /// Cyan vs magenta rather than, say, red/green: neither reads as
    /// "error"/"success" (this banner reports neither), they stay clearly
    /// distinct on both light and dark terminals, and they are not the colours
    /// `tracing` uses for its own INFO/WARN levels.
    fn sgr(self) -> &'static str {
        match self {
            // bold bright-cyan
            Self::HeadAndShoulders => "1;96",
            // bold bright-magenta
            Self::MW => "1;95",
        }
    }
}

/// Where in the arm a banner is being printed. Only changes the wording — the
/// family, colour, and prominence are identical at both ends, because the whole
/// point is that either one alone is enough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Position {
    /// Printed as soon as the pattern is resolved, before the bundle is built.
    Top,
    /// Printed after the arm has finished its work.
    Bottom,
}

impl Position {
    fn verb(self) -> &'static str {
        match self {
            Self::Top => "ARMING",
            Self::Bottom => "ARMED",
        }
    }
}

/// Render the banner as it should appear on a colour-capable terminal.
///
/// Pure (no I/O, no terminal probe) so the wording and the escape sequences are
/// both unit-testable; [`print`] is the thin I/O shell.
///
/// `colour: false` renders the identical text with no escape sequences, for a
/// pipe or a redirect to a file — where a raw `\x1b[1;96m` is noise, and worse,
/// can end up pasted into a bug report.
pub(crate) fn render(
    pattern: TradePattern,
    position: Position,
    instrument: &str,
    colour: bool,
) -> String {
    let family = PatternFamily::of(pattern);
    let headline = format!(
        "  {} {} TRADE — {instrument}  ",
        position.verb(),
        family.headline(),
    );
    let rule = "━".repeat(headline.chars().count());
    // The variant line spells out the full pattern (`m — M-top (short)`), so the
    // banner answers "which family" at a glance AND "exactly which pattern"
    // on a second read.
    let detail = format!("  pattern: {}", pattern.label());
    // Why this family, so a surprised operator knows what to change.
    let because = format!("  because: {}", family.reason());

    let body = format!("{rule}\n{headline}\n{detail}\n{because}\n{rule}");
    if colour {
        format!("\x1b[{}m{body}\x1b[0m", family.sgr())
    } else {
        body
    }
}

/// Print the banner to stderr, colouring only when stderr is a terminal.
pub(crate) fn print(pattern: TradePattern, position: Position, instrument: &str) {
    let colour = std::io::stderr().is_terminal();
    eprintln!("{}", render(pattern, position, instrument, colour));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the feature: the two families must not be
    /// distinguishable only by a word the eye can skim past. Assert they differ
    /// in BOTH the headline text and the colour.
    #[test]
    fn the_two_families_differ_in_word_and_colour() {
        let hs = render(TradePattern::Hs, Position::Top, "EUR_USD", true);
        let mw = render(TradePattern::M, Position::Top, "EUR_USD", true);

        // Scoped to the HEADLINE line, not the whole banner: the M/W banner's
        // advice line legitimately says "remove it for H&S", and that advice is
        // the useful half. What must never be ambiguous is the headline.
        let headline_of = |b: &str| {
            b.lines()
                .find(|l| l.contains("ARMING") || l.contains("ARMED"))
                .expect("every banner has a headline")
                .to_string()
        };
        let (hs_head, mw_head) = (headline_of(&hs), headline_of(&mw));

        assert!(hs_head.contains("H&S"), "H&S headline says H&S: {hs_head}");
        assert!(mw_head.contains("M/W"), "M/W headline says M/W: {mw_head}");
        assert!(
            !hs_head.contains("M/W"),
            "an H&S headline must never mention M/W: {hs_head}"
        );
        assert!(
            !mw_head.contains("H&S"),
            "an M/W headline must never mention H&S: {mw_head}"
        );

        assert!(hs.contains("\x1b[1;96m"), "H&S is bold cyan: {hs:?}");
        assert!(mw.contains("\x1b[1;95m"), "M/W is bold magenta: {mw:?}");
        assert_ne!(
            PatternFamily::of(TradePattern::Hs).sgr(),
            PatternFamily::of(TradePattern::M).sgr(),
            "the colours must actually differ, or the banner only distinguishes by word",
        );
    }

    /// Both variants of a family share its colour and headline — the operator's
    /// mix-up is H&S-vs-M/W, not Hs-vs-Ihs.
    #[test]
    fn variants_within_a_family_share_colour_and_headline() {
        for (a, b) in [
            (TradePattern::Hs, TradePattern::Ihs),
            (TradePattern::M, TradePattern::W),
        ] {
            assert_eq!(PatternFamily::of(a), PatternFamily::of(b));
        }
        assert_ne!(
            PatternFamily::of(TradePattern::Ihs),
            PatternFamily::of(TradePattern::W),
            "iH&S and W are both LONG but different families — direction must not \
             be what the banner keys on",
        );
    }

    /// The exact variant still has to be recoverable, so the banner is useful
    /// for "wait, is this the short or the long one?" too.
    #[test]
    fn the_specific_variant_is_spelled_out() {
        let w = render(TradePattern::W, Position::Bottom, "AUD_JPY", false);
        assert!(
            w.contains("W-bottom"),
            "banner carries the full pattern label: {w}"
        );
        assert!(w.contains("AUD_JPY"), "banner names the instrument: {w}");
    }

    /// Top and bottom are the same banner with different verbs — so a reader
    /// who sees only one of them still gets the family, and a reader who sees
    /// both can tell which end they're looking at.
    #[test]
    fn top_and_bottom_differ_only_in_verb() {
        let top = render(TradePattern::Hs, Position::Top, "EUR_USD", false);
        let bottom = render(TradePattern::Hs, Position::Bottom, "EUR_USD", false);
        assert!(top.contains("ARMING"), "{top}");
        assert!(bottom.contains("ARMED"), "{bottom}");
        // Same family, same colour, same instrument, same variant label at both
        // ends — only the verb moves. Compared with the rules and whitespace
        // stripped, since the headline is padded to the rule's width and the two
        // verbs differ in length.
        let strip = |s: &str| s.replace('\u{2501}', "").replace([' ', '\n'], "");
        assert_eq!(
            strip(&top.replace("ARMING", "VERB")),
            strip(&bottom.replace("ARMED", "VERB")),
            "the two ends must not drift into saying different things",
        );
    }

    /// The banner names the DISCRIMINANT, because "this built an M/W" is only
    /// half an answer — the operator needs to know a path drawing is what did
    /// it, and that removing it is the fix.
    #[test]
    fn the_banner_says_why_this_family_was_chosen() {
        let mw = render(TradePattern::M, Position::Top, "EUR_USD", false);
        assert!(
            mw.contains("PATH drawing"),
            "an M/W banner must name the path drawing as the cause: {mw}"
        );
        let hs = render(TradePattern::Hs, Position::Top, "EUR_USD", false);
        assert!(
            hs.contains("no path drawing"),
            "an H&S banner must say the path drawing was absent: {hs}"
        );
        assert_ne!(
            PatternFamily::HeadAndShoulders.reason(),
            PatternFamily::MW.reason(),
            "the two reasons must differ, or the line says nothing",
        );
    }

    /// A redirect to a file or a pipe must not get escape sequences — they end
    /// up pasted into bug reports and make the output unsearchable.
    #[test]
    fn no_escape_sequences_when_not_a_terminal() {
        let plain = render(TradePattern::M, Position::Top, "EUR_USD", false);
        assert!(
            !plain.contains('\x1b'),
            "non-terminal render carries no ANSI: {plain:?}"
        );
        assert!(
            plain.contains("M/W"),
            "…but still says which family it is: {plain}"
        );
    }
}
