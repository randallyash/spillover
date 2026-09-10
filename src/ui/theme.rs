//! Colors and text styles.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub user: Style,
    pub assistant: Style,
    pub system: Style,
    pub border: Style,
    pub title: Style,
    pub hint: Style,
    pub error: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            assistant: Style::default().fg(Color::Green),
            system: Style::default().fg(Color::DarkGray),
            border: Style::default().fg(Color::DarkGray),
            title: Style::default().add_modifier(Modifier::BOLD),
            hint: Style::default().fg(Color::DarkGray),
            error: Style::default().fg(Color::Red),
        }
    }
}
