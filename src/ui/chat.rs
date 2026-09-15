//! The transcript: the conversation, wrapped to the width it is given.
//!
//! The transcript is borderless and takes every column it can, because this is
//! the text people actually read. Structure comes from a left gutter instead: a
//! filled bar marks a message the user wrote, so a long session can be scanned
//! by finding where their own turns begin. The model's prose is left uncolored
//! and unstyled, which is the most readable thing a terminal can do with it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use crate::app::{App, Role};
use crate::text;
use crate::ui::theme::Theme;
use crate::ui::{BAR, CARET, markdown, spinner};

/// Columns the gutter takes. It is kept at every width: two columns is a small
/// price for being able to find your own turns, even on a narrow terminal.
const GUTTER: usize = 2;

/// How many ticks the caret holds each state. Three ticks is a shade under a
/// third of a second, which blinks without flickering.
const CARET_HOLD: u64 = 3;

/// Whether the streaming caret is in its visible phase.
pub fn caret_visible(tick: u64) -> bool {
    (tick / CARET_HOLD) % 2 == 0
}

pub fn render(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let visible = area.height as usize;

    // The scrollbar is a real column taken out of the text, so it never draws
    // over a character. When it appears the transcript is wrapped again for the
    // narrower width: wrapping for the full width and then rendering into one
    // column less would clip the last cell of every full line.
    let mut built = lines(app, theme, area.width as usize);
    let overflowed = built.len() > visible && area.width > 1;
    if overflowed {
        built = lines(app, theme, area.width as usize - 1);
    }

    let max_scroll = built.len().saturating_sub(visible);
    let scroll_back = (app.scroll_back as usize).min(max_scroll);
    app.scroll_back = scroll_back as u16;

    let end = built.len().saturating_sub(scroll_back);
    let start = end.saturating_sub(visible);
    let window: Vec<Line> = built[start..end].to_vec();

    let text_area = if overflowed {
        Rect {
            width: area.width - 1,
            ..area
        }
    } else {
        area
    };

    frame.render_widget(Paragraph::new(Text::from(window)), text_area);

    if overflowed {
        let track = Rect {
            x: area.x + area.width - 1,
            y: area.y,
            width: 1,
            height: area.height,
        };
        let mut state = ScrollbarState::new(built.len())
            .position(start)
            .viewport_content_length(visible);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            track,
            &mut state,
        );
    }
}

/// The whole transcript as styled lines.
pub fn lines(app: &App, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let text_width = width.saturating_sub(GUTTER).max(1);
    let mut out: Vec<Line<'static>> = Vec::new();

    for (index, message) in app.messages.iter().enumerate() {
        // A blank line between turns, so the transcript reads as speech rather
        // than as a log.
        if index > 0 {
            out.push(Line::raw(""));
        }

        let streaming = app.streaming_index() == Some(index) && caret_visible(app.tick);
        // The caret needs a cell of its own, or a full-width line plus the
        // caret would wrap and shift everything below it.
        let reserve = usize::from(streaming);
        let body_width = text_width.saturating_sub(reserve).max(1);

        let mut block = match message.role {
            // The model answers in markdown whether or not anyone asked, so its
            // prose is parsed rather than shown with the markers intact.
            Role::Assistant => markdown::lines(&message.text, body_width, theme, theme.assistant),
            _ => plain(
                &message.text,
                body_width,
                message.role,
                theme,
                Some(index) == app.running,
                app.tick,
            ),
        };

        for line in &mut block {
            line.spans.insert(0, gutter_span(message.role, theme));
        }

        if streaming {
            if let Some(last) = block.last_mut() {
                last.spans.push(Span::styled(CARET, theme.accent));
            }
        }

        out.extend(block);
    }

    out
}

/// A user turn or an app notice: wrapped as it was written, with no markdown.
fn plain(
    text: &str,
    width: usize,
    role: Role,
    theme: &Theme,
    running: bool,
    tick: u64,
) -> Vec<Line<'static>> {
    let style = style_for(role, text, theme);

    // A tool that is still running is marked by a spinner in place of its arrow,
    // so the line itself says whether the work has finished.
    let (lead, body) = if running {
        match text.strip_prefix("→ ") {
            Some(rest) => (
                Some(Span::styled(format!("{} ", spinner(tick)), theme.accent)),
                rest.to_string(),
            ),
            None => (None, text.to_string()),
        }
    } else {
        (None, text.to_string())
    };

    text::wrap(&body, width)
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if index == 0 {
                if let Some(lead) = &lead {
                    spans.push(lead.clone());
                }
            } else if lead.is_some() {
                // Continue under the spinner rather than under the margin.
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(part, style));
            Line::from(spans)
        })
        .collect()
}

