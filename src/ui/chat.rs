//! The transcript pane: messages wrapped to the available width, scrolled to the bottom.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, Role};
use crate::text;
use crate::ui::theme::Theme;

/// Columns reserved for the role label, so wrapped text lines up under itself.
const LABEL_WIDTH: usize = 8;

pub fn render(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border)
        .title(" transcript ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let text_width = (inner.width as usize).saturating_sub(LABEL_WIDTH).max(1);
    let mut lines: Vec<Line> = Vec::new();

    for message in &app.messages {
        let style = style_for(message.role, theme);
        let label = label_for(message.role);

        for (index, part) in text::wrap(&message.text, text_width).iter().enumerate() {
            let prefix = if index == 0 {
                format!("{label:<LABEL_WIDTH$}")
            } else {
                " ".repeat(LABEL_WIDTH)
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::raw(part.clone()),
            ]));
        }
    }

    let visible = inner.height as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    let scroll_back = (app.scroll_back as usize).min(max_scroll);
    app.scroll_back = scroll_back as u16;

    let end = lines.len().saturating_sub(scroll_back);
    let start = end.saturating_sub(visible);
    let window: Vec<Line> = lines[start..end].to_vec();

    frame.render_widget(Paragraph::new(Text::from(window)), inner);
}

fn label_for(role: Role) -> &'static str {
    match role {
        Role::User => "you",
        Role::Assistant => "spill",
        Role::System => "·",
    }
}

fn style_for(role: Role, theme: &Theme) -> Style {
    match role {
        Role::User => theme.user,
        Role::Assistant => theme.assistant,
        Role::System => theme.system,
    }
}
