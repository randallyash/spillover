//! The approval modal: the one thing that can interrupt a turn.
//!
//! What it shows is shaped by what the tools actually produce, which is three
//! different things. `edit_file` hands over a real before/after with `-` and `+`
//! lines; `run_shell` hands over a context line and the command indented under
//! it; `write_file` hands over a single sentence. So the styling is decided per
//! line rather than assumed to be a diff — a command dressed up as a diff would
//! be a lie about what is about to happen.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::app::PendingApproval;
use crate::text;
use crate::ui::centered;
use crate::ui::theme::Theme;

/// Space between the border and the content.
const PAD: u16 = 1;
/// The widest the modal grows, however wide the terminal is.
const MAX_WIDTH: u16 = 88;
/// Below this it is not worth drawing a box at all.
const MIN_WIDTH: u16 = 24;

pub fn render(frame: &mut Frame, area: Rect, approval: &PendingApproval, theme: &Theme) {
    let width = modal_width(area.width);
    let content_width = content_width(width);
    let mut lines = body(approval, theme, content_width);

    // Trim to what the terminal can actually show, and say so rather than
    // letting the rest fall off the bottom.
    let room = area.height.saturating_sub(4) as usize;
    if lines.len() > room && room > 1 {
        lines.truncate(room - 1);
        lines.push(Line::styled("…", theme.hint));
    }

    let height = lines.len() as u16 + 4;
    let popup = centered(area, width, height);

    // Wipe whatever the transcript drew underneath, or the modal looks broken.
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border_active)
        .title(Line::styled(
            format!(" {} ", action(&approval.tool)),
            theme.brand,
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let content = Rect {
        x: inner.x.saturating_add(PAD),
        y: inner.y.saturating_add(1),
        width: inner.width.saturating_sub(PAD * 2),
        height: inner.height.saturating_sub(2),
    };
    if content.width == 0 || content.height == 0 {
        return;
    }

    frame.render_widget(Paragraph::new(lines), content);
}

/// What is about to happen, in the user's terms rather than the tool's.
pub fn action(tool: &str) -> String {
    match tool {
        "run_shell" => "run a command?".to_string(),
        "write_file" => "write a file?".to_string(),
        "edit_file" => "edit a file?".to_string(),
        other => format!("run {other}?"),
    }
}

/// The modal's content: the preview, then the keys.
pub fn body(approval: &PendingApproval, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    for (index, raw) in approval.preview.lines().enumerate() {
        let style = line_style(index, raw, theme);
        // A preview line can be a long path or a long command, so it wraps
        // rather than being cut off at the box edge. The wrapped parts are used
        // as they come: adding an indent here would push a full line over the
        // edge it was just measured against.
        for part in text::wrap(raw, width) {
            lines.push(Line::from(Span::styled(part, style)));
        }
    }

    lines.push(Line::raw(""));
    lines.push(keys(theme));
    lines
}

/// How one line of a preview is styled.
///
/// The classification is by shape, because that is what the tools emit: `+` and
/// `-` from an edit, an indented line from a shell command, and a leading
/// sentence that describes the whole thing.
fn line_style(index: usize, line: &str, theme: &Theme) -> ratatui::style::Style {
    if line.starts_with("+ ") {
        theme.diff_add
    } else if line.starts_with("- ") {
        theme.diff_del
    } else if line.starts_with("  ") {
        // The command itself, indented under the directory it will run in.
        theme.command
    } else if index == 0 {
        theme.title
    } else {
        theme.assistant
    }
}

fn keys(theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("y", theme.accent),
        Span::styled(" run", theme.hint),
        Span::styled("      ", theme.hint),
        Span::styled("n", theme.accent),
        Span::styled(" skip", theme.hint),
    ])
}

/// The modal never takes the whole screen, so it reads as an interruption
/// rather than as another pane.
pub fn modal_width(area_width: u16) -> u16 {
    area_width
        .saturating_sub(8)
        .clamp(MIN_WIDTH, MAX_WIDTH)
        .min(area_width)
}