/// What sits in the gutter: a bar beside the user's own words, nothing beside
/// the model's.
fn gutter_span(role: Role, theme: &Theme) -> Span<'static> {
    match role {
        Role::User => Span::styled(format!("{BAR} "), theme.accent),
        Role::Assistant | Role::System => Span::raw(" ".repeat(GUTTER)),
    }
}

/// How a message is drawn.
fn style_for(role: Role, text: &str, theme: &Theme) -> ratatui::style::Style {
    match role {
        Role::User => theme.user,
        Role::Assistant => theme.assistant,
        Role::System => match text.chars().next() {
            Some('✓') => theme.success,
            Some('✗') => theme.error,
            Some('!') => theme.warn,
            _ => theme.system,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Message;
    use crate::config::Config;
    use ratatui::style::Style;

    fn app_with(roles_and_text: &[(Role, &str)]) -> App {
        let mut app = App::new(Config::default());
        app.messages.clear();
        for (role, text) in roles_and_text {
            app.messages.push(Message {
                role: *role,
                text: (*text).to_string(),
            });
        }
        app
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
    fn the_users_own_turn_is_marked_by_a_bar() {
        let app = app_with(&[(Role::User, "hello"), (Role::Assistant, "hi there")]);
        let text = flat(&lines(&app, &Theme::default(), 60));

        assert!(text.iter().any(|line| line.starts_with(BAR)), "{text:?}");
        // The model's reply carries no bar, which is what makes the user's
        // turns findable at a glance.
        let reply = text.iter().find(|line| line.contains("hi there")).unwrap();
        assert!(!reply.starts_with(BAR), "{reply:?}");
    }

    #[test]
    fn turns_are_separated_by_a_blank_line() {
        let app = app_with(&[(Role::User, "one"), (Role::Assistant, "two")]);
        let text = flat(&lines(&app, &Theme::default(), 60));

        assert!(text[0].contains("one"), "{text:?}");
        assert_eq!(text[1], "", "the turn should be followed by a blank line");
        assert!(text[2].contains("two"), "{text:?}");
    }

    #[test]
    fn a_wrapped_turn_keeps_its_gutter_aligned() {
        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let app = app_with(&[(Role::User, long)]);
        let text = flat(&lines(&app, &Theme::default(), 40));

        assert!(text.len() > 1, "it should have wrapped: {text:?}");
        for line in &text {
            assert!(line.starts_with(BAR), "every line of the block: {line:?}");
        }
    }

    #[test]
    fn the_users_bar_survives_a_narrow_terminal() {
        // Two columns is a small price for being able to find your own turns,
        // even at the narrowest width the interface supports.
        let app = app_with(&[(Role::User, "hello")]);
        let text = flat(&lines(&app, &Theme::default(), 46));
        assert!(text[0].starts_with(BAR), "{text:?}");
    }

    #[test]
    fn no_line_is_wider_than_the_space_it_was_given() {
        let long = "supercalifragilisticexpialidocious and then some more words after it";
        for width in [20usize, 24, 33, 47, 60, 80] {
            let app = app_with(&[(Role::User, long), (Role::Assistant, long)]);
            for line in lines(&app, &Theme::default(), width) {
                assert!(
                    line.width() <= width,
                    "line overflowed {width}: {:?}",
                    flat(std::slice::from_ref(&line))
                );
            }
        }
    }

    #[test]
    fn the_caret_marks_the_message_being_streamed() {
        // Driven through the event the agent really sends, so this covers the
        // path that sets the streaming index rather than assuming it.
        let mut app = app_with(&[(Role::User, "ask")]);
        app.handle_agent_event(crate::agent::AgentEvent::Text("answering".to_string()));

        app.tick = 0; // visible phase
        let shown = flat(&lines(&app, &Theme::default(), 60));
        assert!(shown.iter().any(|line| line.ends_with(CARET)), "{shown:?}");

        app.tick = CARET_HOLD; // hidden phase
        let hidden = flat(&lines(&app, &Theme::default(), 60));
        assert!(
            !hidden.iter().any(|line| line.ends_with(CARET)),
            "{hidden:?}"
        );
    }

    #[test]
    fn a_streaming_line_still_fits_once_the_caret_is_added() {
        // The caret must not push a full line over the edge.
        let mut app = app_with(&[(Role::Assistant, &"x".repeat(80))]);
        app.handle_agent_event(crate::agent::AgentEvent::Text("y".to_string()));
        app.tick = 0;

        for line in lines(&app, &Theme::default(), 80) {
            assert!(
                line.width() <= 80,
                "{:?}",
                flat(std::slice::from_ref(&line))
            );
        }
    }

    #[test]
    fn a_finished_turn_leaves_no_caret_behind() {
        // No turn in flight, so the transcript is just the text.
        let app = app_with(&[(Role::Assistant, "answer")]);
        let text = flat(&lines(&app, &Theme::default(), 60));
        assert_eq!(text, vec!["  answer"]);
    }

    #[test]
    fn the_caret_blinks_rather_than_staying_on() {
        // Both phases occur within a short window, or it is not a blink.
        let states: Vec<bool> = (0..CARET_HOLD * 2).map(caret_visible).collect();
        assert!(states.contains(&true));
        assert!(states.contains(&false));
        assert!(caret_visible(0));
        assert_eq!(
            caret_visible(u64::MAX),
            caret_visible(u64::MAX % (CARET_HOLD * 2))
        );
    }

    #[test]
    fn an_empty_transcript_draws_nothing() {
        let app = App::new(Config::default());
        let mut app = app;
        app.messages.clear();
        assert!(lines(&app, &Theme::default(), 60).is_empty());
    }

    #[test]
    fn a_tool_outcome_is_readable_by_its_glyph_as_well_as_its_colour() {
        let theme = Theme::default();
        let app = app_with(&[
            (Role::System, "✓ read_file  notes.txt (3 lines)"),
            (Role::System, "✗ write_file  permission denied"),
            (Role::System, "! the model hit its output limit"),
            (Role::System, "→ run_shell  cargo test"),
            (Role::System, "tokens: 120 in, 45 out"),
        ]);

        let styles: Vec<_> = lines(&app, &theme, 60)
            .iter()
            .filter(|line| !line.spans.is_empty())
            .map(|line| line.spans.last().expect("a body span").style)
            .collect();

        assert_eq!(styles[0], theme.success, "a check mark means it worked");
        assert_eq!(styles[1], theme.error, "a cross means it did not");
        assert_eq!(styles[2], theme.warn, "a bang is worth noticing");
        assert_eq!(styles[3], theme.system, "a running tool is quiet");
        assert_eq!(styles[4], theme.system, "so is a token count");
    }

    #[test]
    fn the_prose_itself_is_never_coloured() {
        // The model's answer is the text being read; it takes the terminal's
        // own foreground so it is correct on a light background too.
        let theme = Theme::default();
        let app = app_with(&[(Role::Assistant, "an answer")]);
        let line = &lines(&app, &theme, 60)[0];
        assert_eq!(
            line.spans.last().expect("a body span").style,
            Style::default()
        );
    }

    #[test]
    fn a_running_tool_spins_and_a_finished_one_shows_its_arrow() {
        let mut app = app_with(&[]);
        app.handle_agent_event(crate::agent::AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            preview: "read notes.txt".to_string(),
        });
        app.tick = 3;

        let running = flat(&lines(&app, &Theme::default(), 60));
        assert!(
            running
                .iter()
                .any(|line| line.contains(crate::ui::spinner(3)) && line.contains("read_file")),
            "the running tool should spin: {running:?}"
        );

        app.handle_agent_event(crate::agent::AgentEvent::ToolFinished {
            name: "read_file".to_string(),
            ok: true,
            summary: "notes.txt (3 lines)".to_string(),
        });

        let done = flat(&lines(&app, &Theme::default(), 60));
        assert!(
            !done.iter().any(|line| line.contains(crate::ui::spinner(3))),
            "nothing should still be spinning: {done:?}"
        );
        assert!(
            done.iter().any(|line| line.contains("→ read_file")),
            "the attempt should stay in the transcript: {done:?}"
        );
    }

    #[test]
    fn the_models_markdown_is_rendered_rather_than_shown_literal() {
        let app = app_with(&[
            (Role::Assistant, "## Answer"),
            (Role::Assistant, "use **bold** and `code`"),
        ]);
        let text = flat(&lines(&app, &Theme::default(), 60));

        assert!(text.iter().any(|line| line == "  Answer"), "{text:?}");
        let prose = text.iter().find(|line| line.contains("bold")).unwrap();
        assert_eq!(prose, "  use bold and code", "{text:?}");
        assert!(!text.iter().any(|line| line.contains("**")), "{text:?}");
    }
}
