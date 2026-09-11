//! Rendering a frame to JSON, so the interface can be turned into an image.
//!
//! Only used to make the pictures in the README. It is a test rather than a
//! feature because it must never grow a way to reach the network or the user's
//! files: it walks the frame `draw` already produced and writes down what each
//! cell holds.
//!
//! The colours recorded are the theme's own — named ANSI colours, not RGB — so
//! the renderer that consumes this picks the palette, exactly as a terminal
//! would. That is why the images can be a dark theme without the program having
//! an opinion about it.

use std::fmt::Write as _;

use ratatui::style::{Color, Modifier};

use crate::app::App;
use crate::ui::theme::Theme;

/// Render a frame and describe it as JSON.
///
/// Shape: `{"width":w,"height":h,"rows":[[cell,...],...]}` where a cell is
/// `{"s":"text","fg":"red","bg":null,"b":true,...}`. Absent attributes are
/// false, and a null colour means "the terminal's own default".
pub fn frame_json(app: &mut App, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("a terminal");
    terminal
        .draw(|frame| crate::ui::draw(frame, app))
        .expect("the frame should draw");

    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    let _ = write!(out, "{{\"width\":{width},\"height\":{height},\"rows\":[");

    for y in 0..buffer.area.height {
        if y > 0 {
            out.push(',');
        }
        out.push('[');
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            if x > 0 {
                out.push(',');
            }
            describe(&mut out, cell.symbol(), cell.fg, cell.bg, cell.modifier);
        }
        out.push(']');
    }

    out.push_str("]}");
    out
}

fn describe(out: &mut String, symbol: &str, fg: Color, bg: Color, modifier: Modifier) {
    let _ = write!(out, "{{\"s\":");
    write_json_string(out, symbol);
    let _ = write!(out, ",\"fg\":");
    write_colour(out, fg);
    let _ = write!(out, ",\"bg\":");
    write_colour(out, bg);

    // Only the attributes the theme actually uses are recorded, so the renderer
    // has less to get wrong. Each is omitted when false.
    for (name, flag) in [
        ("b", Modifier::BOLD),
        ("d", Modifier::DIM),
        ("i", Modifier::ITALIC),
        ("u", Modifier::UNDERLINED),
        ("r", Modifier::REVERSED),
        ("x", Modifier::CROSSED_OUT),
    ] {
        if modifier.contains(flag) {
            let _ = write!(out, ",\"{name}\":true");
        }
    }
    out.push('}');
}

fn write_colour(out: &mut String, colour: Color) {
    match colour {
        Color::Reset => out.push_str("null"),
        Color::Black => out.push_str("\"black\""),
        Color::Red => out.push_str("\"red\""),
        Color::Green => out.push_str("\"green\""),
        Color::Yellow => out.push_str("\"yellow\""),
        Color::Blue => out.push_str("\"blue\""),
        Color::Magenta => out.push_str("\"magenta\""),
        Color::Cyan => out.push_str("\"cyan\""),
        Color::Gray => out.push_str("\"gray\""),
        Color::DarkGray => out.push_str("\"darkgray\""),
        Color::LightRed => out.push_str("\"lightred\""),
        Color::LightGreen => out.push_str("\"lightgreen\""),
        Color::LightYellow => out.push_str("\"lightyellow\""),
        Color::LightBlue => out.push_str("\"lightblue\""),
        Color::LightMagenta => out.push_str("\"lightmagenta\""),
        Color::LightCyan => out.push_str("\"lightcyan\""),
        Color::White => out.push_str("\"white\""),
        // Anything else is written as its own RGB, so a future theme that
        // hardcodes a colour still renders rather than silently going default.
        Color::Rgb(r, g, b) => {
            let _ = write!(out, "\"#{r:02x}{g:02x}{b:02x}\"");
        }
        Color::Indexed(index) => {
            let _ = write!(out, "\"idx{index}\"");
        }
    }
}

fn write_json_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

