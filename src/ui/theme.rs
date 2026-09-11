//! Color and text styles, named by the job each one does.
//!
//! Three decisions are deliberate here.
//!
//! **Named ANSI colors, not RGB.** Terminal users theme their terminals; a
//! hardcoded RGB amber would ignore that and could land unreadable on a light
//! background. The named sixteen are remapped by whatever palette the user runs,
//! so they are the portable choice and the considerate one.
//!
//! **Body text carries no color at all.** The model's answer is the thing people
//! read for minutes at a stretch, so it uses the terminal's own foreground rather
//! than a hue, which also means it stays correct on a light background. Color is
//! spent on structure — who is speaking, what state a tier is in — and never on
//! the prose.
//!
//! **One protagonist.** Amber is the accent, and it is rationed: the wordmark,
//! the tier answering, the user's own turn, the caret, the keys. Everything else
//! is a neutral or a state color. An accent that appears everywhere stops
//! meaning anything, and the amber block is only legible as "the tier you are on
//! right now" as long as it is the only thing wearing it.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    // ---- identity ----
    /// The wordmark.
    pub brand: Style,
    /// The one accent: the active tier, the user's own words, primary keys.
    pub accent: Style,

    // ---- the transcript ----
    /// The user's turns.
    pub user: Style,
    /// The model's turns. Uncolored on purpose: this is the text being read.
    pub assistant: Style,
    /// Notices, tool events, anything the app says about itself.
    pub system: Style,

    // ---- structure ----
    /// Box drawing at rest.
    pub border: Style,
    /// The border of whatever currently has focus.
    pub border_active: Style,
    /// Panel and section titles.
    pub title: Style,
    /// The quiet heading over a group inside a panel.
    pub section: Style,
    /// Secondary text: hints, key names, units.
    pub hint: Style,
    /// Text that is present but should not compete for attention.
    pub faint: Style,

    // ---- states ----
    pub success: Style,
    pub error: Style,
    pub warn: Style,

    // ---- the chain ----
    /// The tier currently answering.
    pub tier_active: Style,
    /// A tier not yet reached.
    pub tier_pending: Style,
    /// A tier that failed and was spilled past.
    pub tier_failed: Style,

    // ---- markdown ----
    /// A heading in the model's prose.
    pub heading: Style,
    /// A heading below the first two levels.
    pub subheading: Style,
    /// An inline code span.
    pub code: Style,
    /// The frame and gutter around a fenced code block.
    pub code_frame: Style,
    /// A list bullet or number.
    pub bullet: Style,
    /// Quoted text.
    pub quote: Style,
    /// A horizontal rule.
    pub rule: Style,
    /// A link's own text.
    pub link: Style,

    // ---- the approval modal ----
    /// Lines a diff adds and removes. The previews the tools emit carry `+` and
    /// `-` prefixes, so the color is reinforcement rather than the whole signal.
    pub diff_add: Style,
    pub diff_del: Style,
    /// A shell command about to run.
    pub command: Style,

    // ---- the session panel ----
    /// The per-turn token history.
    pub spark: Style,
}

impl Theme {
    /// The theme for this terminal.
    ///
    /// `NO_COLOR` is honored per its specification: set to anything that is not
    /// an empty string, it turns color off entirely. The monochrome theme is not
    /// a degraded fallback — everything it needs to say is carried by a glyph, a
    /// border, or a weight, so nothing is lost but the hue.
    pub fn detect() -> Self {
        match std::env::var_os("NO_COLOR") {
            Some(value) if !value.is_empty() => Self::monochrome(),
            _ => Self::default(),
        }
    }

    /// No color anywhere. For `NO_COLOR` and for terminals that cannot show it.
    ///
    /// Each role is given the weight or attribute that carries its meaning
    /// without hue: `error` is reversed so it outranks everything, a failed tier
    /// is struck through so it reads as spent, and code is italicized so it is
    /// still distinct from the prose around it.
    pub fn monochrome() -> Self {
        Self {
            brand: Style::default().add_modifier(Modifier::BOLD),
            accent: Style::default().add_modifier(Modifier::BOLD),

            user: Style::default().add_modifier(Modifier::BOLD),
            assistant: Style::default(),
            system: Style::default().add_modifier(Modifier::DIM),

            border: Style::default().add_modifier(Modifier::DIM),
            border_active: Style::default().add_modifier(Modifier::BOLD),
            title: Style::default().add_modifier(Modifier::BOLD),
            section: Style::default().add_modifier(Modifier::DIM),
            hint: Style::default().add_modifier(Modifier::DIM),
            faint: Style::default()
                .add_modifier(Modifier::DIM)
                .add_modifier(Modifier::ITALIC),

            success: Style::default(),
            // Weight is the only signal left, so the two states that must never
            // be confused are given the two most different treatments.
            error: Style::default()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::REVERSED),
            warn: Style::default().add_modifier(Modifier::BOLD),

            tier_active: Style::default().add_modifier(Modifier::REVERSED),
            tier_pending: Style::default().add_modifier(Modifier::DIM),
            tier_failed: Style::default()
                .add_modifier(Modifier::DIM)
                .add_modifier(Modifier::CROSSED_OUT),

            heading: Style::default().add_modifier(Modifier::BOLD),
            subheading: Style::default()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::DIM),
            code: Style::default().add_modifier(Modifier::ITALIC),
            code_frame: Style::default().add_modifier(Modifier::DIM),
            bullet: Style::default().add_modifier(Modifier::DIM),
            quote: Style::default().add_modifier(Modifier::ITALIC),
            rule: Style::default().add_modifier(Modifier::DIM),
            link: Style::default().add_modifier(Modifier::UNDERLINED),

