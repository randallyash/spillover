//! Rendering: one pass over the frame, driven by a pure function of app state.
//!
//! The composition follows the work. This is an instrument people watch while a
//! model works, so the shape is a status rail across the top, the conversation
//! in the middle taking every column it can, and the prompt at the bottom where
//! the hands already are. The tier chain — the one thing spill does that nothing
//! else does — is the rail rather than a line of prose, because which tier is
//! answering right now is the most important thing on screen.

pub mod approval;
pub mod chat;
pub mod input;
pub mod markdown;
pub mod menu;
/// Frame inspection, for making the pictures in the README.
///
/// Test-only on purpose: it is a build tool rather than a feature, so there is
/// no reason for a released binary to carry it.
#[cfg(test)]
pub mod screenshot;
pub mod sidebar;
pub mod status;
pub mod theme;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::App;
use crate::text;
use crate::ui::theme::Theme;

/// Below this the interface cannot lay anything out legibly, so it says so
/// rather than drawing a garbled frame.
const MIN_WIDTH: u16 = 46;
const MIN_HEIGHT: u16 = 12;

/// The width at which a second column can be afforded for session state.
const SIDEBAR_MIN_WIDTH: u16 = 104;
const SIDEBAR_WIDTH: u16 = 32;

/// Columns between two key hints in the footer.
const HINT_GAP: usize = 3;

/// Braille spinner. Ten frames is a full cycle every nine hundred milliseconds,
/// which reads as motion without looking frantic.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Marks the end of the text currently being streamed.
pub const CARET: &str = "▋";

/// The bar down the left of a message the user wrote.
pub const BAR: &str = "▌";

/// The spinner frame for a given tick.
pub fn spinner(tick: u64) -> &'static str {
    SPINNER[(tick as usize) % SPINNER.len()]
}

/// How much room there is, decided once and passed down rather than
/// re-derived in every pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// A second column for session state.
    pub sidebar: bool,
}

impl Shape {
    pub fn from_width(width: u16) -> Self {
        Self {
            sidebar: width >= SIDEBAR_MIN_WIDTH,
        }
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let theme = Theme::detect();
    let area = frame.area();
    // Recorded so key handling can size things the way the renderer does — the
    // approval preview's scroll clamp is the reason.
    app.viewport = area;

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        too_small(frame, area, &theme);
        return;
    }

    let shape = Shape::from_width(area.width);
    let input_height = input::height(app, area.width);

    let rows = Layout::vertical([
        Constraint::Length(1), // the tier rail
        Constraint::Length(1), // the rule under it
        Constraint::Min(4),    // the conversation
        Constraint::Length(input_height),
        Constraint::Length(1), // key hints
    ])
    .areas::<5>(area);

    status::render(frame, rows[0], app, &theme);
    rule(frame, rows[1], &theme);
    body(frame, rows[2], app, &theme, shape);
    input::render(frame, rows[3], app, &theme);
    footer(frame, rows[4], app, &theme, shape);

    // The command menu belongs to the prompt, so it is drawn over it and only
    // while a command is being typed.
    if app.menu_open() {
        menu::render_menu(frame, area, app, &theme, rows[3]);
    }

    // Help is a deliberate detour, so it covers everything except the approval
    // question, which is the one thing that must never be hidden.
    if app.help {
        menu::render_help(frame, area, &theme);
    }

    // Drawn last so it sits above everything, including the prompt.
    if let Some(pending) = &app.approval {
        approval::render(frame, area, pending, &theme, app.approval_scroll);
    }
}

/// The conversation, and the session panel when there is room for one.
fn body(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme, shape: Shape) {
    if !shape.sidebar {
        chat::render(frame, area, app, theme);
        return;
    }

    let columns = Layout::horizontal([Constraint::Min(40), Constraint::Length(SIDEBAR_WIDTH)])
        .areas::<2>(area);
    chat::render(frame, columns[0], app, theme);
    sidebar::render(frame, columns[1], app, theme);
}

/// A hairline across the frame, to close the header band.
fn rule(frame: &mut Frame, area: Rect, theme: &Theme) {
    let line = "─".repeat(area.width as usize);
    frame.render_widget(Paragraph::new(Line::styled(line, theme.border)), area);
}

