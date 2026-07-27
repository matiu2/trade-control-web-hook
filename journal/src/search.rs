//! Incremental `/` filter over the plan list.
//!
//! One idea: given a query string and the rows, which rows match. The matching
//! is deliberately simple and predictable rather than fuzzy — the operator is
//! usually typing a fragment they can see on screen (`eur`, `h1`, `aud-cad`,
//! `await_entry`), and a fuzzy matcher would surface confusing hits for those.
//!
//! Rules:
//!
//! * **Case-insensitive substring** over the row's searchable text.
//! * **Space-separated terms are ANDed**, each matched independently and in any
//!   order — `eur h1` finds the EUR/USD H1 plans, `h1 eur` finds the same. This
//!   is the whole reason for not doing a plain `haystack.contains(query)`: the
//!   columns you want to combine aren't adjacent in the row.
//! * The haystack is the same text the list row shows (`trade_id`, instrument,
//!   granularity, phase) plus `account`, so what you see is what you can search.
//!   Separators are normalised (`_`/`/` → `-`) so `audcad`, `aud-cad`, `AUD_CAD`
//!   all match one another.
//!
//! [`SearchState`] holds the live query plus whether the prompt is open; `App`
//! owns one and derives its visible-row list from it (see `App::visible`).

use crate::plan::PlanRow;

/// The `/` search prompt's state. `active` is the typing mode (the prompt is
/// open and keys go to the query); the `query` survives closing the prompt so
/// the filter stays applied while you navigate — `Esc` clears it.
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    /// True while the operator is typing into the prompt.
    pub active: bool,
    /// The current query. Non-empty means the list is filtered, whether or not
    /// the prompt is still open.
    pub query: String,
}

impl SearchState {
    /// Open the prompt for typing, keeping any existing query so `/` then more
    /// typing refines rather than restarts.
    pub fn open(&mut self) {
        self.active = true;
    }

    /// Close the prompt but **keep** the filter applied (the Enter key).
    pub fn accept(&mut self) {
        self.active = false;
    }

    /// Close the prompt and drop the filter (the Esc key).
    pub fn clear(&mut self) {
        self.active = false;
        self.query.clear();
    }

    /// Append a typed character.
    pub fn push(&mut self, c: char) {
        self.query.push(c);
    }

    /// Backspace one character.
    pub fn pop(&mut self) {
        self.query.pop();
    }

    /// True when a filter is in effect (something typed), regardless of whether
    /// the prompt is still open.
    pub fn is_filtering(&self) -> bool {
        !self.query.trim().is_empty()
    }
}

/// The indices of `rows` matching `query`, in the original order. An empty /
/// whitespace-only query matches everything.
pub fn matching(rows: &[PlanRow], query: &str) -> Vec<usize> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| normalise(&t.to_lowercase()))
        .collect();
    if terms.is_empty() {
        return (0..rows.len()).collect();
    }
    rows.iter()
        .enumerate()
        .filter(|(_, row)| {
            let hay = haystack(row);
            terms.iter().all(|t| hay.contains(t.as_str()))
        })
        .map(|(i, _)| i)
        .collect()
}

/// The searchable text of a row: everything the list shows, plus the account.
/// Lower-cased and separator-normalised so `AUD_CAD` matches `aud-cad`.
fn haystack(row: &PlanRow) -> String {
    let joined = format!(
        "{} {} {} {} {} {}",
        row.trade_id,
        row.instrument,
        row.granularity,
        row.phase.as_deref().unwrap_or(""),
        row.account,
        if row.is_archived() { "archived" } else { "" },
    );
    normalise(&joined.to_lowercase())
}

/// Fold the separators plans use interchangeably (`_`, `/`) onto `-`, so a
/// query typed in any of the broker spellings matches the stored one.
fn normalise(s: &str) -> String {
    s.replace(['_', '/'], "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(trade_id: &str, instrument: &str, granularity: &str, phase: &str) -> PlanRow {
        PlanRow {
            trade_id: trade_id.to_string(),
            account: "demo".into(),
            instrument: instrument.to_string(),
            granularity: granularity.to_string(),
            phase: Some(phase.to_string()),
            shadow: false,
            archived_at: None,
            watermark: None,
        }
    }

    fn fixture() -> Vec<PlanRow> {
        vec![
            row("hs-eur-usd-1111", "EUR_USD", "h1", "await_entry"),
            row("hs-aud-cad-2222", "AUD_CAD", "h1", "done"),
            row("mw-eur-usd-3333", "EUR_USD", "h4", "await_break_and_close"),
        ]
    }

    /// An empty (or whitespace-only) query is "no filter" — everything shows.
    #[test]
    fn empty_query_matches_everything() {
        let rows = fixture();
        assert_eq!(matching(&rows, ""), vec![0, 1, 2]);
        assert_eq!(matching(&rows, "   "), vec![0, 1, 2]);
    }

    /// A substring hits any searchable column, case-insensitively.
    #[test]
    fn matches_substring_case_insensitively_across_columns() {
        let rows = fixture();
        assert_eq!(matching(&rows, "EUR"), vec![0, 2], "instrument");
        assert_eq!(matching(&rows, "eur"), vec![0, 2], "lower-cased query");
        assert_eq!(matching(&rows, "mw-"), vec![2], "trade_id");
        assert_eq!(matching(&rows, "done"), vec![1], "phase");
        assert_eq!(matching(&rows, "demo"), vec![0, 1, 2], "account");
    }

    /// Space-separated terms are ANDed and order-independent — the point of not
    /// doing a plain `contains`, since the columns aren't adjacent.
    #[test]
    fn terms_are_anded_in_any_order() {
        let rows = fixture();
        assert_eq!(matching(&rows, "eur h4"), vec![2]);
        assert_eq!(matching(&rows, "h4 eur"), vec![2], "order-independent");
        assert_eq!(matching(&rows, "eur h1"), vec![0]);
        // A term that matches nothing kills the whole row, even if others hit.
        assert!(matching(&rows, "eur nonsense").is_empty());
    }

    /// Separator spellings are interchangeable, so an operator can type the
    /// instrument the way any broker writes it.
    #[test]
    fn separators_are_interchangeable() {
        let rows = fixture();
        for q in ["aud_cad", "aud-cad", "AUD/CAD"] {
            assert_eq!(matching(&rows, q), vec![1], "query {q}");
        }
    }

    /// No match is an empty list, not "everything" — a filter that finds nothing
    /// must show nothing rather than silently disabling itself.
    #[test]
    fn no_match_yields_empty() {
        assert!(matching(&fixture(), "zzz").is_empty());
    }

    /// `archived` is searchable so the operator can isolate terminated plans.
    #[test]
    fn archived_is_searchable() {
        let mut rows = fixture();
        rows[1].archived_at = Some("2026-07-01T00:00:00Z".into());
        assert_eq!(matching(&rows, "archived"), vec![1]);
    }

    /// State transitions: Enter keeps the filter, Esc drops it.
    #[test]
    fn accept_keeps_filter_esc_clears_it() {
        let mut s = SearchState::default();
        s.open();
        s.push('e');
        s.push('u');
        assert!(s.active && s.is_filtering());
        s.accept();
        assert!(!s.active, "prompt closed");
        assert!(s.is_filtering(), "filter still applied after Enter");
        s.pop();
        assert_eq!(s.query, "e");
        s.clear();
        assert!(!s.active && !s.is_filtering(), "Esc drops the filter");
    }
}
