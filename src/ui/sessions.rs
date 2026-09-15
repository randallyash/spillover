//! The session picker: named history for this workspace.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::app::App;
use crate::session_store::SessionEntry;
use crate::text;
use crate::ui::centered;
use crate::ui::theme::Theme;

const MIN_WIDTH: u16 = 40;
const MAX_WIDTH: u16 = 72;

pub fn render(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let width = area
        .width
        .saturating_sub(8)
        .clamp(MIN_WIDTH.min(area.width), MAX_WIDTH.min(area.width));
    let rows = app.session_entries.len().clamp(1, 14);
    let height = rows as u16 + 5;
    let popup = centered(area, width, height);

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" sessions ")
        .title_style(theme.title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border_active);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line> = Vec::new();
    if app.session_entries.is_empty() {
        lines.push(Line::styled("no saved sessions yet", theme.faint));
    } else {
        for (index, entry) in app.session_entries.iter().enumerate() {
            lines.push(row(app, entry, index, inner.width as usize, theme));
        }
    }
    lines.push(Line::raw(""));
    if let Some(edit) = &app.session_rename {
        lines.push(Line::from(vec![
            Span::styled("rename: ", theme.hint),
            Span::styled(edit.clone(), theme.assistant),
            Span::styled("▌", theme.accent),
        ]));
    } else {
        lines.push(Line::styled(
            "enter open  n new  d delete  r rename  esc close",
            theme.hint,
        ));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn row(
    app: &App,
    entry: &SessionEntry,
    index: usize,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let selected = index == app.session_cursor;
    let current = entry.id == app.session_current;
    let mark = if selected { "▸ " } else { "  " };
    let dot = if current { "● " } else { "  " };
    let when = ago(entry.saved_at);
    let title = if entry.title.is_empty() {
        "untitled"
    } else {
        &entry.title
    };
    let label = format!("{mark}{dot}{title}");
    let value = format!(" {when}");
    let room = width.saturating_sub(text::display_width(&value)).max(1);
    let mut head = label;
    while text::display_width(&head) > room {
        head.pop();
    }
    let pad = " ".repeat(room.saturating_sub(text::display_width(&head)));
    let style = if selected {
        theme.accent
    } else if current {
        theme.assistant
    } else {
        theme.hint
    };
    Line::from(vec![
        Span::styled(format!("{head}{pad}"), style),
        Span::styled(value, theme.hint),
    ])
}

fn ago(saved_at: u64) -> String {
    crate::app::ago(saved_at)
}