/// What the keys do right now, which is not the same in every state.
fn footer(frame: &mut Frame, area: Rect, app: &App, theme: &Theme, shape: Shape) {
    // On a narrow terminal there is no session panel, so the footer is where
    // the active tier has to be said out loud. The spinner lives in the rail,
    // next to the tier it belongs to, so it is not repeated here.
    let status = if shape.sidebar {
        String::new()
    } else {
        active_tier(app).to_string()
    };

    let status_width = text::display_width(&status) as u16;
    let [left, right] = Layout::horizontal([
        Constraint::Min(12),
        Constraint::Length(status_width.min(area.width.saturating_sub(12))),
    ])
    .areas::<2>(area);

    let hints: &[(&str, &str)] = if app.approval.is_some() {
        &[("y", "run"), ("n", "skip"), ("↑↓", "read")]
    } else if crate::commands::looks_like_command(&app.input) {
        // A command is not a turn, so the keys that matter change with it.
        &[("enter", "run"), ("tab", "complete"), ("esc", "cancel")]
    } else if app.busy {
        // The way out of a turn in flight, which used to be quitting outright.
        &[("esc", "stop"), ("", "working")]
    } else if app.mode.is_read_only() {
        // The way out of plan mode is the one thing worth saying while in it.
        &[("shift+tab", "build"), ("enter", "send"), ("?", "help")]
    } else {
        &[("enter", "send"), ("/", "commands"), ("shift+tab", "plan")]
    };

    // Hints are dropped from the end until they fit, rather than being clipped:
    // a clipped hint runs into the tier name on the right with no gap, which
    // reads as one word ("shiftDeepSeek V4 Flash") and is worse than a missing
    // hint. The first hint is always the one that matters most, so it goes first
    // and survives.
    let budget = left.width as usize;
    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0usize;
    for (key, label) in hints {
        let width = text::display_width(key)
            + if label.is_empty() {
                0
            } else {
                1 + text::display_width(label)
            };
        let gap = if spans.is_empty() { 0 } else { HINT_GAP };
        if used + gap + width > budget {
            break;
        }

        if gap > 0 {
            spans.push(Span::styled(" ".repeat(gap), theme.hint));
        }
        spans.push(Span::styled((*key).to_string(), theme.accent));
        if !label.is_empty() {
            spans.push(Span::styled(format!(" {label}"), theme.hint));
        }
        used += gap + width;
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), left);
    if status_width > 0 {
        frame.render_widget(Paragraph::new(Line::styled(status, theme.hint)), right);
    }
}

/// The tier now answering, trimmed to its name.
fn active_tier(app: &App) -> &str {
    app.active_tier_name().map(short_label).unwrap_or("no tier")
}

/// A tier label without its parenthetical detail: "Local (http://…)" becomes
/// "Local". The address belongs in the session panel, not in a one-line rail.
/// A path with the home directory folded to `~`.
///
/// The one place this is done, so a path reads the same in a notice, in the
/// session panel, and in the answer to `/why`. A full `/home/name/...` is long
/// enough to push a line over the width it is shown in, and the home directory
/// is the part that says the least.
pub fn short_path(path: &std::path::Path) -> String {
    let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) else {
        return path.display().to_string();
    };
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

pub fn short_label(label: &str) -> &str {
    match label.find(" (") {
        Some(cut) => &label[..cut],
        None => label,
    }
}

fn too_small(frame: &mut Frame, area: Rect, theme: &Theme) {
    let body = vec![
        Line::raw(""),
        Line::styled("spill needs a little more room", theme.title),
        Line::raw(""),
        Line::styled(
            format!("{} × {} cells, or larger", MIN_WIDTH, MIN_HEIGHT),
            theme.hint,
        ),
        Line::styled(
            format!("this terminal is {} × {}", area.width, area.height),
            theme.hint,
        ),
    ];
    // The message is the whole screen in this state, so it wraps rather than
    // being clipped off the right edge.
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: true }),
        centered(area, area.width, area.height),
    );
}