/// Whether the theme in use has colour at all.
///
/// The images are always made with colour; this exists so the test can say so
/// rather than silently depending on the environment it runs in.
pub fn colour_is_on() -> bool {
    let _ = Theme::detect();
    std::env::var_os("NO_COLOR").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Message, Role};
    use crate::provider::Usage;
    use tokio::sync::oneshot;

    /// A local tier that looped, and a hosted one that answered.
    ///
    /// This is the picture the whole project exists to be, so it is built the
    /// way the real thing gets there: the failing tier is marked, the reason is
    /// in the transcript, and the tier that answered is the one wearing the
    /// filled chip.
    fn spilled() -> App {
        let mut app = App::new(crate::config::Config::default());
        app.messages.clear();
        app.tier_labels = vec![
            "Local (LM Studio)".to_string(),
            "DeepSeek V4 Flash".to_string(),
        ];
        app.tier_failed = vec![true, false];
        app.active_tier = 1;

        app.messages
            .push(Message::user("why does the parser drop blank lines?"));
        app.messages
            .push(Message::system("→ read_file  src/provider/dialect.rs"));
        app.messages
            .push(Message::system("✓ read_file  dialect.rs (503 lines)"));
        app.messages.push(Message::system(
            "✗ Local repeated the same output 4 times — spilling over to DeepSeek V4 Flash",
        ));
        app.messages.push(Message::assistant(
            "It does not drop them — it never sees them.\n\n\
             The parser reads the stream line by line and `run_start` is the only \
             frame that carries the turn's opening state. A blank line is not a \
             frame, so `classify` returns `None` and the loop skips it.\n\n\
             The fix is in `classify`: treat an empty payload as a keep-alive \
             rather than an unknown frame.",
        ));
        app.messages
            .push(Message::system("tokens: 15,412 in, 284 out · 7,168 cached"));

        app.record_usage_on(
            Some("Local (LM Studio)"),
            &Usage {
                prompt_tokens: 2_048,
                completion_tokens: 96,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        );
        for prompt_tokens in [15_300, 15_412, 16_010, 15_880, 15_412, 16_240] {
            app.record_usage_on(
                Some("DeepSeek V4 Flash"),
                &Usage {
                    prompt_tokens,
                    completion_tokens: 240,
                    cache_read_tokens: 7_168,
                    cache_write_tokens: 0,
                },
            );
        }
        app.turns = 6;
        app.tick = 3;
        app
    }

    /// The same session, with the command menu open.
    fn typing_a_command() -> App {
        let mut app = spilled();
        app.input = "/".to_string();
        app
    }

    /// The same session in plan mode, where the box says so.
    fn planning() -> App {
        let mut app = spilled();
        app.mode = crate::agent::Mode::Plan;
        app.messages.clear();
        app.messages
            .push(Message::user("how should the parser handle a blank line?"));
        app.messages.push(Message::assistant(
            "## What I would change\n\n\
             `classify` in `src/provider/dialect.rs` is where a blank payload \
             becomes `None`. Three options:\n\n\
             1. **Keep-alive** — return a no-op and let the loop continue. \
                Smallest change, matches how the SSE spec treats an empty event.\n\
             2. **Protocol error** — refuse the frame and fail the turn. Honest, \
                but one stray newline would end a conversation.\n\
             3. **Ignore at the reader** — filter before `classify` sees it. \
                Pushes the decision into the transport, where the blank line came from.\n\n\
             I would take the first. It is one arm of the match, and it cannot \
             turn a working stream into a failed turn.",
        ));
        app.input.clear();
        app.mode = crate::agent::Mode::Plan;
        app
    }

    /// The approval prompt, scrolled into the middle of a real diff.
    fn approving() -> App {
        let mut app = spilled();
        let (reply, _answer) = oneshot::channel();
        let mut preview = String::from("edit src/provider/dialect.rs — 3 occurrences\n");
        for n in 0..28 {
            preview.push_str(&format!(
                "- if payload.is_empty() {{ return Vec::new(); }} // {n}\n"
            ));
            preview.push_str(&format!(
                "+ if payload.is_empty() {{ return keepalive(); }} // {n}\n"
            ));
        }
        app.set_approval(crate::agent::approval::ApprovalRequest {
            tool: "edit_file".to_string(),
            preview,
            reply,
        });
        app.approval_scroll = 8;
        app
    }

    /// Write every picture's JSON to `dir`.
    ///
    /// Ignored so it does not run with the suite; it is a build step for the
    /// README, not a test of anything.
    ///
    /// ```text
    /// SPILL_SCREENSHOTS=/tmp/shots cargo test dump_screens -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "writes the README pictures; asserts nothing"]
    fn dump_screens() {
        let dir = match std::env::var("SPILL_SCREENSHOTS") {
            Ok(dir) => dir,
            Err(_) => {
                println!("set SPILL_SCREENSHOTS to a directory to write into");
                return;
            }
        };
        std::fs::create_dir_all(&dir).expect("the output directory should be creatable");

        assert!(colour_is_on(), "these pictures are made with colour on");

        // Named, because the tuple says nothing about which field is which.
        type Shot = (&'static str, u16, u16, fn() -> App);

        let shots: Vec<Shot> = vec![
            ("hero", 118, 30, spilled),
            ("commands", 118, 30, typing_a_command),
            ("plan", 118, 28, planning),
            ("approval", 104, 26, approving),
        ];

        for (name, width, height, build) in shots {
            let mut app = build();
            let json = frame_json(&mut app, width, height);
            let path = std::path::Path::new(&dir).join(format!("{name}.json"));
            std::fs::write(&path, json).expect("the frame should be writable");
            println!("wrote {}", path.display());
        }
    }

    /// The frame's text, read back out of its JSON.
    ///
    /// Parsing it rather than searching the raw string does two things: it
    /// proves the JSON is well formed, and it reconstructs the screen, which a
    /// substring search cannot do because every cell is quoted separately.
    fn text_of(json: &str) -> String {
        let value: serde_json::Value =
            serde_json::from_str(json).expect("the frame should be valid JSON");
        let rows = value["rows"].as_array().expect("rows");

        let mut out = String::new();
        for row in rows {
            for cell in row.as_array().expect("a row of cells") {
                out.push_str(cell["s"].as_str().expect("a symbol"));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn a_frame_is_described_as_json_with_its_styling() {
        let mut app = App::new(crate::config::Config::default());
        app.messages.clear();
        app.messages.push(Message {
            role: Role::User,
            text: "hi".to_string(),
        });

        // Above the interface's minimum size, or the size guard renders instead
        // and there is no transcript to describe.
        let (width, height) = (80u16, 16u16);
        let json = frame_json(&mut app, width, height);
        let header = format!("{{\"width\":{width},\"height\":{height},\"rows\":[");
        assert!(json.starts_with(&header), "{json}");
        assert!(json.ends_with("]}"), "{json}");

        // The user's own words are on screen, with the accent bar beside them.
        let text = text_of(&json);
        assert!(text.contains("▌ hi"), "{text}");

        // And the styling came through as a colour rather than being dropped.
        assert!(json.contains("\"fg\":\"yellow\""), "{json}");

        // One entry per cell, so nothing was skipped or duplicated.
        assert_eq!(
            json.matches("\"s\":").count(),
            (width * height) as usize,
            "one entry per cell"
        );
    }

    #[test]
    fn a_frame_below_the_minimum_size_is_described_too() {
        // The size guard is part of what `draw` does, so the dumper has to
        // survive it rather than assume a transcript is there.
        let mut app = App::new(crate::config::Config::default());
        let json = frame_json(&mut app, 20, 3);

        // Squashed, because at this width the message wraps mid-phrase and the
        // row break would sit in the middle of the substring.
        let text = text_of(&json)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("needs a little more room"), "{text}");
        assert_eq!(json.matches("\"s\":").count(), 20 * 3, "{json}");
    }

    #[test]
    fn colours_are_named_or_explicit_never_invented() {
        let mut out = String::new();
        write_colour(&mut out, Color::DarkGray);
        write_colour(&mut out, Color::Reset);
        write_colour(&mut out, Color::Rgb(1, 2, 3));
        write_colour(&mut out, Color::Indexed(9));
        assert_eq!(out, "\"darkgray\"null\"#010203\"\"idx9\"");
    }

    #[test]
    fn text_is_escaped_so_the_json_stays_valid() {
        let mut out = String::new();
        write_json_string(&mut out, "a\"b\\c\nd\te\u{1}f");
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\\te\\u0001f\"");
    }
}
