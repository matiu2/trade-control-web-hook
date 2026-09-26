//! Keyboard → [`Action`] mapping. Modal/popup states swallow most keys, so the
//! mapping is context-sensitive on the app state.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;
use crate::screen::Screen;

/// A resolved intent from a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    SelectNext,
    SelectPrev,
    /// Push deeper (list→timeline→replay→compare) — also the `n` "next/drill".
    Deeper,
    /// Pop one screen shallower.
    Shallower,
    LoadTv,
    Replay,
    /// Capture the fixture-grid corpus for the current trade — the `s` key.
    /// Runs `tv-arm --save-fixture … replay`, which writes JSON fixtures under
    /// `replay-fixtures/` (committed to git). Replaced the old journal-DB
    /// "record" action on 2026-07-29.
    ///
    /// The grid is **eight cells** — four entry rules (`normal`, `skip-bcr`,
    /// `strategy-v2`, `strategy-v2-qm-market`) × news on/off — not the six it
    /// launched with. The count isn't restated in the status message; that
    /// reports what actually landed on disk (see `App::apply_job`).
    SaveFixture,
    /// The `f` key: re-bless this plan's saved fixture cells, or — when it has
    /// none — prompt for a message and capture one. Which arm runs is decided
    /// by the fixture status the info bar is already showing, so the key does
    /// what the operator just read.
    FixtureKey,
    /// The `a` key: redraw this plan's ARM geometry on local-chart, recovered
    /// from the stored plan's own rule triggers. For a chart whose drawings are
    /// gone, so `/arm-setup` answers `422 missing required roles` even though
    /// every level is still in the plan.
    DrawGeometry,
    /// The `R` key: replay the STORED plan as-is (`replay-candles --plan`),
    /// with no chart read and no tv-arm re-arm. Capital-R deliberately: it is
    /// a different question from `r`, not a variation of it.
    RawReplay,
    /// Type a character into the open one-line prompt.
    PromptPush(char),
    /// Backspace one character in the open prompt.
    PromptPop,
    /// Accept the prompt and run its pending action (Enter).
    PromptAccept,
    /// Cancel the prompt, dropping the text and the action (Esc).
    PromptCancel,
    TogglePopup,
    RequestDelete,
    ConfirmYes,
    ConfirmNo,
    /// Scroll the detail popup by N lines (negative = up).
    PopupScroll(i32),
    PopupHome,
    PopupEnd,
    /// Scroll the Replay report by N lines (negative = up).
    ReplayScroll(i32),
    ReplayHome,
    ReplayEnd,
    /// Force a full-screen repaint (Ctrl-L) — clears any residual corruption.
    Redraw,
    /// Copy the full content of the current view to the clipboard (the `c` key).
    Copy,
    /// Open the current plan's arm-time TradingView screenshot in the browser
    /// (the `o` key). No-op when the plan carries no screenshot URL.
    OpenScreenshot,
    /// Open the `/` search prompt on the list.
    SearchOpen,
    /// Type a character into the live search query.
    SearchPush(char),
    /// Backspace one character from the search query.
    SearchPop,
    /// Close the prompt, keeping the filter (Enter).
    SearchAccept,
    /// Close the prompt and drop the filter (Esc).
    SearchClear,
    None,
}

