//! Rendering: one pass over the frame, driven by a pure function of app state.

pub mod approval;
pub mod chat;
pub mod input;
pub mod status;
pub mod theme;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::ui::theme::Theme;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let theme = Theme::default();
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas::<4>(frame.area());

    status::render(frame, areas[0], app, &theme);
    chat::render(frame, areas[1], app, &theme);
    input::render(frame, areas[2], app, &theme, app.approval.is_none());
    hints(frame, areas[3], app, &theme);

    // Drawn last so it sits above everything, including the prompt.
    if let Some(pending) = &app.approval {
        approval::render(frame, frame.area(), pending, &theme);
    }
}

fn hints(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let text = if app.approval.is_some() {
        "y run   ·   n skip"
    } else if app.busy {
        "working…   ·   PgUp/PgDn scroll"
    } else {
        "Enter send   ·   PgUp/PgDn scroll   ·   Esc quit"
    };

    frame.render_widget(Paragraph::new(Line::styled(text, theme.hint)), area);
}