/// Shrink a box to fit inside a frame, centered.
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
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

    #[test]
    fn a_wide_terminal_gets_a_second_column() {
        assert!(Shape::from_width(120).sidebar);
    }

    #[test]
    fn a_narrow_terminal_keeps_the_conversation_and_drops_the_panel() {
        assert!(!Shape::from_width(60).sidebar);
        assert!(!Shape::from_width(90).sidebar, "90 is still one column");
    }

    #[test]
    fn the_sidebar_appears_exactly_at_its_threshold() {
        assert!(!Shape::from_width(SIDEBAR_MIN_WIDTH - 1).sidebar);
        assert!(Shape::from_width(SIDEBAR_MIN_WIDTH).sidebar);
    }

    #[test]
    fn the_spinner_cycles_and_wraps() {
        assert_eq!(spinner(0), SPINNER[0]);
        assert_eq!(spinner(9), SPINNER[9]);
        assert_eq!(spinner(10), SPINNER[0], "it wraps rather than overflowing");
        assert_eq!(spinner(u64::MAX), SPINNER[(u64::MAX % 10) as usize]);
    }

    #[test]
    fn a_tier_label_is_trimmed_to_its_name() {
        assert_eq!(short_label("Local (http://localhost:1234/v1)"), "Local");
        assert_eq!(short_label("Grok Build"), "Grok Build");
        // Nothing to trim is not an error.
        assert_eq!(short_label(""), "");
    }

    #[test]
    fn a_box_is_centered_and_never_leaves_the_frame() {
        let frame = Rect::new(0, 0, 100, 40);
        let boxed = centered(frame, 60, 10);
        assert_eq!(boxed.x, 20);
        assert_eq!(boxed.y, 15);
        assert_eq!((boxed.width, boxed.height), (60, 10));

        let tiny = centered(Rect::new(0, 0, 20, 6), 60, 10);
        assert_eq!((tiny.width, tiny.height), (20, 6));
    }

    #[test]
    fn every_moving_glyph_is_a_single_cell() {
        // A double-width frame would shift the whole line by one column each
        // time it advanced.
        for frame in SPINNER {
            assert_eq!(text::display_width(frame), 1, "{frame} is not one cell");
        }
        assert_eq!(text::display_width(CARET), 1);
        assert_eq!(text::display_width(BAR), 1);
    }

    /// A session worth looking at: a chain that has spilled once, a transcript
    /// with every kind of line in it, a tool still running, and enough turns
    /// behind it for the sparkline to have a shape.
    fn realistic() -> App {
        use crate::app::Message;

        let mut app = App::new(crate::config::Config::default());
        app.messages.clear();
        app.tier_labels = vec![
            "Local (http://192.168.1.50:1234/v1)".to_string(),
            "DeepSeek V4 Flash (Command Code)".to_string(),
        ];
        app.tier_failed = vec![true, false];
        app.active_tier = 1;
        app.messages.push(Message::system(
            "spill runs one prompt at a time through an ordered list of model tiers.",
        ));
        app.messages
            .push(Message::user("explain how the parser reads a frame"));
        app.messages.push(Message::assistant(
            "Each line is one JSON object, so the tag alone says what it is:\n\n\
             - `run_start` opens the turn\n\
             - `text` carries the answer, and `usage` the cost\n\n\
             ```rust\nfn classify(line: &str) -> Frame { serde_json::from_str(line) }\n```\n\n\
             A tag the parser does not know is passed over rather than failing the turn.",
        ));
        app.messages
            .push(Message::system("→ read_file  src/provider/dialect.rs"));
        app.messages
            .push(Message::system("✓ read_file  dialect.rs (503 lines)"));
        app.messages.push(Message::system(
            "✗ Local repeated the same output 4 times — spilling over to DeepSeek V4 Flash",
        ));
        app.messages
            .push(Message::system("→ grep  fn classify  src/provider"));
        app.running = Some(app.messages.len() - 1);

        for prompt_tokens in [15_360, 15_412, 15_980, 16_120, 15_360, 16_880] {
            app.record_usage(&crate::provider::Usage {
                prompt_tokens,
                completion_tokens: 24,
                cache_read_tokens: 7_424,
                cache_write_tokens: 0,
            });
        }
        app.turns = 6;
        app.input = "now make it stream tokens".to_string();
        app
    }

    /// Render one frame to text, the way a terminal would receive it.
    fn rendered(app: &mut App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("a terminal");
        terminal
            .draw(|frame| draw(frame, app))
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

    #[test]
    fn the_session_panel_appears_only_when_there_is_room_for_it() {
        let mut app = realistic();
        let wide = rendered(&mut app, 120, 30);
        assert!(
            wide.contains("session"),
            "a wide frame has the panel:\n{wide}"
        );
        assert!(wide.contains("tokens"), "with the usage in it:\n{wide}");

        let mut app = realistic();
        let narrow = rendered(&mut app, 70, 30);
        assert!(
            !narrow.contains("session"),
            "a narrow frame drops the panel:\n{narrow}"
        );
        // The tier that is answering is still said out loud, in the footer.
        assert!(narrow.contains("DeepSeek V4 Flash"), "{narrow}");
    }

    #[test]
    fn the_chain_is_on_screen_with_the_failed_tier_marked() {
        let mut app = realistic();
        let out = rendered(&mut app, 140, 30);

        assert!(out.contains("spill"), "the wordmark leads:\n{out}");
        assert!(out.contains('✗'), "the tier that failed is marked:\n{out}");
    }

    #[test]
    fn a_terminal_too_small_says_so_instead_of_drawing_something_garbled() {
        let mut app = realistic();
        let out = rendered(&mut app, 30, 8);

        assert!(out.contains("more room"), "{out}");
        assert!(out.contains("46 × 12"), "it names what it needs:\n{out}");
    }

    #[test]
    fn the_interface_draws_at_every_size_it_claims_to_support() {
        // A pane that draws outside itself, or a caret placed off the frame,
        // panics rather than failing quietly, so this is the check that the
        // layout arithmetic holds at the edges.
        for (width, height) in [
            (MIN_WIDTH, MIN_HEIGHT),
            (50, 14),
            (70, 20),
            (103, 24),
            (104, 30),
            (120, 40),
            (200, 60),
            (320, 80),
            // And below the floor, where the guard takes over.
            (20, 6),
            (45, 11),
        ] {
            let mut app = realistic();
            let out = rendered(&mut app, width, height);
            assert_eq!(
                out.lines().count(),
                height as usize,
                "the frame should be exactly {height} rows at {width}×{height}"
            );
        }
    }

    #[test]
    fn the_input_box_is_the_only_thing_that_says_prompt() {
        let mut app = realistic();
        let out = rendered(&mut app, 120, 30);
        assert!(out.contains("prompt"), "{out}");
        // The typed text is visible rather than only the placeholder.
        assert!(out.contains("now make it stream tokens"), "{out}");
    }

    #[test]
    fn the_prompt_is_marked_as_the_place_to_type() {
        let mut app = realistic();
        let out = rendered(&mut app, 120, 30);
        assert!(
            out.contains('❯'),
            "the prompt mark should be on screen: {out}"
        );
    }

    #[test]
    fn the_footer_never_runs_a_hint_into_the_tier_name() {
        // Hints longer than the space left for them used to be clipped by the
        // paragraph, so the last one ran straight into the tier on the right and
        // read as one word ("shiftDeepSeek V4 Flash").
        for width in [46u16, 48, 52, 60, 70, 80, 100] {
            let mut app = realistic();
            let out = rendered(&mut app, width, 20);
            let last = out.lines().last().expect("a footer");

            assert!(
                !last.contains("shiftDeepSeek"),
                "a hint ran into the tier name at {width}: {last:?}"
            );
            assert!(
                text::display_width(last) <= width as usize,
                "the footer overflowed at {width}: {last:?}"
            );
            // A hint that is shown must be whole, so nothing ends mid-word.
            assert!(
                !last.trim_end().ends_with("shif"),
                "a hint was cut in half at {width}: {last:?}"
            );
        }
    }

    #[test]
    fn the_way_out_of_plan_mode_is_on_screen_while_in_it() {
        let mut app = realistic();
        app.mode = crate::agent::Mode::Plan;
        let out = rendered(&mut app, 120, 30);

        assert!(out.contains("plan"), "the mode should be visible: {out}");
        assert!(
            out.contains("shift+tab") && out.contains("build"),
            "and so should the way back: {out}"
        );
    }

    #[test]
    fn a_turn_in_flight_spins_in_the_transcript() {
        let mut app = realistic();
        app.tick = 3;
        let out = rendered(&mut app, 120, 30);
        assert!(
            out.contains(spinner(3)),
            "the running tool should spin: {out}"
        );
    }

    /// Print a frame, to look at the interface without running it.
    ///
    /// Ignored by default because it asserts nothing — it is a viewing aid:
    ///
    /// ```text
    /// cargo test print_a_frame -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "prints a frame to look at; asserts nothing"]
    fn print_a_frame() {
        // (width, height), chosen to cover the responsive break points.
        for (width, height) in [(120, 28), (100, 24), (64, 16), (48, 12)] {
            let mut app = realistic();
            let out = rendered(&mut app, width, height);
            println!("\n===== {width} x {height} =====\n{out}");
        }

        // And the same session in plan mode, where the box has to say so.
        for (width, height) in [(120, 28), (64, 16)] {
            let mut app = realistic();
            app.mode = crate::agent::Mode::Plan;
            let out = rendered(&mut app, width, height);
            println!("\n===== {width} x {height} (plan mode) =====\n{out}");
        }
    }
}
