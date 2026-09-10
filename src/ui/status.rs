//! The header line: the configured tier chain, in order.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::ui::theme::Theme;

pub fn render(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let line = if app.tiers().is_empty() {
        Line::from(vec![
            Span::styled(" spill ", theme.title),
            Span::styled(" · ", theme.hint),
            Span::styled("no tiers configured", theme.hint),
        ])
    } else {
        let mut spans = vec![
            Span::styled(" spill ", theme.title),
            Span::styled(" · ", theme.hint),
        ];
        for (index, tier) in app.tiers().iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled("  →  ", theme.hint));
            }
            spans.push(Span::styled(
                format!("{}. {}", index + 1, tier.display_name()),
                theme.assistant,
            ));
        }
        Line::from(spans)
    };

    frame.render_widget(Paragraph::new(line), area);
}
