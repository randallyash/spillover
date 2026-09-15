//! The two popups: the command menu that opens as you type a slash, and the
//! help overlay.
//!
//! Both are drawn from the same catalogue the parser reads, so what the menu
//! offers and what actually runs cannot drift apart. The menu is anchored to the
//! prompt box rather than centered, because it belongs to the line being typed
//! and would lose that connection floating in the middle of the screen.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::app::App;
use crate::commands;
use crate::text;
use crate::ui::centered;
use crate::ui::theme::Theme;

/// How wide the menu will grow.
const MENU_MAX_WIDTH: u16 = 96;
const MENU_MIN_WIDTH: u16 = 40;
/// Rows of menu before it starts scrolling: one more than the catalogue holds.
fn menu_max_rows() -> usize {
    commands::CATALOGUE.len() + 1
}

/// The command menu, sitting on top of the prompt.
pub fn render_menu(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, prompt: Rect) {
    let matches = app.menu_matches();
    if matches.is_empty() {
        return;
    }

    let width = area.width.saturating_sub(8).clamp(
        MENU_MIN_WIDTH.min(area.width),
        MENU_MAX_WIDTH.min(area.width),
    );
    // Room for the rows, the hint line, and the border — and no more than the
    // space between the top of the frame and the prompt, so the menu cannot
    // cover the prompt it belongs to.
    let above = prompt.y.saturating_sub(area.y).saturating_sub(3).max(1) as usize;
    let rows = matches.len().min(menu_max_rows()).min(above).max(1);
    let height = rows as u16 + 3;

    // Sits directly above the prompt when there is room, and drops to the
    // bottom of the frame when there is not.
    let y = prompt.y.saturating_sub(height);
    let popup = Rect {
        x: area.x + 1,
        y,
        width: width.min(area.width.saturating_sub(2)),
        height: height.min(area.height),
    };

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border)
        .title(Line::styled(" commands ", theme.section));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Keep the highlighted row on screen when there are more matches than rows.
    let selected = app.menu_index.min(matches.len().saturating_sub(1));
    let first = selected.saturating_sub(rows.saturating_sub(1));
    let visible = matches.len().min(first + rows);
    let shown = &matches[first..visible];

    // A column wide enough for the longest command on screen, so the summaries
    // line up instead of starting wherever each command happens to end.
    let column = shown
        .iter()
        .map(|spec| text::display_width(&commands::usage(spec)))
        .max()
        .unwrap_or(0);

    let mut lines: Vec<Line> = Vec::new();
    for (offset, spec) in shown.iter().enumerate() {
        let index = first + offset;
        let chosen = index == selected;

        let marker = if chosen { "▸ " } else { "  " };
        let name_style = if chosen {
            theme.accent
        } else {
            theme.assistant
        };

        let usage = commands::usage(spec);
        let room = (inner.width as usize).saturating_sub(2 + column + 2);

        let mut spans = vec![
            Span::styled(marker, if chosen { theme.accent } else { theme.faint }),
            Span::styled(format!("{usage:<column$}  "), name_style),
        ];
        // The summary is the first thing to go when the row runs out of room:
        // the command's own name is what is being chosen.
        if room > 8 {
            spans.push(Span::styled(clip(spec.summary, room), theme.faint));
        }
        lines.push(Line::from(spans));
    }

    // The keys, spelled out rather than implied.
    lines.push(Line::from(vec![
        Span::styled("↑↓", theme.accent),
        Span::styled(" choose   ", theme.faint),
        Span::styled("tab", theme.accent),
        Span::styled(" complete   ", theme.faint),
        Span::styled("enter", theme.accent),
        Span::styled(" run", theme.faint),
    ]));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// The help overlay: every command, and every key that is not a character.