/// Map a key event to an action, given the current app state.
pub fn map_key(app: &App, key: KeyEvent) -> Action {
    // Ctrl-C always quits.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Quit;
    }
    // Ctrl-L always forces a full repaint (recovers from residual corruption),
    // on any screen or modal.
    if key.code == KeyCode::Char('l') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Redraw;
    }

    // An open one-line prompt (the fixture message) is a text field and must
    // swallow every printable key, exactly as the `/` search prompt does — a
    // typed `q` would otherwise quit mid-message. It sits ABOVE the search
    // block because it is modal on every screen, including the list where a
    // search prompt could also exist; only one can be open at a time, and this
    // one wins while it is.
    if app.prompt.is_some() {
        return match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                Action::PromptPush(c)
            }
            KeyCode::Backspace => Action::PromptPop,
            KeyCode::Enter => Action::PromptAccept,
            KeyCode::Esc => Action::PromptCancel,
            _ => Action::None,
        };
    }

    // An open `/` search prompt is a text field: it swallows every printable
    // key so typing a query can't trigger a command binding (`q` would quit,
    // `d` would delete). Only Enter/Esc/Backspace and the arrows escape it —
    // the arrows so you can move the selection while still refining the query.
    // This must sit above the confirm/popup blocks: the prompt is only open on
    // the list screen, where neither of those can be showing.
    if app.search.active {
        return match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                Action::SearchPush(c)
            }
            KeyCode::Backspace => Action::SearchPop,
            KeyCode::Enter => Action::SearchAccept,
            KeyCode::Esc => Action::SearchClear,
            KeyCode::Up => Action::SelectPrev,
            KeyCode::Down => Action::SelectNext,
            KeyCode::Right => Action::Deeper,
            _ => Action::None,
        };
    }

    // A pending confirm modal only listens for y/n/esc.
    if app.confirm.is_some() {
        return match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Action::ConfirmYes,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Action::ConfirmNo,
            _ => Action::None,
        };
    }

    // The detail popup is modal and scrollable: i/esc/q close it; arrows + vim
    // keys + page/home/end scroll it. One page ≈ 20 lines (the render clamps the
    // bottom, so an over-scroll just pins to the last page).
    if app.show_popup {
        const PAGE: i32 = 20;
        return match key.code {
            KeyCode::Char('i') | KeyCode::Char('I') | KeyCode::Esc | KeyCode::Char('q') => {
                Action::TogglePopup
            }
            KeyCode::Up | KeyCode::Char('k') => Action::PopupScroll(-1),
            KeyCode::Down | KeyCode::Char('j') => Action::PopupScroll(1),
            KeyCode::PageUp | KeyCode::Char('u') => Action::PopupScroll(-PAGE),
            KeyCode::PageDown | KeyCode::Char('d') | KeyCode::Char(' ') => {
                Action::PopupScroll(PAGE)
            }
            KeyCode::Home | KeyCode::Char('g') => Action::PopupHome,
            KeyCode::End | KeyCode::Char('G') => Action::PopupEnd,
            KeyCode::Char('c') => Action::Copy,
            _ => Action::None,
        };
    }

    // The Replay screen scrolls its (long) report: arrows + vim + page/home/end,
    // the same bindings as the detail popup. The vim scroll keys (j/k/u/d/g/G)
    // and arrows override their list-navigation meanings here (there's no list to
    // move), so delete on this screen is `x` only (not `d`, which pages down).
    // `←` back, `→`/`n` deeper, `r`/`l`/`i`/`x`/`q` keep working.
    if app.screen == Screen::Replay {
        const PAGE: i32 = 20;
        return match key.code {
            KeyCode::Up | KeyCode::Char('k') => Action::ReplayScroll(-1),
            KeyCode::Down | KeyCode::Char('j') => Action::ReplayScroll(1),
            KeyCode::PageUp | KeyCode::Char('u') => Action::ReplayScroll(-PAGE),
            KeyCode::PageDown | KeyCode::Char(' ') => Action::ReplayScroll(PAGE),
            KeyCode::Home | KeyCode::Char('g') => Action::ReplayHome,
            KeyCode::End | KeyCode::Char('G') => Action::ReplayEnd,
            KeyCode::Right | KeyCode::Enter | KeyCode::Char('n') => Action::Deeper,
            KeyCode::Left => Action::Shallower,
            KeyCode::Char('l') => Action::LoadTv,
            KeyCode::Char('r') => Action::Replay,
            KeyCode::Char('R') => Action::RawReplay,
            KeyCode::Char('a') => Action::DrawGeometry,
            KeyCode::Char('f') | KeyCode::Char('F') => Action::FixtureKey,
            KeyCode::Char('s') | KeyCode::Char('S') => Action::SaveFixture,
            KeyCode::Char('c') => Action::Copy,
            KeyCode::Char('o') => Action::OpenScreenshot,
            KeyCode::Char('i') => Action::TogglePopup,
            KeyCode::Char('x') => Action::RequestDelete,
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            _ => Action::None,
        };
    }

    match key.code {
        // `/` opens the search prompt (list only; `open_search` no-ops elsewhere).
        KeyCode::Char('/') => Action::SearchOpen,
        // With a filter applied but the prompt closed, Esc clears the filter
        // instead of quitting — otherwise the only way out of a filter is to
        // reopen the prompt, and a stray Esc would exit the app unexpectedly.
        KeyCode::Esc if app.search.is_filtering() => Action::SearchClear,
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Up | KeyCode::Char('k') => Action::SelectPrev,
        KeyCode::Down | KeyCode::Char('j') => Action::SelectNext,
        KeyCode::Right | KeyCode::Enter | KeyCode::Char('n') => Action::Deeper,
        KeyCode::Left => Action::Shallower,
        KeyCode::Char('l') => Action::LoadTv,
        KeyCode::Char('r') => Action::Replay,
        KeyCode::Char('R') => Action::RawReplay,
        KeyCode::Char('f') | KeyCode::Char('F') => Action::FixtureKey,
        KeyCode::Char('s') | KeyCode::Char('S') => Action::SaveFixture,
        KeyCode::Char('c') => Action::Copy,
        KeyCode::Char('o') => Action::OpenScreenshot,
        KeyCode::Char('i') => Action::TogglePopup,
        KeyCode::Char('a') => Action::DrawGeometry,
        KeyCode::Char('d') | KeyCode::Char('x') => Action::RequestDelete,
        _ => Action::None,
    }
}

