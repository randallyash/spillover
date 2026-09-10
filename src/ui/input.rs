//! The prompt editor.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::text;
use crate::ui::theme::Theme;

pub fn render(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, show_cursor: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border)
        .title(" prompt ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let body = if app.input.is_empty() {
        Line::styled("ask something", theme.hint)
    } else {
        Line::raw(app.input.clone())
    };
    frame.render_widget(Paragraph::new(Text::from(body)), inner);

    if !show_cursor {
        return;
    }

    // Keep the caret inside the box even when the prompt is wider than the pane.
    let typed = text::display_width(&app.input);
    let limit = inner.width.saturating_sub(1) as usize;
    let cursor_x = inner.x + typed.min(limit) as u16;
    frame.set_cursor_position((cursor_x, inner.y));
}
