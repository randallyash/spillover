//! The session panel: what the chain is doing and what it has cost.
//!
//! An inspector rather than a second conversation. It answers the questions a
//! long session raises — which tier am I on, how much of the chain is left, what
//! has this cost so far — and it only appears when the terminal is wide enough
//! to afford it, because the conversation matters more.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline};

use crate::app::App;
use crate::config::OnStuck;
use crate::text;
use crate::ui::theme::Theme;

/// Space between the panel's border and its content.
const PAD: u16 = 1;
/// Columns the label column takes, values starting after it.
const LABEL: usize = 10;
/// Indent for a value under its group heading.
const INDENT: usize = 2;

pub fn render(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(theme.border);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let content = Rect {
        x: inner.x.saturating_add(PAD),
        y: inner.y,
        width: inner.width.saturating_sub(PAD * 2),
        height: inner.height,
    };
    if content.width == 0 || content.height == 0 {
        return;
    }

    let width = content.width as usize;
    let value_width = value_width(width);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::styled("session", theme.title));
    if !app.session_title.is_empty() {
        lines.extend(pair("name", &app.session_title, theme, value_width));
    }
    lines.push(Line::raw(""));

    if app.tier_labels.is_empty() {
        lines.push(Line::styled("no tier is attached", theme.faint));
        frame.render_widget(Paragraph::new(lines), content);
        return;
    }

    lines.push(section_with_value("chain", &position(app), theme, width));
    lines.extend(chain_rows(app, theme, width));
    lines.extend(pair("fallback", &fallback(app), theme, value_width));
    lines.extend(policy_row(app, theme, value_width));

    lines.push(Line::raw(""));
    lines.push(Line::styled("usage", theme.section));
    lines.extend(pair("tokens", &tokens(app), theme, value_width));
    lines.extend(pair("cached", &cached(app), theme, value_width));
    lines.extend(pair("turns", &app.turns.to_string(), theme, value_width));

    lines.push(Line::raw(""));
    lines.push(Line::styled("context", theme.section));
    lines.extend(pair(
        "workspace",
        &tilde(&app.config.general.workspace_path().display().to_string()),
        theme,
        value_width,
    ));

    // The chart follows the numbers it graphs rather than being pinned to the
    // bottom of the panel, which would leave a dead gap between the two. It is
    // dropped rather than clipped when the terminal is too short for both.
    let text_height = lines.len() as u16;
    let room_for_chart = text_height.saturating_add(2) <= content.height;

    let text_area = Rect {
        height: if room_for_chart {
            text_height
        } else {
            content.height
        },
        ..content
    };
    frame.render_widget(Paragraph::new(lines), text_area);

    if room_for_chart {
        let chart = Rect {
            y: content.y + text_height,
            height: 2,
            ..content
        };
        draw_sparkline(frame, chart, app, theme, width);
    }
}

/// The per-turn token spend, drawn as block glyphs.
fn draw_sparkline(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, width: usize) {
    if area.height < 2 {
        return;
    }
    let [label, graph] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas::<2>(area);

    frame.render_widget(
        Paragraph::new(Line::styled("cost per turn", theme.section)),
        label,
    );

    if app.usage_history.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled("no turns yet", theme.faint)),
            graph,
        );
        return;
    }

    // Only the most recent turns fit; the graph is a window on the session, not
    // a compressed picture of all of it.
    let data = &app.usage_history[app.usage_history.len().saturating_sub(width)..];
    frame.render_widget(Sparkline::default().data(data).style(theme.spark), graph);
}

/// One row per tier: its state as a glyph, its name, and what that state is.
fn chain_rows(app: &App, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();

    for (index, label) in app.tier_labels.iter().enumerate() {
        let failed = app.tier_failed.get(index).copied().unwrap_or(false);
        let active = index == app.active_tier;

        let (glyph, glyph_style) = if failed {
            ("✗", theme.tier_failed)
        } else if active {
            ("●", theme.accent)
        } else {
            ("○", theme.faint)
        };
        let (name_style, status) = if failed {
            (theme.tier_failed, "spilled")
        } else if active {
            (theme.heading, "active")
        } else {
            (theme.faint, "next")
        };

        let room = width.saturating_sub(INDENT + 2);
        let name = clip(crate::ui::short_label(label), room);
        let mut spans = vec![
            Span::raw(" ".repeat(INDENT)),
            Span::styled(format!("{glyph} "), glyph_style),
            Span::styled(name.clone(), name_style),
        ];

        // The state word is right-aligned when it fits, so the column of states
        // can be scanned without reading each name.
        let used = INDENT + 2 + text::display_width(&name);
        let status_width = text::display_width(status);
        if used + 1 + status_width <= width {
            spans.push(Span::raw(" ".repeat(width - used - status_width)));
            spans.push(Span::styled(status.to_string(), theme.faint));
        }

        out.push(Line::from(spans));
    }

    out
}

