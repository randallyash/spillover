//! The prompt editor.
//!
//! The box grows with what is typed, up to a point, so a long prompt stays
//! visible instead of scrolling off the only line. It is the one bordered thing
//! on screen, which is how the eye finds where typing goes without being told.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::app::App;
use crate::text;
use crate::ui::theme::Theme;

/// Space between the border and the text.
const PAD: u16 = 1;
/// The box stops growing here; past it the text scrolls within the box.
const MAX_LINES: usize = 5;
/// The mark that says this is where you type, and the columns it costs.
const PROMPT: &str = "❯";
const PROMPT_WIDTH: usize = 2;

/// How tall the prompt box wants to be for this app and this width.
pub fn height(app: &App, width: u16) -> u16 {
    let body = body_width(width);
    let lines = wrapped(app, body).len().clamp(1, MAX_LINES);
    lines as u16 + 2
}

pub fn render(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    // While the model is waiting on an approval, the keys belong to the modal,
    // so the prompt is drawn as inactive rather than pretending to accept text.
    let focused = app.approval.is_none();
    let planning = app.mode.is_read_only();

    let mut title = vec![Span::styled(" prompt ", theme.title)];
    // The mode goes here rather than in the rail because it is what pressing
    // enter will mean, and this is the box enter is pressed in. In plan mode the
    // whole box is drawn in the warning colour too: a read-only session that
    // looked like any other would be the one way this could surprise someone.
    if planning {
        title.push(Span::styled(format!("· {} ", app.mode.label()), theme.warn));
    }
    if app.busy {
        title.push(Span::styled("· working ", theme.hint));
    }

    let border = if planning {
        theme.warn
    } else if focused {
        theme.border_active
    } else {
        theme.border
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Line::from(title));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let area = Rect {
        x: inner.x.saturating_add(PAD),
        y: inner.y,
        width: inner.width.saturating_sub(PAD * 2),
        height: inner.height,
    };
    let width = area.width as usize;
    let body = width.saturating_sub(PROMPT_WIDTH).max(1);

    let parts = wrapped(app, body);
    let shown = visible(&parts, area.height as usize);
    let offset = parts.len().saturating_sub(shown.len());
    let empty = app.input.is_empty();

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, part) in shown.iter().enumerate() {
        // The mark sits on the first line only; the rest hang under the text, so
        // a long prompt still reads as one block.
        let lead = if index == 0 {
            Span::styled(format!("{PROMPT} "), theme.accent)
        } else {
            Span::raw(" ".repeat(PROMPT_WIDTH))
        };
        let text = if empty && index == 0 {
            Span::styled(placeholder(app), theme.faint)
        } else {
            Span::styled(part.clone(), theme.assistant)
        };
        lines.push(Line::from(vec![lead, text]));
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), area);

    if !focused {
        return;
    }

    // The caret sits after the last character of the last visible line, and
    // never outside the box, so it stays put as the text grows.
    let last = shown.last().map(String::as_str).unwrap_or("");
    let typed = if app.input.is_empty() {
        0
    } else {
        text::display_width(last)
    };
    let column = (PROMPT_WIDTH + typed).min(width.saturating_sub(1));
    let x = area.x + column as u16;
    let y = area.y + offset as u16 + shown.len().saturating_sub(1) as u16;
    frame.set_cursor_position((x, y));
}

fn placeholder(app: &App) -> String {
    if app.busy {
        "the model is working".to_string()
    } else if app.mode.is_read_only() {
        // Says what this box will do, which in plan mode is not "change things".
        "ask what you would like planned".to_string()
    } else {
        "ask something".to_string()
    }
}

/// The columns available for text, once the mark has taken its share.
fn body_width(width: u16) -> usize {
    inner_width(width).saturating_sub(PROMPT_WIDTH).max(1)
}

/// The input split into lines of at most `width` cells. Never empty, so there is
/// always a line to put a caret on.
fn wrapped(app: &App, width: usize) -> Vec<String> {
    if app.input.is_empty() {
        return vec![String::new()];
    }
    text::wrap(&app.input, width)
}

