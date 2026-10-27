//! Visible plan controls and their mouse targets share the same rectangles.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::App;
use crate::keys::Action;
use crate::plan_admin::{AccountPicker, ActionConfirmation};
use crate::screen::Screen;

const BUTTONS: [(&str, Action); 3] = [
    ("[p Paper mode]", Action::PaperMode),
    ("[b Swap account]", Action::SwapAccount),
    ("[m Closed manually]", Action::ClosedManually),
];

pub fn render_controls(f: &mut Frame, app: &App, area: Rect) {
    let targets = control_targets(area);
    BUTTONS
        .iter()
        .zip(targets)
        .for_each(|((label, _), target)| {
            f.render_widget(Paragraph::new(*label).style(button_style()), target);
        });
    if let Some(plan) = app.current_plan() {
        let mode = if plan.shadow {
            "paper · entries logged"
        } else {
            "broker"
        };
        let status = format!("{} · {mode}", plan.account);
        let used = targets.iter().map(|r| r.width).sum::<u16>();
        f.render_widget(
            Paragraph::new(status).style(Style::default().fg(Color::Gray)),
            Rect::new(
                area.x + used,
                area.y,
                area.width.saturating_sub(used),
                area.height,
            ),
        );
    }
}

pub fn render_modal(f: &mut Frame, app: &App) {
    if let Some(confirm) = &app.action_confirm {
        render_confirmation(f, confirm);
    } else if let Some(picker) = &app.account_picker {
        render_picker(f, app, picker);
    }
}

fn render_confirmation(f: &mut Frame, confirm: &ActionConfirmation) {
    let area = modal_area(f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", confirm.action.title()))
        .border_style(Style::default().fg(Color::Yellow));
    f.render_widget(block, area);
    let body = Rect::new(
        area.x + 2,
        area.y + 2,
        area.width.saturating_sub(4),
        area.height.saturating_sub(5),
    );
    let text = confirm.action.explanation(&confirm.trade_id);
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), body);
    let [yes, no] = confirm_targets(area);
    f.render_widget(Paragraph::new("[y Confirm]").style(button_style()), yes);
    f.render_widget(Paragraph::new("[n/Esc Cancel]").style(button_style()), no);
}

fn render_picker(f: &mut Frame, app: &App, picker: &AccountPicker) {
    let area = modal_area(f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", picker.purpose.title()))
        .title_bottom(" ↑↓ choose · Enter select · Esc cancel ")
        .border_style(Style::default().fg(Color::Cyan));
    f.render_widget(block, area);
    let body = picker_body(area);
    let lines = account_lines(picker, body.height);
    if lines.is_empty() {
        let message = if app.is_current_loading(crate::jobs::JobKind::Accounts) {
            "Loading accounts…"
        } else {
            "No alternative accounts loaded. See status below; Esc to close."
        };
        f.render_widget(Paragraph::new(message).wrap(Wrap { trim: true }), body);
    } else {
        f.render_widget(Paragraph::new(lines), body);
    }
}

fn account_lines(picker: &AccountPicker, height: u16) -> Vec<Line<'static>> {
    picker
        .accounts
        .iter()
        .enumerate()
        .skip(picker_start(picker.selected, height))
        .take(height as usize)
        .map(|(index, account)| {
            let selected = index == picker.selected;
            let marker = if selected { "▶" } else { " " };
            let unavailable = account.unavailable_reason(picker.purpose);
            let style = if unavailable.is_some() {
                Style::default().fg(Color::DarkGray)
            } else if selected {
                button_style()
            } else {
                Style::default().fg(Color::White)
            };
            Line::styled(
                format!(
                    "{marker} {}  {}  {}{}",
                    account.name,
                    account.broker,
                    account.kind,
                    unavailable
                        .map(|reason| format!("  [unavailable: {reason}]"))
                        .unwrap_or_default()
                ),
                style,
            )
        })
        .collect()
}

pub fn map_mouse(app: &App, event: MouseEvent, area: Rect) -> Action {
    if event.kind != MouseEventKind::Down(MouseButton::Left)
        || app.prompt.is_some()
        || app.confirm.is_some()
        || app.show_popup
    {
        return Action::None;
    }
    if app.action_confirm.is_some() {
        let [yes, no] = confirm_targets(modal_area(area));
        return if contains(yes, event) {
            Action::ConfirmManagement
        } else if contains(no, event) {
            Action::CancelManagement
        } else {
            Action::None
        };
    }
    if let Some(picker) = &app.account_picker {
        let body = picker_body(modal_area(area));
        if contains(body, event) {
            let index = picker_start(picker.selected, body.height) + (event.row - body.y) as usize;
            if index < picker.accounts.len() {
                return Action::AccountChoose(index);
            }
        }
        return Action::None;
    }
    if app.screen == Screen::List {
        return Action::None;
    }
    let row = Rect::new(area.x, area.y + super::infobar::height(app), area.width, 1);
    BUTTONS
        .iter()
        .zip(control_targets(row))
        .find_map(|((_, action), target)| contains(target, event).then_some(*action))
        .unwrap_or(Action::None)
}

fn button_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn control_targets(area: Rect) -> [Rect; 3] {
    let targets = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(18),
            Constraint::Length(18),
            Constraint::Length(22),
            Constraint::Min(0),
        ])
        .split(area);
    [targets[0], targets[1], targets[2]]
}

