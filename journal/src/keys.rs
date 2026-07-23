//! Keyboard → [`Action`] mapping. Modal/popup states swallow most keys, so the
//! mapping is context-sensitive on the app state.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;

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
    /// Record the current trade's outcome to the journal DB (the `s` key on
    /// Compare).
    Record,
    TogglePopup,
    RequestDelete,
    ConfirmYes,
    ConfirmNo,
    /// Scroll the detail popup by N lines (negative = up).
    PopupScroll(i32),
    PopupHome,
    PopupEnd,
    None,
}

/// Map a key event to an action, given the current app state.
pub fn map_key(app: &App, key: KeyEvent) -> Action {
    // Ctrl-C always quits.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Quit;
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
            _ => Action::None,
        };
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Up | KeyCode::Char('k') => Action::SelectPrev,
        KeyCode::Down | KeyCode::Char('j') => Action::SelectNext,
        KeyCode::Right | KeyCode::Enter | KeyCode::Char('n') => Action::Deeper,
        KeyCode::Left => Action::Shallower,
        KeyCode::Char('l') => Action::LoadTv,
        KeyCode::Char('r') => Action::Replay,
        KeyCode::Char('s') | KeyCode::Char('S') => Action::Record,
        KeyCode::Char('i') => Action::TogglePopup,
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
        Action::Record => app.record_current(),
        Action::TogglePopup => app.toggle_popup(),
        Action::RequestDelete => app.request_delete(),
        Action::ConfirmYes => app.resolve_confirm(true),
        Action::ConfirmNo => app.resolve_confirm(false),
        Action::PopupScroll(delta) => app.scroll_popup(delta),
        Action::PopupHome => app.scroll_popup_home(),
        Action::PopupEnd => app.scroll_popup_end(),
        Action::None => {}
    }
}
