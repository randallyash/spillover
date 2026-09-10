//! Drawing the setup wizard.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::setup::{Choice, Step, Wizard};
use crate::ui::theme::Theme;

pub fn render(frame: &mut Frame, wizard: &Wizard) {
    let theme = Theme::default();
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas::<3>(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" spill setup ", theme.title),
            Span::styled(" · ", theme.hint),
            Span::styled(heading(wizard), theme.hint),
        ])),
        areas[0],
    );

    match wizard.step {
        Step::Finding => finding(frame, areas[1], &theme),
        Step::ChooseLocal => {
            let hint = "Choose the model spill should try first.";
            list(
                frame,
                areas[1],
                hint,
                &rows_of(&wizard.local),
                wizard.local_cursor,
                &theme,
            )
        }
        Step::ChooseOnline => {
            let hint = if wizard.online_slot() == 0 {
                "Add your first online fallback, or skip to finish."
            } else {
                "Add a final fallback, or skip to finish."
            };
            let available = wizard.available_online();
            list(
                frame,
                areas[1],
                hint,
                &rows_of(&available),
                wizard.online_cursor,
                &theme,
            )
        }
        Step::CustomLocal => text_step(
            frame,
            areas[1],
            "Address of your local server",
            "Something like http://localhost:1234/v1",
            wizard,
            &theme,
        ),
        Step::ChooseModel => choose_model(frame, areas[1], wizard, &theme),
        Step::CustomOnline => text_step(
            frame,
            areas[1],
            "Address of the endpoint",
            "Something like https://gateway.example.com/v1",
            wizard,
            &theme,
        ),
        Step::OnlineModel => text_step(
            frame,
            areas[1],
            "Which model should this endpoint use?",
            "For example deepseek/deepseek-v4-flash",
            wizard,
            &theme,
        ),
        Step::Review => review(frame, areas[1], wizard, &theme),
    }

    frame.render_widget(
        Paragraph::new(Line::styled(hints(wizard), theme.hint)),
        areas[2],
    );
}

fn heading(wizard: &Wizard) -> &'static str {
    match wizard.step {
        Step::Finding => "looking for a local model",
        Step::ChooseLocal => "step 1 of 3 · local model",
        Step::ChooseOnline => "step 2 of 3 · online fallbacks",
        Step::ChooseModel | Step::CustomLocal | Step::CustomOnline | Step::OnlineModel => {
            "step 2 of 3 · details"
        }
        Step::Review => "step 3 of 3 · review",
    }
}

fn hints(wizard: &Wizard) -> &'static str {
    match wizard.step {
        Step::Finding => "probing the usual local ports…",
        Step::ChooseLocal => "↑/↓ move   ·   Enter choose   ·   q quit",
        Step::ChooseOnline => "↑/↓ move   ·   Enter choose   ·   s skip   ·   b back   ·   q quit",
        Step::ChooseModel => "↑/↓ move   ·   Enter choose   ·   Esc back   ·   q quit",
        Step::CustomLocal | Step::CustomOnline | Step::OnlineModel => {
            "type   ·   Enter accept   ·   Esc back   ·   q quit"
        }
        Step::Review => "w write the config   ·   b back   ·   q quit",
    }
}

/// The models an endpoint advertises, plus a way to type one instead.
fn choose_model(frame: &mut Frame, area: Rect, wizard: &Wizard, theme: &Theme) {
    if wizard.models_loading {
        let text = Text::from(vec![
            Line::raw(""),
            Line::styled("  Asking the endpoint which models it offers…", theme.hint),
        ]);
        frame.render_widget(Paragraph::new(text), area);
        return;
    }

    let mut rows: Vec<(String, String)> = wizard
        .models
        .iter()
        .map(|model| (model.clone(), String::new()))
        .collect();
    rows.push(("Type a model id myself".to_string(), String::new()));

    if let Some(problem) = &wizard.models_problem {
        rows.insert(0, (String::from("…"), problem.clone()));
    }

    list(
        frame,
        area,
        "Which model should this endpoint use?",
        &rows,
        wizard.model_cursor,
        theme,
    );
}

fn finding(frame: &mut Frame, area: Rect, theme: &Theme) {
    let text = Text::from(vec![
        Line::raw(""),
        Line::styled(
            "  Looking for a local model — LM Studio, Ollama, llama.cpp, vLLM…",
            theme.title,
        ),
        Line::raw(""),
        Line::styled(
            "  If none is running, you can still choose an online model next.",
            theme.hint,
        ),
    ]);
    frame.render_widget(Paragraph::new(text), area);
}

/// Draw a selectable list of `(label, detail)` rows.
fn list(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    choices: &[(String, String)],
    cursor: usize,
    theme: &Theme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border)
        .title(format!(" {title} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    if choices.is_empty() {
        lines.push(Line::styled("  nothing to choose from", theme.hint));
    }

    for (index, (label, detail)) in choices.iter().enumerate() {
        let selected = index == cursor;
        let marker = if selected { "▸ " } else { "  " };
        let style = if selected {
            theme.user.add_modifier(Modifier::BOLD)
        } else {
            theme.title
        };
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::styled(label.clone(), style),
        ]));
        if !detail.is_empty() {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(detail.clone(), theme.hint),
            ]));
        }
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

