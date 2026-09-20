//! A one-line modal text prompt.
//!
//! One idea: collect a single line of text from the operator, then hand it to
//! whatever asked for it. Today that is the fixture-capture `--message` — the
//! note that says *why* a fixture exists, which `--rebless` deliberately never
//! overwrites, so it is written once at capture time or not at all.
//!
//! # Why not reuse the `/` search prompt
//!
//! [`crate::search::SearchState`] looks like the same widget and is not. It is
//! **list-scoped and live**: every keystroke re-filters the visible rows, `Esc`
//! means "drop the filter", and its query survives the prompt closing because
//! the filter outlives the typing. A message prompt has none of that — it
//! filters nothing, it is modal on any screen, `Esc` means "cancel the whole
//! action", and its text is consumed once and discarded. Sharing one struct
//! would mean a mode flag threaded through both behaviours, so they stay
//! separate.
//!
//! # The pending action is carried, not remembered
//!
//! The prompt owns [`Prompt::pending`] — what to do with the text once it is
//! accepted. Keeping it *in* the prompt rather than in a side field on `App`
//! means an abandoned prompt cannot leave a stale "…and then capture a
//! fixture" intention behind: cancelling drops the prompt and the intent
//! together.

/// What the app should do with an accepted prompt line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// Capture the fixture grid for this trade, with the typed text as the
    /// capture's `--message`.
    SaveFixture { trade_id: String },
}

/// An open one-line text prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    /// Shown above the input, e.g. "fixture message".
    pub title: String,
    /// What has been typed so far.
    pub value: String,
    /// What to run when the operator accepts.
    pub pending: Pending,
}

impl Prompt {
    pub fn new(title: impl Into<String>, pending: Pending) -> Self {
        Self {
            title: title.into(),
            value: String::new(),
            pending,
        }
    }

    pub fn push(&mut self, c: char) {
        self.value.push(c);
    }

    pub fn pop(&mut self) {
        self.value.pop();
    }

    /// The typed text, trimmed, or `None` when nothing meaningful was typed.
    ///
    /// Whitespace-only is `None` rather than `Some("   ")`: the message flows
    /// into `tv-arm --message`, and a blank one would be recorded in the
    /// fixture's `meta.json` as if it said something. Absent is honest; a
    /// string of spaces is not.
    pub fn text(&self) -> Option<&str> {
        let t = self.value.trim();
        (!t.is_empty()).then_some(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt() -> Prompt {
        Prompt::new(
            "fixture message",
            Pending::SaveFixture {
                trade_id: "hs-aud-nzd-ff8e66e8".into(),
            },
        )
    }

    #[test]
    fn typing_and_backspace_build_the_line() {
        let mut p = prompt();
        for c in "why".chars() {
            p.push(c);
        }
        p.pop();
        p.push('y');
        assert_eq!(p.value, "why");
        assert_eq!(p.text(), Some("why"));
    }

    /// A blank or whitespace-only line must read as "no message", so the
    /// capture omits `--message` entirely rather than recording an empty note
    /// in the fixture's `meta.json` — which `--rebless` would then preserve
    /// forever.
    #[test]
    fn a_blank_line_is_no_message() {
        let mut p = prompt();
        assert_eq!(p.text(), None, "untouched prompt");
        for c in "   ".chars() {
            p.push(c);
        }
        assert_eq!(p.text(), None, "whitespace only");
    }

    /// Surrounding whitespace is trimmed — the operator's stray trailing space
    /// shouldn't land in the committed corpus.
    #[test]
    fn the_text_is_trimmed() {
        let mut p = prompt();
        for c in "  news spike  ".chars() {
            p.push(c);
        }
        assert_eq!(p.text(), Some("news spike"));
    }

    /// The pending action rides on the prompt, so the trade it was opened for
    /// is the trade it acts on — even if the selection moves while typing.
    #[test]
    fn the_prompt_carries_its_trade() {
        let p = prompt();
        assert_eq!(
            p.pending,
            Pending::SaveFixture {
                trade_id: "hs-aud-nzd-ff8e66e8".into()
            }
        );
    }
}
