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
    TogglePopup,
    RequestDelete,
    ConfirmYes,
    ConfirmNo,
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

    // The detail popup: any of i/esc/q closes it; other keys pass through so
    // navigation still works with it open would be surprising — keep it modal.
    if app.show_popup {
        return match key.code {
            KeyCode::Char('i') | KeyCode::Char('I') | KeyCode::Esc | KeyCode::Char('q') => {
                Action::TogglePopup
            }
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
        Action::TogglePopup => app.toggle_popup(),
        Action::RequestDelete => app.request_delete(),
        Action::ConfirmYes => app.resolve_confirm(true),
        Action::ConfirmNo => app.resolve_confirm(false),
        Action::None => {}
    }
}