/// Apply an action to the app.
pub fn apply(app: &mut App, action: Action) {
    match action {
        Action::Quit => app.should_quit = true,
        Action::SelectNext => app.select_next(),
        Action::SelectPrev => app.select_prev(),
        Action::Deeper => app.push_deeper(),
        Action::Shallower => app.pop_shallower(),
        Action::LoadTv => app.load_tv(),
        Action::Replay => app.rerun_replay(),
        Action::SaveFixture => app.save_fixture_current(),
        Action::FixtureKey => app.fixture_key(),
        Action::RawReplay => app.raw_replay_current(),
        Action::DrawGeometry => app.draw_geometry_current(),
        Action::PromptPush(c) => app.prompt_push(c),
        Action::PromptPop => app.prompt_pop(),
        Action::PromptAccept => app.prompt_accept(),
        Action::PromptCancel => app.prompt_cancel(),
        Action::TogglePopup => app.toggle_popup(),
        Action::RequestDelete => app.request_delete(),
        Action::ConfirmYes => app.resolve_confirm(true),
        Action::ConfirmNo => app.resolve_confirm(false),
        Action::PopupScroll(delta) => app.scroll_popup(delta),
        Action::PopupHome => app.scroll_popup_home(),
        Action::PopupEnd => app.scroll_popup_end(),
        Action::ReplayScroll(delta) => app.scroll_replay(delta),
        Action::ReplayHome => app.scroll_replay_home(),
        Action::ReplayEnd => app.scroll_replay_end(),
        Action::Redraw => app.request_redraw(),
        Action::Copy => app.copy_current(),
        Action::OpenScreenshot => app.open_screenshot(),
        Action::SearchOpen => app.open_search(),
        Action::SearchPush(c) => app.search_push(c),
        Action::SearchPop => app.search_pop(),
        Action::SearchAccept => app.search_accept(),
        Action::SearchClear => app.search_clear(),
        Action::None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanRow;

    fn app_with_one_plan() -> App {
        App::from_rows(vec![PlanRow {
            trade_id: "hs-eur-usd-1".into(),
            account: "demo".into(),
            instrument: "EUR_USD".into(),
            granularity: "h1".into(),
            phase: Some("done".into()),
            shadow: false,
            archived_at: None,
            watermark: None,
        }])
    }

    fn press(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// `/` opens the prompt from the list.
    #[test]
    fn slash_opens_search_on_the_list() {
        let app = app_with_one_plan();
        assert_eq!(map_key(&app, press('/')), Action::SearchOpen);
    }

    /// **The** thing that would break the feature: while typing a query, letters
    /// that are command bindings elsewhere (`q` quit, `d` delete, `r` replay,
    /// `c` copy) must be captured as text, not fire their commands.
    #[test]
    fn typing_in_the_prompt_never_triggers_commands() {
        let mut app = app_with_one_plan();
        app.open_search();
        for c in ['q', 'd', 'r', 'c', 'l', 's', 'i', 'n', 'x', 'o', '/'] {
            assert_eq!(
                map_key(&app, press(c)),
                Action::SearchPush(c),
                "'{c}' must be typed into the query, not run as a command"
            );
        }
    }

    /// Ctrl-C still quits even mid-query — a text field must not trap the user.
    #[test]
    fn ctrl_c_still_quits_while_typing() {
        let mut app = app_with_one_plan();
        app.open_search();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(map_key(&app, ctrl_c), Action::Quit);
    }

    /// Enter keeps the filter, Esc drops it, Backspace edits.
    #[test]
    fn prompt_edit_keys() {
        let mut app = app_with_one_plan();
        app.open_search();
        let key = |code| map_key(&app, KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(key(KeyCode::Enter), Action::SearchAccept);
        assert_eq!(key(KeyCode::Esc), Action::SearchClear);
        assert_eq!(key(KeyCode::Backspace), Action::SearchPop);
        // Arrows still navigate so you can pick a row without closing the prompt.
        assert_eq!(key(KeyCode::Down), Action::SelectNext);
        assert_eq!(key(KeyCode::Up), Action::SelectPrev);
    }

    /// With the prompt closed but a filter applied, Esc clears the filter rather
    /// than quitting the app (a stray Esc shouldn't drop you out).
    #[test]
    fn esc_clears_an_applied_filter_instead_of_quitting() {
        let mut app = app_with_one_plan();
        app.open_search();
        app.search_push('e');
        app.search_accept(); // prompt closed, filter still on
        assert!(!app.search.active && app.search.is_filtering());
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(map_key(&app, esc), Action::SearchClear);
        // With no filter, Esc quits as before.
        app.search_clear();
        assert_eq!(map_key(&app, esc), Action::Quit);
    }

    /// **The** thing that would break the message prompt: while typing a
    /// reason, letters that are command bindings must be captured as text.
    /// `f` would re-open the prompt, `q` would quit, `x` would ask to delete,
    /// `R` would fire a raw replay — mid-message.
    #[test]
    fn typing_in_the_message_prompt_never_triggers_commands() {
        let mut app = app_with_one_plan();
        app.prompt = Some(crate::prompt::Prompt::new(
            "fixture message",
            crate::prompt::Pending::SaveFixture {
                trade_id: "hs-eur-usd-1".into(),
            },
        ));
        for c in [
            'q', 'd', 'r', 'R', 'c', 'l', 's', 'i', 'n', 'x', 'o', 'f', '/',
        ] {
            assert_eq!(
                map_key(&app, press(c)),
                Action::PromptPush(c),
                "'{c}' must be typed into the message, not run as a command"
            );
        }
        // Enter accepts, Esc cancels, Backspace edits.
        let key = |code| map_key(&app, KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(key(KeyCode::Enter), Action::PromptAccept);
        assert_eq!(key(KeyCode::Esc), Action::PromptCancel);
        assert_eq!(key(KeyCode::Backspace), Action::PromptPop);
        // Ctrl-C still escapes — a text field must never trap the operator.
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(map_key(&app, ctrl_c), Action::Quit);
    }

    /// `a` (draw arm geometry) works on BOTH screens, for the same reason `f`
    /// and `R` do: the Replay screen rebinds most letters for scrolling, so a
    /// binding added only to the list would be dead exactly where the operator
    /// has just read a replay and wants the chart redrawn.
    #[test]
    fn the_draw_geometry_key_works_on_both_screens() {
        let mut app = app_with_one_plan();
        app.screen = crate::screen::Screen::List;
        assert_eq!(map_key(&app, press('a')), Action::DrawGeometry, "list");
        app.screen = crate::screen::Screen::Replay;
        assert_eq!(map_key(&app, press('a')), Action::DrawGeometry, "replay");
    }

    /// `a` must not have stolen a scroll key. The Replay screen binds
    /// `j/k/u/d/g/G` and space to scrolling, and `g` was the first choice for
    /// "geometry" — the compiler caught it as an unreachable arm, and this keeps
    /// it caught if someone moves the binding back.
    #[test]
    fn the_replay_screens_scroll_keys_are_untouched() {
        let mut app = app_with_one_plan();
        app.screen = crate::screen::Screen::Replay;
        assert_eq!(map_key(&app, press('g')), Action::ReplayHome, "g scrolls");
        assert_eq!(map_key(&app, press('G')), Action::ReplayEnd, "G scrolls");
        assert_eq!(map_key(&app, press('j')), Action::ReplayScroll(1));
        assert_eq!(map_key(&app, press('k')), Action::ReplayScroll(-1));
    }

    /// `a` is not one shift-press away from a destructive key. `d`/`x` delete,
    /// so `D` was rejected as the binding — pinned so a future rename does not
    /// quietly put "draw" next to "delete".
    #[test]
    fn the_draw_geometry_key_is_not_adjacent_to_delete() {
        let mut app = app_with_one_plan();
        app.screen = crate::screen::Screen::List;
        assert_eq!(map_key(&app, press('d')), Action::RequestDelete);
        assert_ne!(
            map_key(&app, press('A')),
            Action::RequestDelete,
            "shift-a must not delete"
        );
    }

    /// `f` and `R` work on BOTH the list and the Replay screen — the Replay
    /// screen rebinds most letters for scrolling, so a binding added only to
    /// the list would be dead exactly where the fixture status is displayed.
    #[test]
    fn the_fixture_and_raw_replay_keys_work_on_both_screens() {
        let mut app = app_with_one_plan();
        for (upper, action) in [('F', Action::FixtureKey), ('R', Action::RawReplay)] {
            let lower = upper.to_ascii_lowercase();
            app.screen = crate::screen::Screen::List;
            assert_eq!(map_key(&app, press(upper)), action, "list, '{upper}'");
            app.screen = crate::screen::Screen::Replay;
            assert_eq!(map_key(&app, press(upper)), action, "replay, '{upper}'");
            // `f` is case-insensitive; `r`/`R` are NOT — lowercase r is the
            // re-arm replay and must stay that way on both screens.
            if upper == 'F' {
                assert_eq!(map_key(&app, press(lower)), action, "replay, '{lower}'");
            } else {
                assert_eq!(map_key(&app, press(lower)), Action::Replay, "'{lower}'");
            }
        }
    }

    /// `R` (raw) and `r` (re-arm) are different questions, so they must never
    /// collapse onto one action — on either screen.
    #[test]
    fn capital_r_is_not_the_same_action_as_lowercase_r() {
        let mut app = app_with_one_plan();
        for screen in [crate::screen::Screen::List, crate::screen::Screen::Replay] {
            app.screen = screen;
            assert_ne!(map_key(&app, press('R')), map_key(&app, press('r')));
        }
    }

    /// The Replay screen's scroll bindings still win over the new letters —
    /// `j`/`k`/`g`/`G` scroll there, and adding `f`/`R` must not have shadowed
    /// them.
    #[test]
    fn replay_scroll_keys_survive_the_new_bindings() {
        let mut app = app_with_one_plan();
        app.screen = crate::screen::Screen::Replay;
        assert_eq!(map_key(&app, press('j')), Action::ReplayScroll(1));
        assert_eq!(map_key(&app, press('k')), Action::ReplayScroll(-1));
        assert_eq!(map_key(&app, press('g')), Action::ReplayHome);
        assert_eq!(map_key(&app, press('G')), Action::ReplayEnd);
    }

    /// Off the list, `/` is not a search key (no prompt on deeper screens), and
    /// the existing bindings are untouched.
    #[test]
    fn list_bindings_are_otherwise_unchanged() {
        let app = app_with_one_plan();
        assert_eq!(map_key(&app, press('q')), Action::Quit);
        assert_eq!(map_key(&app, press('j')), Action::SelectNext);
        assert_eq!(map_key(&app, press('r')), Action::Replay);
        assert_eq!(map_key(&app, press('d')), Action::RequestDelete);
    }
}