/// The choice lists are pairs of label and detail, like the model list.
fn rows_of(choices: &[Choice]) -> Vec<(String, String)> {
    choices
        .iter()
        .map(|choice| (choice.name.clone(), choice.detail.clone()))
        .collect()
}

fn text_step(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    placeholder: &str,
    wizard: &Wizard,
    theme: &Theme,
) {
    let chunks = Layout::vertical([Constraint::Length(5), Constraint::Min(0)]).areas::<2>(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border)
        .title(format!(" {title} "));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let body = if wizard.text.is_empty() {
        Line::styled(placeholder, theme.hint)
    } else {
        Line::raw(wizard.text.clone())
    };
    frame.render_widget(Paragraph::new(Text::from(body)), inner);

    // Caret at the end of what has been typed.
    if inner.width > 0 {
        let typed = crate::text::display_width(&wizard.text);
        let limit = inner.width.saturating_sub(1) as usize;
        frame.set_cursor_position((inner.x + typed.min(limit) as u16, inner.y));
    }

    if let Some(error) = &wizard.problem {
        frame.render_widget(
            Paragraph::new(Line::styled(error.to_string(), theme.error)),
            chunks[1],
        );
    }
}

fn review(frame: &mut Frame, area: Rect, wizard: &Wizard, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(6), Constraint::Length(10)]).areas::<2>(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border)
        .title(" Your tiers, in order ");
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let mut lines: Vec<Line> = Vec::new();
    for (index, tier) in wizard.tiers().iter().enumerate() {
        let role = if index == 0 {
            "tried first"
        } else {
            "fallback"
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{}. ", index + 1), theme.title),
            Span::styled(
                tier.name.clone().unwrap_or_else(|| tier.id.clone()),
                theme.title.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  ({role}, {})", tier.kind), theme.hint),
        ]));
        lines.push(Line::raw(
            tier.base_url
                .clone()
                .or_else(|| tier.preset.clone())
                .unwrap_or_default(),
        ));

        match wizard.checks.get(index).and_then(|check| check.as_ref()) {
            Some(readiness) if readiness.ok => lines.push(Line::styled(
                format!("  ✓ {}", readiness.detail),
                theme.assistant,
            )),
            Some(readiness) => lines.push(Line::styled(
                format!("  ✗ {}", readiness.detail),
                theme.error,
            )),
            None => lines.push(Line::styled("  checking…", theme.hint)),
        }
        lines.push(Line::raw(""));
    }

    if wizard.tiers().is_empty() {
        lines.push(Line::styled(
            "No tiers yet. Go back and choose at least one.",
            theme.error,
        ));
    }

    // Anything that went wrong, such as a config that could not be written.
    if let Some(problem) = &wizard.problem {
        lines.push(Line::styled(problem.clone(), theme.error));
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );

    // A peek at the file itself, so there are no surprises.
    let preview: Vec<Line> = wizard
        .to_toml()
        .lines()
        .take(9)
        .map(|line| {
            let style = if line.trim_start().starts_with('#') {
                theme.hint
            } else {
                theme.title
            };
            Line::styled(line.to_string(), style)
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border)
        .title(" config.toml ");
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);
    frame.render_widget(Paragraph::new(Text::from(preview)), inner);
}

/// Confirm the write before anything is replaced.
pub fn confirm_overwrite(frame: &mut Frame, path: &str, theme: &Theme) {
    let area = centered(frame.area(), 62, 7);
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.error)
        .title(" Replace your config? ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::raw(format!("{path} already exists.")),
        Line::raw(""),
        Line::styled("It will be copied to config.toml.bak first.", theme.hint),
        Line::styled("[y] replace        [n] go back", theme.user),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preset::Library;

    #[test]
    fn every_step_has_a_heading_and_a_hint() {
        let mut wizard = Wizard::new(&Library::embedded());
        for step in [
            Step::Finding,
            Step::ChooseLocal,
            Step::CustomLocal,
            Step::ChooseOnline,
            Step::CustomOnline,
            Step::ChooseModel,
            Step::OnlineModel,
            Step::Review,
        ] {
            wizard.step = step;
            assert!(!heading(&wizard).is_empty(), "{step:?} has no heading");
            assert!(!hints(&wizard).is_empty(), "{step:?} has no hints");
        }
    }

    #[test]
    fn a_popup_fits_inside_a_small_terminal() {
        let popup = centered(Rect::new(0, 0, 20, 6), 62, 7);
        assert!(popup.width <= 20);
        assert!(popup.height <= 6);
    }

    #[test]
    fn the_online_heading_changes_with_which_fallback_is_being_asked_for() {
        let mut wizard = Wizard::new(&Library::embedded());
        wizard.step = Step::ChooseOnline;
        assert!(wizard.online_slot() == 0);
        // Nothing chosen yet, so this is the first fallback.
        assert_eq!(heading(&wizard), "step 2 of 3 · online fallbacks");
    }
}