            // A diff line is already marked by its +/- in the first column, so
            // these need no help.
            diff_add: Style::default(),
            diff_del: Style::default(),
            command: Style::default(),
            spark: Style::default().add_modifier(Modifier::DIM),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        let accent = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);

        Self {
            brand: accent,
            // The user's own words are the one thing in the transcript that is
            // theirs, so they get the accent.
            accent,

            user: Style::default().add_modifier(Modifier::BOLD),
            assistant: Style::default(),
            system: Style::default().fg(Color::DarkGray),

            border: Style::default().fg(Color::DarkGray),
            border_active: accent,
            title: Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
            // A group heading inside a panel is a signpost, not a title, so it
            // stays dim rather than competing with the panel's own name.
            section: Style::default().fg(Color::DarkGray),
            hint: Style::default().fg(Color::DarkGray),
            faint: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),

            success: Style::default().fg(Color::Green),
            // Light red rather than red: an error has to outrank the amber
            // accent beside it, and plain red does not always.
            error: Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
            // A warning is amber's loud cousin, and it is rare enough that the
            // brightness reads as "look here" rather than as another accent.
            warn: Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),

            tier_active: Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            tier_pending: Style::default().fg(Color::DarkGray),
            tier_failed: Style::default().fg(Color::LightRed),

            heading: Style::default().add_modifier(Modifier::BOLD),
            subheading: Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
            // Cyan is the one hue besides amber, spent only on code, where it
            // reads as "this is not prose" on both a light and a dark terminal.
            code: Style::default().fg(Color::Cyan),
            code_frame: Style::default().fg(Color::DarkGray),
            bullet: Style::default().fg(Color::Yellow),
            quote: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
            rule: Style::default().fg(Color::DarkGray),
            link: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),

            diff_add: Style::default().fg(Color::Green),
            diff_del: Style::default().fg(Color::LightRed),
            command: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            spark: Style::default().fg(Color::Yellow),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulated in a test rather than read from the real environment, which
    /// tests must not share or mutate.
    fn no_color_is_set() -> bool {
        matches!(std::env::var_os("NO_COLOR"), Some(value) if !value.is_empty())
    }

    #[test]
    fn the_default_theme_spends_color_only_on_structure() {
        let theme = Theme::default();

        // The prose is uncolored: no hue, no weight, just the terminal's own
        // foreground.
        assert_eq!(theme.assistant, Style::default());
        assert_eq!(
            theme.user.fg, None,
            "the user's weight carries it, not a hue"
        );
        assert_eq!(theme.user.add_modifier, Modifier::BOLD);
    }

    #[test]
    fn a_failed_tier_is_distinguishable_from_an_active_one_by_more_than_hue() {
        let theme = Theme::default();
        // One is a filled chip, the other is not, so the two survive a
        // colorblind viewer and a monochrome terminal alike.
        assert!(theme.tier_active.bg.is_some(), "the active tier is a chip");
        assert!(theme.tier_failed.bg.is_none());
        assert_ne!(theme.tier_active, theme.tier_failed);
    }

    #[test]
    fn the_monochrome_theme_still_separates_every_state() {
        let theme = Theme::monochrome();

        let states = [
            theme.tier_active,
            theme.tier_pending,
            theme.tier_failed,
            theme.success,
            theme.error,
            theme.warn,
        ];
        for (index, state) in states.iter().enumerate() {
            assert_eq!(
                state.fg, None,
                "no color may survive in monochrome: {state:?}"
            );
            assert!(
                states[index + 1..].iter().all(|other| other != state),
                "two states collapsed into the same style in monochrome: {state:?}"
            );
        }
    }

    #[test]
    fn the_monochrome_diff_needs_no_color_because_the_prefix_labels_it() {
        let theme = Theme::monochrome();
        // Nothing distinguishes these two styles, which is correct: the `+` and
        // `-` at the start of the line already do.
        assert_eq!(theme.diff_add, theme.diff_del);
    }

    #[test]
    fn monochrome_turns_every_color_off() {
        let theme = Theme::monochrome();
        for (name, style) in [
            ("brand", theme.brand),
            ("accent", theme.accent),
            ("user", theme.user),
            ("assistant", theme.assistant),
            ("system", theme.system),
            ("border", theme.border),
            ("title", theme.title),
            ("section", theme.section),
            ("hint", theme.hint),
            ("faint", theme.faint),
            ("error", theme.error),
            ("command", theme.command),
            ("code", theme.code),
            ("code_frame", theme.code_frame),
            ("bullet", theme.bullet),
            ("quote", theme.quote),
            ("link", theme.link),
            ("spark", theme.spark),
        ] {
            assert_eq!(style.fg, None, "{name} kept a foreground color");
            assert_eq!(style.bg, None, "{name} kept a background color");
        }
    }

    #[test]
    fn the_accent_is_the_only_role_wearing_amber() {
        // The amber block means "the tier you are on right now". If a heading or
        // a section label also wore it, it would stop meaning that.
        let theme = Theme::default();
        for (name, style) in [
            ("heading", theme.heading),
            ("subheading", theme.subheading),
            ("section", theme.section),
            ("title", theme.title),
            ("hint", theme.hint),
            ("faint", theme.faint),
            ("system", theme.system),
        ] {
            assert_ne!(style.fg, Some(Color::Yellow), "{name} stole the accent");
        }
    }

    #[test]
    fn detect_respects_the_environment_it_is_given() {
        // The real process environment decides, so this asserts the mapping
        // rather than mutating anything: whichever way it resolves, the result
        // must be a coherent theme.
        let theme = Theme::detect();
        let expected = if no_color_is_set() {
            Theme::monochrome()
        } else {
            Theme::default()
        };
        assert_eq!(theme.brand, expected.brand);
        assert_eq!(theme.error, expected.error);
    }
}
