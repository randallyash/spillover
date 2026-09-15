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
/// Rows of preview shown at once. Past this the preview scrolls, so a long diff
/// is read rather than truncated — being asked to approve a change you cannot
/// see the whole of is the one thing this modal must not do.
const MAX_BODY_ROWS: u16 = 20;
/// Rows of the frame the modal leaves alone, so it reads as an interruption
/// rather than taking the screen.
const FRAME_MARGIN: u16 = 6;

pub fn render(
    frame: &mut Frame,
    area: Rect,
    approval: &PendingApproval,
    theme: &Theme,
    scroll: u16,
) {
    let width = modal_width(area.width);
    let content_width = content_width(width);

    let preview = preview_lines(approval, theme, content_width);
    let capacity = body_capacity(area.height);
    let scroll = scroll.min(max_scroll(approval, theme, area));
    let shown = preview.len().min(capacity);
    let window = &preview[(scroll as usize).min(preview.len())..][..shown];

    // Six rows beyond the preview: two borders, the row the content starts
    // below the top border, a blank line, and the keys. Getting this wrong clips
    // the keys off the bottom of the box, which is how the answer keys became
    // invisible on a long preview.
    let height = shown as u16 + 6;
    let popup = centered(area, width, height);

    // Wipe whatever the transcript drew underneath, or the modal looks broken.
    frame.render_widget(Clear, popup);

    // When the preview scrolls, the title says where in it you are. Without
    // that the arrows would move text with nothing saying there is more of it.
    let position = if preview.len() > capacity {
        format!(
            " {} — {}/{} ",
            action(&approval.tool),
            scroll + 1,
            preview.len()
        )
    } else {
        format!(" {} ", action(&approval.tool))
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border_active)
        .title(Line::styled(position, theme.brand));
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

    let mut lines: Vec<Line<'static>> = window.to_vec();
    lines.push(Line::raw(""));
    lines.push(keys(theme, preview.len() > capacity));
    frame.render_widget(Paragraph::new(lines), content);
}

/// How many preview rows fit in a frame of this height.
pub fn body_capacity(area_height: u16) -> usize {
    let room = area_height.saturating_sub(FRAME_MARGIN).min(MAX_BODY_ROWS);
    room.max(1) as usize
}