/// A group heading with a value pushed to the right edge.
fn section_with_value(name: &str, value: &str, theme: &Theme, width: usize) -> Line<'static> {
    let name_width = text::display_width(name);
    let value_width = text::display_width(value);
    let mut spans = vec![Span::styled(name.to_string(), theme.section)];

    if name_width + 1 + value_width <= width {
        spans.push(Span::raw(" ".repeat(width - name_width - value_width)));
        spans.push(Span::styled(value.to_string(), theme.faint));
    }

    Line::from(spans)
}

/// Cut a string to a display width, keeping whole characters.
fn clip(source: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in source.chars() {
        let ch_width = text::display_width(&ch.to_string());
        if used + ch_width > width {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out
}

/// Columns left for a value once the indent, the label and the space after it
/// have been taken. Getting this wrong clips the last character off every
/// value, which is how a model name ends up reading "DeepSeek V4 Flas".
fn value_width(content_width: usize) -> usize {
    content_width.saturating_sub(INDENT + LABEL + 1).max(1)
}

/// A label and its value, wrapped under itself so a long path stays readable.
fn pair(label: &str, value: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    pair_styled(label, value, Style::default(), theme, width)
}

/// The same, with the value styled. Used where a value is worth noticing rather
/// than merely reading.
fn pair_styled(
    label: &str,
    value: &str,
    value_style: Style,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let indent = " ".repeat(INDENT);
    let wrapped = text::wrap(value, width);
    let mut lines = Vec::new();

    for (index, part) in wrapped.iter().enumerate() {
        let lead = if index == 0 {
            let padded = format!("{label:<LABEL$}");
            Span::styled(format!("{indent}{padded} "), theme.hint)
        } else {
            Span::raw(format!("{indent}{:<LABEL$} ", ""))
        };
        lines.push(Line::from(vec![
            lead,
            Span::styled(part.clone(), value_style),
        ]));
    }

    lines
}

/// What a stuck tier does, which is the policy worth watching in a session.
fn policy_row(app: &App, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let policy = app.on_stuck();
    let value = match policy {
        OnStuck::Consult => "consult",
        OnStuck::Escalate => "escalate",
    };
    let style = match policy {
        OnStuck::Escalate => theme.warn,
        OnStuck::Consult => Style::default(),
    };

    // Marked when the session chose it rather than the config, so a policy that
    // outlives its experiment is visible. The word carries the meaning; this is
    // a second reading of it.
    let label = if app.on_stuck_is_chosen() {
        "on stuck *"
    } else {
        "on stuck"
    };

    pair_styled(label, value, style, theme, width)
}

/// "2 of 3", which is the number that matters when a chain is failing.
fn position(app: &App) -> String {
    format!(
        "{} of {}",
        app.active_tier + 1,
        app.tier_labels.len().max(1)
    )
}

fn fallback(app: &App) -> String {
    if app.config.general.sticky_fallback {
        "sticky".to_string()
    } else {
        "per turn".to_string()
    }
}

fn tokens(app: &App) -> String {
    if app.tokens_in == 0 && app.tokens_out == 0 {
        return "none yet".to_string();
    }
    format!(
        "{} in · {} out",
        text::thousands(app.tokens_in),
        text::thousands(app.tokens_out)
    )
}

/// Cache reads, shown only once a tier has reported one: a provider without a
/// cache should not be given a line of zeros to explain away.
fn cached(app: &App) -> String {
    if app.cache_read == 0 && app.cache_write == 0 {
        return "none reported".to_string();
    }
    if app.cache_write > 0 {
        format!(
            "{} read · {} written",
            text::thousands(app.cache_read),
            text::thousands(app.cache_write)
        )
    } else {
        text::thousands(app.cache_read)
    }
}

/// Replace a home-directory prefix with `~`, the way a shell would show it.
fn tilde(path: &str) -> String {
    crate::ui::short_path(std::path::Path::new(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn app_with(labels: &[&str]) -> App {
        let mut app = App::new(Config::default());
        app.tier_labels = labels.iter().map(|label| label.to_string()).collect();
        app.tier_failed = vec![false; labels.len()];
        app
    }

    #[test]
    fn the_position_counts_from_one_and_names_the_total() {
        let mut app = app_with(&["Local", "DeepSeek", "Grok"]);
        assert_eq!(position(&app), "1 of 3");

        app.activate_tier("Grok");
        assert_eq!(position(&app), "3 of 3");
    }

    #[test]
    fn every_tier_is_a_row_with_its_own_state() {
        let mut app = app_with(&["Local", "DeepSeek", "Grok"]);
        app.fail_tier("Local");
        app.activate_tier("DeepSeek");

        let theme = Theme::default();
        let text: Vec<String> = chain_rows(&app, &theme, 29)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert!(
            text[0].contains('✗') && text[0].contains("Local") && text[0].contains("spilled"),
            "{text:?}"
        );
        assert!(
            text[1].contains('●') && text[1].contains("DeepSeek") && text[1].contains("active"),
            "{text:?}"
        );
        assert!(
            text[2].contains('○') && text[2].contains("Grok") && text[2].contains("next"),
            "{text:?}"
        );
    }

    #[test]
    fn a_chain_row_never_widens_past_the_panel() {
        let app = app_with(&["A remarkably long tier name", "Grok"]);
        for row in chain_rows(&app, &Theme::default(), 20) {
            assert!(row.width() <= 20, "the row would clip: {row:?}");
        }
    }

    /// The panel's rows as plain text.
    fn rows(app: &App) -> Vec<String> {
        let theme = Theme::default();
        let width = value_width(29);
        let mut lines = Vec::new();
        lines.extend(pair("fallback", &fallback(app), &theme, width));
        lines.extend(policy_row(app, &theme, width));
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_panel_says_which_stuck_policy_is_live() {
        let app = app_with(&["Local", "DeepSeek"]);
        let text = rows(&app).join("\n");

        assert!(text.contains("on stuck"), "{text}");
        assert!(
            text.contains("consult"),
            "the policy in force should be named: {text}"
        );
        // And it is a separate line from the fallback policy, because they are
        // different questions: whether a spill happens, and whether it sticks.
        let fallback = rows(&app)[0].clone();
        assert!(fallback.contains("fallback"), "{fallback}");
        assert!(!fallback.contains("consult"), "{fallback}");
    }

    #[test]
    fn the_panel_follows_a_session_choice_and_marks_it_as_chosen() {
        let mut app = app_with(&["Local", "DeepSeek"]);
        app.on_stuck = Some(OnStuck::Consult);

        let text = rows(&app).join("\n");
        assert!(text.contains("consult"), "{text}");
        assert!(
            text.contains("on stuck *"),
            "a policy chosen for the session should be marked as such: {text}"
        );
    }

    #[test]
    fn the_panel_does_not_mark_a_policy_that_came_from_the_config() {
        let app = app_with(&["Local", "DeepSeek"]);
        let text = rows(&app).join("\n");

        assert!(text.contains("on stuck"), "{text}");
        assert!(
            !text.contains("on stuck *"),
            "nothing was chosen, so nothing should look altered: {text}"
        );
    }

    #[test]
    fn escalate_is_drawn_in_the_warning_colour_and_consult_is_not() {
        // Escalating is the departure from the default and the expensive one:
        // the turn goes to the tier below whole, and the session stays there. It
        // is worth noticing — with the word carrying the meaning either way.
        let theme = Theme::default();

        let mut chosen = app_with(&["Local", "DeepSeek"]);
        chosen.on_stuck = Some(OnStuck::Escalate);
        let escalate = policy_row(&chosen, &theme, value_width(29));
        let value = &escalate[0].spans[1];
        assert_eq!(value.content, "escalate");
        assert_eq!(value.style, theme.warn);

        let mut configured = app_with(&["Local", "DeepSeek"]);
        configured.on_stuck = Some(OnStuck::Consult);
        let consult = policy_row(&configured, &theme, value_width(29));
        assert_eq!(consult[0].spans[1].content, "consult");
        assert_eq!(
            consult[0].spans[1].style,
            Style::default(),
            "the default should not shout"
        );
    }

    #[test]
    fn the_label_and_the_value_both_fit_the_panel() {
        // The chosen marker makes the label ten characters, which is exactly the
        // label column: one more and it would push the value over the edge.
        let mut app = app_with(&["Local", "DeepSeek"]);
        app.on_stuck = Some(OnStuck::Consult);

        for line in rows(&app) {
            assert!(
                text::display_width(&line) <= 29,
                "the panel would clip this: {line:?}"
            );
        }
    }

    #[test]
    fn tokens_read_naturally_before_anything_has_run() {
        let app = app_with(&["Local"]);
        assert_eq!(tokens(&app), "none yet");
        assert_eq!(cached(&app), "none reported");
    }

    #[test]
    fn tokens_are_grouped_once_a_turn_has_been_recorded() {
        let mut app = app_with(&["Local"]);
        app.record_usage(&crate::provider::Usage {
            prompt_tokens: 15_360,
            completion_tokens: 2,
            cache_read_tokens: 7_424,
            cache_write_tokens: 0,
        });

        assert_eq!(tokens(&app), "15,360 in · 2 out");
        assert_eq!(cached(&app), "7,424");
    }

    #[test]
    fn a_cache_write_is_shown_only_when_a_provider_charges_for_one() {
        let mut app = app_with(&["Local"]);
        app.record_usage(&crate::provider::Usage {
            prompt_tokens: 100,
            completion_tokens: 1,
            cache_read_tokens: 50,
            cache_write_tokens: 25,
        });
        assert_eq!(cached(&app), "50 read · 25 written");
    }

    #[test]
    fn usage_accumulates_across_turns_and_tiers() {
        let mut app = app_with(&["Local", "Grok"]);
        let turn = crate::provider::Usage {
            prompt_tokens: 1_500,
            completion_tokens: 10,
            cache_read_tokens: 500,
            cache_write_tokens: 0,
        };

        app.record_usage(&turn);
        app.record_usage(&turn);

        assert_eq!(app.tokens_in, 3_000);
        assert_eq!(app.tokens_out, 20);
        assert_eq!(app.cache_read, 1_000);
        assert_eq!(tokens(&app), "3,000 in · 20 out");
    }

    #[test]
    fn the_fallback_policy_is_stated_in_words() {
        let app = app_with(&["Local"]);
        assert_eq!(fallback(&app), "sticky", "sticky is the default");
    }

    #[test]
    fn a_home_directory_is_shortened_to_a_tilde() {
        let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
        else {
            return;
        };
        let home = home.display().to_string();

        assert_eq!(tilde(&home), "~");
        assert_eq!(
            tilde(&format!("{home}/Projects/spillover")),
            "~/Projects/spillover"
        );
        // Somewhere else is left alone.
        assert_eq!(tilde("/var/tmp"), "/var/tmp");
    }

    #[test]
    fn the_value_column_leaves_room_for_its_label_and_the_space_after_it() {
        // Two of indent, ten of label, one space, then the value.
        assert_eq!(INDENT + LABEL + 1 + value_width(29), 29);
        assert_eq!(value_width(29), 16);
        assert_eq!(value_width(2), 1, "it never reaches zero");
    }

    #[test]
    fn a_long_value_wraps_rather_than_losing_its_last_character() {
        let theme = Theme::default();
        let width = value_width(29);
        let lines = pair("model", "DeepSeek V4 Flash", &theme, width);

        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert!(text.len() > 1, "it should have wrapped: {text:?}");
        // Normalised, because a wrapped value is split across two lines and the
        // label column sits between the halves.
        let joined = text
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("DeepSeek V4 Flash"),
            "no character may be lost: {text:?}"
        );
        for line in &text {
            assert!(
                text::display_width(line) <= 29,
                "the panel would clip this: {line:?}"
            );
        }
    }

    #[test]
    fn a_short_value_stays_on_one_line() {
        let theme = Theme::default();
        let lines = pair("fallback", "sticky", &theme, value_width(29));
        assert_eq!(lines.len(), 1);
    }
}