fn modal_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(92);
    let height = area.height.saturating_sub(2).min(14);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn confirm_targets(area: Rect) -> [Rect; 2] {
    let row = Rect::new(
        area.x + 2,
        area.y + area.height.saturating_sub(2),
        area.width.saturating_sub(4),
        1,
    );
    let targets = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(16), Constraint::Min(0)])
        .split(row);
    [targets[0], targets[1]]
}

fn picker_body(area: Rect) -> Rect {
    Rect::new(
        area.x + 2,
        area.y + 2,
        area.width.saturating_sub(4),
        area.height.saturating_sub(3),
    )
}

fn picker_start(selected: usize, height: u16) -> usize {
    selected.saturating_sub(height.saturating_sub(1) as usize)
}

fn contains(rect: Rect, event: MouseEvent) -> bool {
    event.column >= rect.x
        && event.column < rect.right()
        && event.row >= rect.y
        && event.row < rect.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::parse_plan_list;
    use crate::plan_admin::{
        AccountChoice, AccountPicker, AccountPurpose, ActionConfirmation, PlanAction,
    };
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        let mut app = App::from_rows(
            parse_plan_list("- trade_id: trade-1\n  account: expired-mt5\n  instrument: AUDUSD\n")
                .unwrap(),
        );
        app.screen = Screen::Timeline;
        app
    }

    fn click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn archived_plan_can_record_manual_closure_but_cannot_switch_accounts() {
        let mut app = app();
        app.plans[0].archived_at = Some("2026-10-20T01:00:00Z".into());
        app.request_plan_action(PlanAction::ClosedManually);
        assert_eq!(
            app.action_confirm.as_ref().unwrap().action,
            PlanAction::ClosedManually
        );
        app.action_confirm = None;
        app.request_swap_account();
        assert!(app.account_picker.is_none());
        app.request_paper_mode();
        assert!(app.account_picker.is_none());
    }

    #[test]
    fn open_plan_controls_are_visible_and_clickable() {
        let app = app();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| super::super::render(f, &app)).unwrap();
        let text = text(&terminal);
        assert!(text.contains("Paper mode"));
        assert!(text.contains("Swap account"));
        assert!(text.contains("Closed manually"));
        let y = super::super::infobar::height(&app);
        assert_eq!(
            map_mouse(&app, click(2, y), Rect::new(0, 0, 120, 30)),
            Action::PaperMode
        );
        assert_eq!(
            map_mouse(&app, click(20, y), Rect::new(0, 0, 120, 30)),
            Action::SwapAccount
        );
        assert_eq!(
            map_mouse(&app, click(38, y), Rect::new(0, 0, 120, 30)),
            Action::ClosedManually
        );
    }

    #[test]
    fn paper_confirmation_identifies_data_account_and_observation_limit() {
        let mut app = app();
        app.action_confirm = Some(ActionConfirmation {
            trade_id: "trade-1".into(),
            action: PlanAction::Paper {
                account: "reversals".into(),
            },
        });
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| super::super::render(f, &app)).unwrap();
        let text = text(&terminal);
        assert!(text.contains("market data from reversals"));
        assert!(text.contains("Entries are logged"));
        assert!(text.contains("broker orders"));
        assert!(text.contains("detached"));
        let modal = modal_area(Rect::new(0, 0, 120, 30));
        let [yes, no] = confirm_targets(modal);
        assert_eq!(
            map_mouse(&app, click(yes.x, yes.y), Rect::new(0, 0, 120, 30)),
            Action::ConfirmManagement
        );
        assert_eq!(
            map_mouse(&app, click(no.x, no.y), Rect::new(0, 0, 120, 30)),
            Action::CancelManagement
        );
    }

    #[test]
    fn paper_account_picker_click_opens_confirmation_for_selected_data_account() {
        let mut app = app();
        app.account_picker = Some(AccountPicker {
            trade_id: "original-trade".into(),
            purpose: AccountPurpose::Paper,
            accounts: vec![AccountChoice {
                name: "reversals".into(),
                broker: "tradenation".into(),
                kind: "demo".into(),
                capabilities: None,
            }],
            selected: 0,
        });
        let area = Rect::new(0, 0, 120, 30);
        let row = picker_body(modal_area(area));
        let action = map_mouse(&app, click(row.x, row.y), area);
        crate::keys::apply(&mut app, action);
        let confirm = app.action_confirm.as_ref().unwrap();
        assert_eq!(confirm.trade_id, "original-trade");
        assert_eq!(
            confirm.action,
            PlanAction::Paper {
                account: "reversals".into()
            }
        );
    }

    #[test]
    fn quote_only_account_click_cannot_confirm_a_plan_transition() {
        let mut app = app();
        for purpose in [AccountPurpose::Paper, AccountPurpose::SwapAccount] {
            let accounts = crate::plan_admin::parse_accounts(r#"[{"name":"ctrader-demo","broker":"ctrader","kind":"demo","capabilities":{"quotes":true,"execution":false,"history":false,"readiness":"quotes-only","reason":"FIX quotes only; Open API needed"}}]"#).unwrap();
            app.account_picker = Some(AccountPicker {
                trade_id: "original-trade".into(),
                purpose,
                accounts,
                selected: 0,
            });
            app.accept_account();
            assert!(app.action_confirm.is_none());
            assert!(app.account_picker.is_some());
            assert!(app.status.text.contains("Open API"));
        }
    }
}