/// The furthest the preview can scroll before it runs out of content.
pub fn max_scroll(approval: &PendingApproval, theme: &Theme, area: Rect) -> u16 {
    let width = modal_width(area.width);
    let lines = preview_lines(approval, theme, content_width(width));
    lines
        .len()
        .saturating_sub(body_capacity(area.height))
        .min(u16::MAX as usize) as u16
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

/// The preview on its own, wrapped ready to be windowed.
pub fn preview_lines(
    approval: &PendingApproval,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
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

    if lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines
}

/// The modal's content: the preview, then the keys.
#[cfg(test)]
pub fn body(approval: &PendingApproval, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let mut lines = preview_lines(approval, theme, width);
    lines.push(Line::raw(""));
    lines.push(keys(theme, false));
    lines
}

/// How one line of a preview is styled.
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

fn keys(theme: &Theme, scrollable: bool) -> Line<'static> {
    let mut spans = vec![
        Span::styled("y", theme.accent),
        Span::styled(" run", theme.hint),
        Span::styled("      ", theme.hint),
        Span::styled("n", theme.accent),
        Span::styled(" skip", theme.hint),
    ];
    // Only offered when there is something to scroll to, so the keys shown are
    // the keys that do anything.
    if scrollable {
        spans.push(Span::styled("      ", theme.hint));
        spans.push(Span::styled("↑↓", theme.accent));
        spans.push(Span::styled(" read it all", theme.hint));
    }
    Line::from(spans)
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

    // ---- scrolling --------------------------------------------------------

    /// A long diff, as the modal would receive it: enough lines to scroll.
    fn long_edit_preview() -> String {
        let mut preview = String::from("edit /tmp/big.rs — 60 occurrences\n");
        for n in 0..60 {
            preview.push_str(&format!("- old line {n}\n+ new line {n}\n"));
        }
        preview
    }

    fn long_edit() -> PendingApproval {
        approval("edit_file", &long_edit_preview())
    }

    fn rendered(approval: &PendingApproval, area: Rect, scroll: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(area.width, area.height);
        let mut terminal = ratatui::Terminal::new(backend).expect("a terminal");
        terminal
            .draw(|frame| render(frame, frame.area(), approval, &Theme::default(), scroll))
            .expect("the modal should draw");

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

    #[test]
    fn a_long_preview_can_be_read_to_the_end_by_scrolling() {
        // The whole point: being asked to approve a change you cannot see all of
        // is worse than being asked to approve nothing.
        let theme = Theme::default();
        let pending = long_edit();
        let area = Rect::new(0, 0, 100, 30);
        let last = max_scroll(&pending, &theme, area);
        assert!(last > 0, "this preview should need scrolling");

        let top = rendered(&pending, area, 0);
        assert!(top.contains("old line 0"), "{top}");
        let bottom = rendered(&pending, area, last);
        assert!(
            bottom.contains("new line 59"),
            "the end of the diff must be reachable: {bottom}"
        );
        assert!(
            !bottom.contains("old line 0"),
            "the start should have scrolled away: {bottom}"
        );
    }

    #[test]
    fn the_title_says_where_in_the_preview_you_are() {
        let pending = long_edit();
        let area = Rect::new(0, 0, 100, 30);
        let last = max_scroll(&pending, &Theme::default(), area);

        let top = rendered(&pending, area, 0);
        assert!(top.contains("1/"), "the position should be shown: {top}");
        let bottom = rendered(&pending, area, last);
        assert!(
            bottom.contains(&format!("{}/", last + 1)),
            "the position should move with the scroll: {bottom}"
        );
    }

    #[test]
    fn the_keys_stay_pinned_while_the_preview_scrolls() {
        let pending = long_edit();
        let area = Rect::new(0, 0, 100, 30);
        let bottom = rendered(
            &pending,
            area,
            max_scroll(&pending, &Theme::default(), area),
        );

        let last = bottom.lines().find(|line| line.contains("skip"));
        assert!(
            last.is_some(),
            "the answer keys must not scroll away: {bottom}"
        );
    }

    #[test]
    fn the_scroll_hint_appears_only_when_there_is_something_to_scroll_to() {
        let short = rendered(
            &approval("write_file", "create /tmp/a.txt"),
            Rect::new(0, 0, 100, 30),
            0,
        );
        assert!(
            !short.contains("read it all"),
            "offering a key that does nothing: {short}"
        );

        let long = rendered(&long_edit(), Rect::new(0, 0, 100, 30), 0);
        assert!(long.contains("read it all"), "{long}");
    }

    #[test]
    fn a_scroll_past_the_end_is_clamped_rather_than_drawn_empty() {
        // A stale offset from a resize must not leave a blank modal.
        let pending = long_edit();
        let area = Rect::new(0, 0, 100, 30);
        let clamped = rendered(&pending, area, u16::MAX);

        assert!(
            clamped.contains("new line 59"),
            "it should show the last page: {clamped}"
        );
    }

    #[test]
    fn a_preview_that_fits_is_shown_whole_with_no_scrolling() {
        let pending = approval("run_shell", "run in /tmp:\n  cargo test");
        let area = Rect::new(0, 0, 100, 30);

        assert_eq!(max_scroll(&pending, &Theme::default(), area), 0);
        let out = rendered(&pending, area, 0);
        assert!(out.contains("cargo test"), "{out}");
        assert!(
            !out.contains("1/"),
            "no position when there is one page: {out}"
        );
    }

    #[test]
    fn the_capacity_shrinks_with_the_frame_and_never_reaches_zero() {
        assert!(body_capacity(30) > body_capacity(12));
        assert_eq!(body_capacity(0), 1);
        assert_eq!(body_capacity(4), 1);
        // Capped, so a very tall terminal does not turn the modal into the screen.
        assert_eq!(body_capacity(200), MAX_BODY_ROWS as usize);
    }

    /// Print the modal, to look at it without running spill.
    #[test]
    #[ignore = "prints the modal to look at; asserts nothing"]
    fn print_the_modal() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let cases: Vec<(&str, &str, String, u16)> = vec![
            (
                "edit_file",
                "edit_file",
                "edit /home/ramzal/Projects/spillover/src/ui/chat.rs — 1 occurrence\n\
                 - let gutter = if shape.compact { 0 } else { GUTTER };\n\
                 - let text_width = width.saturating_sub(gutter).max(1);\n\
                 + let text_width = width.saturating_sub(GUTTER).max(1);"
                    .to_string(),
                0,
            ),
            (
                "run_shell",
                "run_shell",
                "run in /home/ramzal/Projects/spillover:\n  cargo test --all-targets".to_string(),
                0,
            ),
            (
                "write_file",
                "write_file",
                "overwrite /home/ramzal/Projects/spillover/README.md (8204 → 11454 bytes)"
                    .to_string(),
                0,
            ),
            (
                "a long diff, at the top",
                "edit_file",
                long_edit_preview(),
                0,
            ),
            (
                "the same diff, scrolled to the end",
                "edit_file",
                long_edit_preview(),
                96,
            ),
        ];

        for (label, tool, preview, scroll) in &cases {
            let (width, height) = (100u16, 30);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a terminal");
            let pending = approval(tool, preview);
            terminal
                .draw(|frame| render(frame, frame.area(), &pending, &Theme::default(), *scroll))
                .expect("the modal should draw");

            let buffer = terminal.backend().buffer();
            let mut out = String::new();
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    out.push_str(buffer[(x, y)].symbol());
                }
                out.push('\n');
            }
            println!("\n===== {label} =====\n{out}");
        }
    }
}