/// The tail of the lines, which is where the caret is.
fn visible(parts: &[String], height: usize) -> Vec<String> {
    let start = parts.len().saturating_sub(height.max(1));
    parts[start..].to_vec()
}

fn inner_width(width: u16) -> usize {
    width.saturating_sub(2 + PAD * 2).max(1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn app_with_input(input: &str) -> App {
        let mut app = App::new(Config::default());
        app.input = input.to_string();
        app
    }

    #[test]
    fn an_empty_prompt_is_one_line_tall() {
        // Two borders plus one line of text.
        assert_eq!(height(&app_with_input(""), 80), 3);
    }

    #[test]
    fn the_box_grows_with_a_long_prompt() {
        let app = app_with_input(&"word ".repeat(40));
        assert!(
            height(&app, 60) > 3,
            "a long prompt needs more than one line"
        );
    }

    #[test]
    fn the_box_stops_growing_at_its_limit() {
        let app = app_with_input(&"word ".repeat(400));
        assert_eq!(height(&app, 40), MAX_LINES as u16 + 2);
    }

    #[test]
    fn a_narrower_box_makes_the_prompt_taller() {
        let app = app_with_input(&"word ".repeat(20));
        assert!(
            height(&app, 40) > height(&app, 200),
            "the same text needs more lines when there are fewer columns"
        );
    }

    #[test]
    fn the_tail_is_what_stays_visible() {
        let parts: Vec<String> = (1..=9).map(|n| n.to_string()).collect();
        assert_eq!(visible(&parts, 3), vec!["7", "8", "9"]);
        // Fewer lines than the box can hold shows all of them.
        assert_eq!(visible(&parts, 20), parts);
    }

    #[test]
    fn a_prompt_that_is_empty_always_has_a_line_for_the_caret() {
        let app = app_with_input("");
        assert_eq!(wrapped(&app, 10), vec![""]);
    }

    #[test]
    fn the_inner_width_accounts_for_the_border_and_the_padding() {
        // Two border columns, and a pad on each side.
        assert_eq!(inner_width(40), 36);
        assert_eq!(inner_width(3), 1, "it never reaches zero");
    }

    #[test]
    fn the_placeholder_changes_while_the_model_is_working() {
        let mut app = app_with_input("");
        assert_eq!(placeholder(&app), "ask something");

        app.busy = true;
        assert!(
            placeholder(&app).contains("working"),
            "{}",
            placeholder(&app)
        );
    }

    #[test]
    fn plan_mode_says_so_on_the_box_and_in_the_placeholder() {
        let mut app = app_with_input("");
        app.mode = crate::agent::Mode::Plan;

        // The placeholder: this box will not change anything.
        assert!(
            placeholder(&app).contains("planned"),
            "{}",
            placeholder(&app)
        );
    }

    /// Render the prompt box to text, to check what it says.
    fn rendered(app: &App) -> String {
        let backend = ratatui::backend::TestBackend::new(80, 3);
        let mut terminal = ratatui::Terminal::new(backend).expect("a terminal");
        terminal
            .draw(|frame| render(frame, frame.area(), app, &Theme::default()))
            .expect("the box should draw");

        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, 0)].symbol());
        }
        out
    }

    #[test]
    fn the_mode_is_written_on_the_prompt_box_in_plan_mode_only() {
        let mut app = app_with_input("");
        // Build is the default, so it does not need to be announced; plan mode
        // is the state that changes what pressing enter will do, and is the one
        // worth saying out loud.
        let build = rendered(&app);
        assert!(
            !build.contains("plan"),
            "build mode should not be wearing the plan badge: {build}"
        );

        app.mode = crate::agent::Mode::Plan;
        let plan = rendered(&app);
        assert!(plan.contains("plan"), "plan mode must be visible: {plan}");
    }

    #[test]
    fn the_prompt_still_renders_when_the_mode_and_busy_are_both_set() {
        // The title carries both, so they must not collide.
        let mut app = app_with_input("hello");
        app.mode = crate::agent::Mode::Plan;
        app.busy = true;
        let out = rendered(&app);

        assert!(out.contains("plan"), "{out}");
        assert!(out.contains("working"), "{out}");
    }
}