fn content_width(modal_width: u16) -> usize {
    modal_width.saturating_sub(2 + PAD * 2).max(1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    fn approval(tool: &str, preview: &str) -> PendingApproval {
        let (reply, _answer) = oneshot::channel();
        PendingApproval {
            tool: tool.to_string(),
            preview: preview.to_string(),
            reply,
        }
    }

    fn flat(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn the_modal_names_the_action_in_the_users_terms() {
        assert_eq!(action("run_shell"), "run a command?");
        assert_eq!(action("write_file"), "write a file?");
        assert_eq!(action("edit_file"), "edit a file?");
        // An unknown tool still gets a sentence rather than nothing.
        assert_eq!(action("mystery"), "run mystery?");
    }

    #[test]
    fn an_edit_is_shown_as_a_before_and_after() {
        let theme = Theme::default();
        let preview = "edit /tmp/a.txt — 1 occurrence\n- old line\n+ new line";
        let lines = body(&approval("edit_file", preview), &theme, 60);

        let removals = lines
            .iter()
            .find(|line| flat(std::slice::from_ref(line))[0].starts_with("- old"))
            .expect("the removed line");
        let additions = lines
            .iter()
            .find(|line| flat(std::slice::from_ref(line))[0].starts_with("+ new"))
            .expect("the added line");

        assert_eq!(removals.spans[0].style, theme.diff_del);
        assert_eq!(additions.spans[0].style, theme.diff_add);
    }

    #[test]
    fn a_shell_command_is_styled_as_a_command_not_as_a_diff() {
        let theme = Theme::default();
        let preview = "run in /tmp:\n  cargo test --all";
        let lines = body(
            &approval("run_shell", &preview.replace("\\n", "\n")),
            &theme,
            60,
        );
        let text = flat(&lines);

        assert!(
            text.iter().any(|line| line.contains("run in /tmp:")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("cargo test --all")),
            "{text:?}"
        );

        let command = lines
            .iter()
            .find(|line| flat(std::slice::from_ref(line))[0].contains("cargo test"))
            .expect("the command line");
        assert_eq!(command.spans[0].style, theme.command);
    }

    #[test]
    fn a_write_is_shown_as_its_sentence() {
        let theme = Theme::default();
        let lines = body(
            &approval("write_file", "create /tmp/new.txt (42 bytes)"),
            &theme,
            60,
        );
        let text = flat(&lines);
        assert!(text[0].contains("create /tmp/new.txt"), "{text:?}");
        assert_eq!(lines[0].spans[0].style, theme.title);
    }

    #[test]
    fn a_long_path_wraps_instead_of_being_cut_off() {
        let theme = Theme::default();
        let preview = format!("create {}/a/very/long/path (10 bytes)", "/x".repeat(30));
        let lines = body(&approval("write_file", &preview), &theme, 40);

        for line in &lines {
            assert!(
                line.width() <= 40,
                "the modal overflowed: {:?}",
                flat(std::slice::from_ref(line))
            );
        }
        let joined = flat(&lines).join(" ");
        assert!(
            joined.contains("(10 bytes)"),
            "the end should survive: {joined}"
        );
    }

    #[test]
    fn the_keys_are_always_the_last_line() {
        let theme = Theme::default();
        let lines = body(&approval("write_file", "create x"), &theme, 60);
        let last = flat(&lines).pop().expect("a last line");

        assert!(last.contains('y') && last.contains("run"), "{last}");
        assert!(last.contains('n') && last.contains("skip"), "{last}");
    }

    #[test]
    fn a_modal_fits_inside_the_frame_it_is_given() {
        for (width, height) in [(200u16, 60u16), (100, 40), (60, 20), (40, 12)] {
            let area = Rect::new(0, 0, width, height);
            let popup = centered(area, modal_width(width), height);
            assert!(popup.width <= width, "{popup:?} in {area:?}");
            assert!(popup.height <= height, "{popup:?} in {area:?}");
        }
    }

    #[test]
    fn the_modal_stays_a_comfortable_measure_on_a_wide_terminal() {
        assert_eq!(modal_width(200), MAX_WIDTH);
        assert!(modal_width(80) < MAX_WIDTH);
        assert!(modal_width(30) <= 30, "never wider than the terminal");
    }

    /// Print the modal, to look at it without running spill.
    ///
    /// ```text
    /// cargo test print_the_modal -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "prints the modal to look at; asserts nothing"]
    fn print_the_modal() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let cases = [
            (
                "edit_file",
                "edit /home/ramzal/Projects/spillover/src/ui/chat.rs — 1 occurrence\n\
                 - let gutter = if shape.compact { 0 } else { GUTTER };\n\
                 - let text_width = width.saturating_sub(gutter).max(1);\n\
                 + let text_width = width.saturating_sub(GUTTER).max(1);",
            ),
            (
                "run_shell",
                "run in /home/ramzal/Projects/spillover:\n  cargo test --all-targets",
            ),
            (
                "write_file",
                "overwrite /home/ramzal/Projects/spillover/README.md (8204 → 11454 bytes)",
            ),
        ];

        for (tool, preview) in cases {
            let (width, height) = (100u16, 30);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a terminal");
            let pending = approval(tool, preview);
            terminal
                .draw(|frame| render(frame, frame.area(), &pending, &Theme::default()))
                .expect("the modal should draw");

            let buffer = terminal.backend().buffer();
            let mut out = String::new();
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    out.push_str(buffer[(x, y)].symbol());
                }
                out.push('\n');
            }
            println!("\n===== {tool} =====\n{out}");
        }
    }
}