pub fn render_help(frame: &mut Frame, area: Rect, theme: &Theme) {
    let width = area.width.saturating_sub(8).clamp(
        MENU_MIN_WIDTH.min(area.width),
        MENU_MAX_WIDTH.min(area.width),
    );
    let content_width = width.saturating_sub(4) as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::styled("keys", theme.section));
    for (keys, what) in [
        ("enter", "send the prompt"),
        ("shift+tab", "switch between build and plan mode"),
        ("tab", "the same, or complete a command being typed"),
        ("esc", "quit, or cancel a half-typed command"),
        ("ctrl-c", "quit from anywhere"),
        ("ctrl-n", "start a new session"),
        ("ctrl-p", "session picker"),
        ("pgup / pgdn", "scroll the conversation"),
        ("↑ / ↓", "choose in the command menu"),
        ("?", "this"),
        ("y / n", "approve or skip a file write or command"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<14}", keys), theme.accent),
            Span::styled(what.to_string(), theme.assistant),
        ]));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled("modes", theme.section));
    for (name, what) in [
        ("build", "reads, writes and runs commands, asking first"),
        (
            "plan",
            "reads and searches only — you get a plan, not a change",
        ),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!("  {name:<14}"), theme.accent),
            Span::styled(what.to_string(), theme.assistant),
        ]));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled("commands", theme.section));
    for spec in commands::CATALOGUE {
        let usage = commands::usage(spec);
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<26}", usage), theme.accent),
            Span::styled(
                clip(spec.summary, content_width.saturating_sub(28)),
                theme.assistant,
            ),
        ]));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled("  esc or ? closes this", theme.faint));

    let height = (lines.len() as u16 + 2).min(area.height);
    let popup = centered(area, width, height);

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border_active)
        .title(Line::styled(" spill ", theme.brand));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.width == 0 || inner.height == 0 {
        return;
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Cut to a display width, keeping whole characters.
fn clip(source: &str, width: usize) -> String {
    if text::display_width(source) <= width {
        return source.to_string();
    }

    let mut out = String::new();
    let mut used = 0usize;
    for ch in source.chars() {
        let ch_width = text::display_width(&ch.to_string());
        // Leave a cell for the ellipsis, so the clip is visible as one.
        if used + ch_width > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn rendered(app: &App, width: u16, height: u16, help: bool) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("a terminal");
        let theme = Theme::default();
        terminal
            .draw(|frame| {
                let prompt = Rect::new(0, height - 4, width, 3);
                if help {
                    render_help(frame, frame.area(), &theme);
                } else {
                    render_menu(frame, frame.area(), app, &theme, prompt);
                }
            })
            .expect("the frame should draw");

        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn typing(input: &str) -> App {
        let mut app = App::new(Config::default());
        app.input = input.to_string();
        app
    }

    #[test]
    fn a_lone_slash_opens_the_menu_with_every_command() {
        let out = rendered(&typing("/"), 110, 30, false);
        for spec in commands::CATALOGUE {
            assert!(out.contains(spec.name), "{} is missing:\n{out}", spec.name);
        }
    }

    #[test]
    fn the_menu_narrows_to_what_has_been_typed() {
        let out = rendered(&typing("/comp"), 110, 30, false);
        assert!(out.contains("compact"), "{out}");
        assert!(!out.contains("sticky"), "it should have filtered:\n{out}");
    }

    #[test]
    fn the_highlighted_row_is_marked_by_a_glyph_rather_than_only_by_colour() {
        let mut app = typing("/");
        app.menu_index = 1;
        let out = rendered(&app, 110, 30, false);

        assert!(out.contains('▸'), "the selection needs a mark:\n{out}");
        // The mark sits on the second row of the list, not the first.
        let marker_row = out
            .lines()
            .find(|line| line.contains('▸'))
            .expect("a marked row");
        assert!(
            marker_row.contains("escalate"),
            "wrong row marked: {marker_row}"
        );
    }

    #[test]
    fn the_menu_shows_each_command_with_its_argument() {
        let out = rendered(&typing("/tier"), 110, 30, false);
        assert!(out.contains("/tier <name|number|auto>"), "{out}");
    }

    #[test]
    fn the_menu_lists_its_own_keys() {
        let out = rendered(&typing("/"), 110, 30, false);
        assert!(out.contains("complete"), "{out}");
        assert!(out.contains("enter"), "{out}");
    }

    #[test]
    fn the_menu_stays_inside_the_frame_at_every_size() {
        for (width, height) in [(60u16, 16u16), (80, 24), (110, 30), (200, 50)] {
            let out = rendered(&typing("/"), width, height, false);
            assert_eq!(
                out.lines().count(),
                height as usize,
                "the frame should be exactly {height} rows"
            );
        }
    }

    #[test]
    fn the_help_overlay_lists_every_command_and_the_keys() {
        let out = rendered(&typing(""), 120, 44, true);
        for spec in commands::CATALOGUE {
            assert!(out.contains(spec.name), "{} is missing:\n{out}", spec.name);
        }
        for key in [
            "enter",
            "esc",
            "ctrl-c",
            "ctrl-n",
            "ctrl-p",
            "tab",
            "shift+tab",
        ] {
            assert!(out.contains(key), "{key} is missing:\n{out}");
        }
    }

    #[test]
    fn the_help_overlay_explains_what_the_two_modes_do() {
        // A mode whose difference is not written down is a key that appears to
        // do nothing.
        let out = rendered(&typing(""), 120, 44, true);
        assert!(out.contains("modes"), "{out}");
        assert!(out.contains("build"), "{out}");
        assert!(out.contains("plan"), "{out}");
        assert!(
            out.contains("reads and searches only"),
            "plan mode should say what it means: {out}"
        );
    }

    #[test]
    fn a_long_summary_is_clipped_rather_than_overflowing() {
        let clipped = clip("a summary that goes on and on and on and on", 10);
        assert!(text::display_width(&clipped) <= 10, "{clipped:?}");
        assert!(clipped.ends_with('…'), "{clipped:?}");

        // Something that already fits is untouched.
        assert_eq!(clip("short", 20), "short");
    }

    /// Print the popups, to look at them without running spill.
    #[test]
    #[ignore = "prints the popups to look at; asserts nothing"]
    fn print_the_popups() {
        for (label, input, help) in [
            ("a lone slash", "/", false),
            ("filtered to ti", "/ti", false),
            ("the help overlay", "", true),
        ] {
            let mut app = typing(input);
            app.menu_index = 1;
            println!("\n===== {label} =====\n{}", rendered(&app, 110, 32, help));
        }
    }
}
