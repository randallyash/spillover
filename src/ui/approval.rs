//! The approval modal: the one thing that can interrupt the user mid-turn.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::PendingApproval;
use crate::ui::theme::Theme;

/// Lines of the preview shown before it is cut off.
const MAX_PREVIEW_LINES: usize = 12;

pub fn render(frame: &mut Frame, area: Rect, approval: &PendingApproval, theme: &Theme) {
    let width = area.width.saturating_sub(8).clamp(24, 88);

    let preview_lines = approval.preview.lines().count().min(MAX_PREVIEW_LINES);
    let height = (preview_lines as u16 + 6)
        .min(area.height.saturating_sub(2))
        .max(6);

    let popup = centered(area, width, height);
    // Wipe whatever the transcript drew underneath, or the modal looks broken.
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.assistant)
        .title(format!(" run {}? ", approval.tool));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line> = Vec::new();
    for (index, line) in approval.preview.lines().enumerate() {
        if index == MAX_PREVIEW_LINES {
            lines.push(Line::styled("…", theme.hint));
            break;
        }
        lines.push(Line::raw(line.to_string()));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("[y] run        [n] skip", theme.user));

    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_popup_sits_inside_its_parent() {
        let area = Rect::new(0, 0, 100, 40);
        let popup = centered(area, 60, 10);
        assert_eq!(popup.width, 60);
        assert_eq!(popup.height, 10);
        assert_eq!(popup.x, 20);
        assert_eq!(popup.y, 15);
    }

    #[test]
    fn a_popup_never_exceeds_a_small_terminal() {
        let area = Rect::new(0, 0, 20, 6);
        let popup = centered(area, 80, 20);
        assert!(popup.width <= area.width, "{popup:?}");
        assert!(popup.height <= area.height, "{popup:?}");
    }

    #[test]
    fn a_wide_preview_is_bounded() {
        let area = Rect::new(0, 0, 200, 50);
        let popup = centered(area, 200u16.saturating_sub(8).clamp(24, 88), 20);
        assert!(popup.width <= 88, "{popup:?}");
    }
}
