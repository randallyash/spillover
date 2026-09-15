//! The header rail: the tier chain, in order, and which tier is answering.
//!
//! This is the one thing spill does that nothing else does, so it gets the most
//! valuable strip of the screen. The rail degrades in three steps rather than
//! being clipped: full labels, then names without their addresses, and finally
//! just the tier answering and its position in the chain.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::text;
use crate::ui::theme::Theme;
use crate::ui::{short_label, spinner};

/// Below this the rail keeps every column for the chain, and the state word
/// would cost more than it says.
const BADGE_MIN_WIDTH: usize = 56;

/// The cells a rate needs, held back on any rail wide enough for the badge.
const RATE_RESERVE: usize = " ~0000 tok/s".len();

pub fn render(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let line = rail(app, theme, area.width);
    frame.render_widget(Paragraph::new(line), area);
}

/// The rail, in whichever form fits the width.
pub fn rail(app: &App, theme: &Theme, width: u16) -> Line<'static> {
    let budget = width as usize;
    let badge = badge(app, theme);
    let badge_width: usize = badge.iter().map(Span::width).sum();
    // Only reserve room for the badge on a terminal that can afford both it and
    // the chain; otherwise the chain would shorten to make space for a word. The
    // rate is reserved along with it, and always, whether or not one is showing.
    let reserve = if budget >= BADGE_MIN_WIDTH {
        badge_width + 2 + RATE_RESERVE
    } else {
        0
    };

    let mut head = vec![Span::styled("spill", theme.brand)];

    if app.tier_labels.is_empty() {
        head.push(Span::styled("  no tiers configured", theme.faint));
        return Line::from(clamp(head, budget));
    }

    let mut placed = false;
    for abbreviate in [false, true] {
        let tiers = tiers(app, theme, abbreviate);
        let tiers_width: usize = tiers.iter().map(Span::width).sum();
        let head_width: usize = head.iter().map(Span::width).sum();
        if head_width + 2 + tiers_width + reserve <= budget {
            head.push(Span::styled("  ", theme.faint));
            head.extend(tiers);
            placed = true;
            break;
        }
    }

    if !placed {
        // Not even shortened names fit alongside the badge. Say the one thing
        // that matters instead: which tier is answering, and how far down.
        let active = active_only(app, theme);
        let head_width: usize = head.iter().map(Span::width).sum();
        let active_width: usize = active.iter().map(Span::width).sum();
        let budget_for_active = if head_width + 2 + active_width + reserve <= budget {
            budget - reserve
        } else {
            budget
        };
        head.push(Span::styled("  ", theme.faint));
        head.extend(active);
        head = clamp(head, budget_for_active);
    } else {
        head = clamp(head, budget.saturating_sub(reserve));
    }

    // The state word is right-aligned, which gives the header a right edge to
    // balance the wordmark on the left instead of trailing off mid-line.
    let used: usize = head.iter().map(Span::width).sum();
    if badge_width > 0 && used + badge_width + 2 <= budget {
        // The rate was originally left to ride on whatever padding the chain left
        // over, on the reasoning that the chain and the state word both say more
        // than an estimate. That was wrong, and the reason is worth keeping: the
        // chain re-measures itself against the width and steps up to a longer
        // form the moment one fits, so at some widths the leftover shrank below
        // the number and the rate disappeared — at *exactly* the widths where the
        // chain grew, and back again when it grew enough to have room to spare.
        // A figure that comes and goes with the size of the terminal reads as a
        // figure that does not exist, which is the one thing this must not do.
        //
        // So it is given a slot of its own in the reserve above. `slack` is now
        // positive by construction wherever the badge is drawn at all, and this
        // check is the belt to that pair of braces.
        let mut badge = badge;
        let mut badge_width = badge_width;
        if let Some(spans) = rate_spans(app, theme, budget - used - badge_width - 2) {
            badge_width += spans.iter().map(Span::width).sum::<usize>();
            badge.extend(spans);
        }

        head.push(Span::styled(
            " ".repeat(budget - used - badge_width),
            theme.faint,
        ));
        head.extend(badge);
    }

    Line::from(head)
}

/// The streaming rate, when there is one and the padding can hold it.
fn rate_spans(app: &App, theme: &Theme, slack: usize) -> Option<Vec<Span<'static>>> {
    let value = app.stream_rate()?;
    let span = Span::styled(format!(" {}", rate(value)), theme.faint);

    (span.width() <= slack).then(|| vec![span])
}

/// A rate, to the precision it has earned: whole tokens once there are ten of
/// them, and a tenth below that, where the difference between 2 and 2.5 is the
/// difference between usable and not.
fn rate(value: f64) -> String {
    if value >= 10.0 {
        format!("~{value:.0} tok/s")
    } else {
        format!("~{value:.1} tok/s")
    }
}

/// What the app is doing, at the far right of the rail. The spinner lives here
/// rather than beside the wordmark so the top line moves in one place, next to the
/// word it belongs to — and the streaming rate hangs off it for the same reason,
/// rather than sitting somewhere of its own.
fn badge(app: &App, theme: &Theme) -> Vec<Span<'static>> {
    if app.active_tier_name().is_none() {
        return Vec::new();
    }
    if app.busy {
        vec![
            Span::styled(spinner(app.tick).to_string(), theme.accent),
            Span::styled(" working", theme.faint),
        ]
    } else {
        vec![Span::styled("ready", theme.faint)]
    }
}

/// Cut spans down to a width, so the rail can never draw over its neighbour.
fn clamp(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    for span in spans {
        let span_width = span.width();
        if used + span_width <= width {
            used += span_width;
            out.push(span);
            continue;
        }

        // Part of this span fits. Keep whole characters, so a wide glyph is
        // never cut in half and left as a stray cell.
        let mut kept = String::new();
        let mut kept_width = 0usize;
        for ch in span.content.chars() {
            let ch_width = text::display_width(&ch.to_string());
            if used + kept_width + ch_width > width {
                break;
            }
            kept.push(ch);
            kept_width += ch_width;
        }
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
        break;
    }

    out
}

fn tiers(app: &App, theme: &Theme, abbreviate: bool) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    // The tier being abandoned, for the length of the beat that announces it. It
    // is the single most important event in a session, and a static mark in a
    // wall of grey is easy to miss — but it belongs *before* the move, as the
    // reason for it, rather than flashing after the fact.
    let abandoning = app.abandoning_tier();

    for (index, label) in app.tier_labels.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" → ", theme.faint));
        }

        let failed = app.tier_failed.get(index).copied().unwrap_or(false);
        let active = index == app.active_tier;
        let going = abandoning == Some(index);
        let name = if abbreviate {
            short_label(label)
        } else {
            label
        };
        let numbered = format!("{}. {name}", index + 1);

        // A failed tier is marked by a glyph as well as a color, so the rail
        // survives a monochrome terminal and a colorblind reader alike. The tier
        // on its way out carries the same glyph, because that is what it is about
        // to be — drawn in the warning color, reversed, so it reads as happening
        // now rather than as having happened.
        if going || failed {
            let style = if going {
                theme.warn.add_modifier(Modifier::REVERSED)
            } else {
                theme.tier_failed
            };
            spans.push(Span::styled("✗ ", style));
        }

        let style = if going {
            theme.warn.add_modifier(Modifier::REVERSED)
        } else if active {
            theme.tier_active
        } else if failed {
            theme.tier_failed
        } else {
            theme.tier_pending
        };

        // The answering tier is drawn as a filled block. Padding it with spaces
        // would fight the separator for the same gap, so the background hugs
        // the text: the block itself is the mark, not the whitespace around it.
        spans.push(Span::styled(numbered, style));
    }

    spans
}

fn active_only(app: &App, theme: &Theme) -> Vec<Span<'static>> {
    let name = app.active_tier_name().map(short_label).unwrap_or("no tier");
    let position = format!(
        " {}/{total}",
        app.active_tier + 1,
        total = app.tier_labels.len()
    );

    vec![
        Span::styled(format!(" {name} "), theme.tier_active),
        Span::styled(position, theme.hint),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{HANDOFF_TICKS, Handoff};
    use crate::config::Config;

    fn app_with(labels: &[&str]) -> App {
        let mut app = App::new(Config::default());
        app.tier_labels = labels.iter().map(|label| label.to_string()).collect();
        app.tier_failed = vec![false; labels.len()];
        app
    }

    fn text_of(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn the_rail_shows_the_chain_in_order() {
        let app = app_with(&["Local", "DeepSeek", "Grok"]);
        let line = rail(&app, &Theme::default(), 200);
        let text = text_of(&line);

        assert!(text.starts_with("spill"), "{text}");
        assert!(text.contains("1. Local"), "{text}");
        assert!(text.contains("2. DeepSeek"), "{text}");
        assert!(text.contains("3. Grok"), "{text}");
        // In order, which is what makes it a chain.
        let positions: Vec<usize> = ["1. Local", "2. DeepSeek", "3. Grok"]
            .iter()
            .map(|part| text.find(part).expect("every tier is listed"))
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
    }

    #[test]
    fn long_labels_are_abbreviated_rather_than_clipping_the_chain() {
        let app = app_with(&[
            "Local (http://192.168.1.50:1234/v1)",
            "DeepSeek (Command Code)",
        ]);
        // Wide enough only for the shortened pair.
        let line = rail(&app, &Theme::default(), 52);
        let text = text_of(&line);

        assert!(
            !text.contains("192.168.1.50"),
            "the address should go: {text}"
        );
        assert!(text.contains("1. Local"), "{text}");
        assert!(text.contains("2. DeepSeek"), "{text}");
    }

    #[test]
    fn a_narrow_rail_still_says_which_tier_is_answering() {
        let app = app_with(&[
            "Local (http://192.168.1.50:1234/v1)",
            "DeepSeek (Command Code)",
        ]);
        let line = rail(&app, &Theme::default(), 20);
        let text = text_of(&line);

        assert!(text.contains("Local"), "{text}");
        assert!(text.contains("1/2"), "it should say how far down: {text}");
    }

    #[test]
    fn the_rail_never_draws_wider_than_it_was_given() {
        let app = app_with(&["Local (http://192.168.1.50:1234/v1)", "Grok (SuperGrok)"]);
        for width in [16u16, 24, 30, 40, 60, 80, 120, 200] {
            let line = rail(&app, &Theme::default(), width);
            assert!(
                line.width() <= width as usize,
                "rail overflowed {width} cells at {}: {:?}",
                line.width(),
                text_of(&line)
            );
        }
    }

    #[test]
    fn the_active_tier_is_marked_by_more_than_color() {
        let app = app_with(&["Local", "Grok"]);
        let line = rail(&app, &Theme::default(), 200);

        let active = line
            .spans
            .iter()
            .find(|span| span.content.contains("1. Local"))
            .expect("the active chip");
        // A filled block, which is a shape rather than a hue, so it reads
        // without color.
        assert!(
            active.style.bg.is_some(),
            "the active tier should be a filled block"
        );
        assert_ne!(
            active.style,
            Theme::default().tier_pending,
            "and not the style of a tier yet to be reached"
        );
    }

    #[test]
    fn the_separator_is_a_single_space_on_each_side_of_the_arrow() {
        let app = app_with(&["Local", "DeepSeek"]);
        let text = text_of(&rail(&app, &Theme::default(), 200));

        assert!(text.contains("1. Local → 2. DeepSeek"), "{text}");
        assert!(!text.contains("  →"), "no gap before the arrow: {text}");
        assert!(!text.contains("→  "), "no gap after the arrow: {text}");
    }

    #[test]
    fn a_tier_that_was_spilled_past_is_marked() {
        let mut app = app_with(&["Local", "Grok"]);
        app.fail_tier("Local");
        app.activate_tier("Grok");

        let line = rail(&app, &Theme::default(), 200);
        let text = text_of(&line);

        assert!(
            text.contains("✗ 1. Local"),
            "the failed tier needs a mark: {text}"
        );
        assert_eq!(app.active_tier, 1);
    }

    #[test]
    fn the_tier_being_abandoned_flashes_before_it_settles() {
        // The beat: while a handoff is being narrated the tier is drawn as
        // *going*, in the warning color and reversed — before it is spent. Once
        // the beat expires the same tier settles into the spent style.
        let mut app = app_with(&["Local", "DeepSeek"]);
        app.handoff = Some(Handoff {
            from: Some(0),
            until: HANDOFF_TICKS,
        });

        let during = rail(&app, &Theme::default(), 200);
        let chip = during
            .spans
            .iter()
            .find(|span| span.content.contains("1. Local"))
            .expect("the abandoning tier");

        assert!(
            chip.style.add_modifier.contains(Modifier::REVERSED),
            "it should be flashing: {:?}",
            chip.style
        );
        assert!(
            text_of(&during).contains("✗ 1. Local"),
            "and carry the mark it is about to earn: {}",
            text_of(&during)
        );

        // The beat over, and the move recorded.
        app.tick = HANDOFF_TICKS;
        app.fail_tier("Local");
        app.activate_tier("DeepSeek");
        let after = rail(&app, &Theme::default(), 200);
        let settled = after
            .spans
            .iter()
            .find(|span| span.content.contains("1. Local"))
            .expect("the spent tier");

        assert!(
            !settled.style.add_modifier.contains(Modifier::REVERSED),
            "the flash must not outstay its welcome: {:?}",
            settled.style
        );
    }

    #[test]
    fn no_tiers_configured_says_so() {
        let app = App::new(Config::default());
        let line = rail(&app, &Theme::default(), 200);
        assert!(text_of(&line).contains("no tiers configured"));
    }

    #[test]
    fn the_state_word_is_pushed_to_the_right_edge() {
        let app = app_with(&["Local"]);
        let text = text_of(&rail(&app, &Theme::default(), 200));
        assert!(text.ends_with("ready"), "{text:?}");
    }

    #[test]
    fn a_narrow_rail_keeps_the_chain_and_drops_the_state_word() {
        let app = app_with(&["Local (http://10.0.0.1:1234/v1)", "Grok"]);
        let text = text_of(&rail(&app, &Theme::default(), 30));
        assert!(!text.contains("ready"), "{text:?}");
        assert!(text.contains("Local"), "{text:?}");
    }

    #[test]
    fn a_busy_app_spins_in_the_rail() {
        let mut app = app_with(&["Local"]);
        let quiet = text_of(&rail(&app, &Theme::default(), 200));

        app.busy = true;
        app.tick = 3;
        let working = text_of(&rail(&app, &Theme::default(), 200));

        assert!(!quiet.contains(spinner(3)), "{quiet}");
        assert!(working.contains(spinner(3)), "{working}");
    }

    #[test]
    fn clamping_trims_a_line_that_is_too_wide() {
        let spans = vec![Span::raw("abcdefghij")];
        assert_eq!(text_of(&Line::from(clamp(spans, 4))), "abcd");
    }

    #[test]
    fn clamping_leaves_a_line_that_already_fits_exactly_alone() {
        let exactly = || vec![Span::raw("abc")];
        assert_eq!(text_of(&Line::from(clamp(exactly(), 3))), "abc");
        assert_eq!(text_of(&Line::from(clamp(exactly(), 40))), "abc");
    }

    #[test]
    fn clamping_keeps_whole_characters() {
        // Each of these is two cells wide, so only two fit in five columns.
        let spans = vec![Span::raw("日本語")];
        let clamped = text_of(&Line::from(clamp(spans, 5)));

        assert_eq!(clamped, "日本");
        assert_eq!(text::display_width(&clamped), 4, "never half a glyph");
    }

    #[test]
    fn clamping_across_spans_stops_where_the_room_runs_out() {
        let spans = vec![Span::raw("ab"), Span::raw("cd"), Span::raw("ef")];
        assert_eq!(text_of(&Line::from(clamp(spans, 3))), "abc");
    }

    #[test]
    fn clamping_to_nothing_draws_nothing() {
        let spans = vec![Span::raw("abc")];
        assert!(clamp(spans, 0).is_empty());
    }

    /// A busy app mid-reply: `chars` characters in over `ticks` redraws.
    fn streaming_app(labels: &[&str], chars: usize, ticks: u64) -> App {
        let mut app = app_with(labels);
        app.busy = true;
        app.tick = 100;
        app.pretend_to_stream(chars, ticks);
        app
    }

    #[test]
    fn the_rail_shows_the_rate_while_the_model_writes() {
        // 360 characters is 90 tokens at the assumed ratio, over the 3.6s in which
        // they arrived.
        let app = streaming_app(&["Local"], 360, 40);
        let text = text_of(&rail(&app, &Theme::default(), 200));

        assert!(text.contains("working"), "{text}");
        assert!(text.contains("~25 tok/s"), "{text}");
        // Marked as an estimate, because it is one: no tier reported this.
        assert!(text.contains('~'), "{text}");
    }

    #[test]
    fn a_rail_with_nothing_arriving_shows_no_rate() {
        let mut app = app_with(&["Local"]);
        app.busy = true;
        app.tick = 100;
        let text = text_of(&rail(&app, &Theme::default(), 200));

        assert!(text.contains("working"), "{text}");
        assert!(!text.contains("tok/s"), "{text}");
        assert!(!text.contains('~'), "{text}");
    }

    #[test]
    fn the_rate_gives_way_before_the_state_word_does() {
        // Room for the chain and the word, but not the ~10 cells a rate needs on
        // top. The word stays and the number goes: it says more.
        let app = streaming_app(&["Local"], 360, 40);
        assert!(app.stream_rate().is_some(), "the rate exists to be dropped");

        let text = text_of(&rail(&app, &Theme::default(), 30));

        assert!(text.contains("working"), "{text}");
        assert!(!text.contains("tok/s"), "{text}");
    }

    #[test]
    fn a_rail_with_a_rate_never_draws_wider_than_it_was_given() {
        // The rate is padded into place with arithmetic rather than measured into
        // it, so this is where it would overflow if the sum were wrong.
        let app = streaming_app(&["Local (http://192.168.1.50:1234/v1)", "Grok"], 400, 20);
        for width in [16u16, 24, 30, 40, 56, 60, 80, 120, 200] {
            let line = rail(&app, &Theme::default(), width);
            assert!(
                line.width() <= width as usize,
                "rail overflowed {width} cells at {}: {:?}",
                line.width(),
                text_of(&line)
            );
        }
    }

    #[test]
    fn the_rate_keeps_the_state_word_on_the_right_edge() {
        let app = streaming_app(&["Local"], 360, 40);
        let text = text_of(&rail(&app, &Theme::default(), 200));

        assert!(text.ends_with("~25 tok/s"), "{text:?}");
    }

    #[test]
    fn a_rate_reads_whole_tokens_once_there_are_ten_of_them() {
        assert_eq!(rate(38.4), "~38 tok/s");
        assert_eq!(rate(10.0), "~10 tok/s");
    }

    #[test]
    fn a_rate_below_ten_keeps_a_decimal() {
        // Where the difference between 2 and 2.5 is the difference between usable
        // and not, the whole number is not enough.
        assert_eq!(rate(2.54), "~2.5 tok/s");
        assert_eq!(rate(9.9), "~9.9 tok/s");
    }

    #[test]
    fn a_rate_is_drawn_at_every_width_that_draws_the_badge() {
        // The regression this pins: the rate used to ride on the chain's leftover
        // padding, and the chain steps up to a longer form as the rail gets wider.
        // Between 69 and 78 cells with this label the chain grew into the rate's
        // space and the number silently vanished, then came back at 79 — so a
        // waveform of a fast model looked like a terminal that could not show it.
        //
        // Wherever the state word is drawn there is room for the number, at every
        // width, and that is the invariant rather than any particular width.
        let app = streaming_app(
            &["Local model (http://192.168.1.50:1234/v1)", "Grok"],
            360,
            40,
        );
        assert!(app.stream_rate().is_some(), "there is a rate to draw");

        // Below BADGE_MIN_WIDTH the rail is chain-first by design and reserves
        // nothing, so a short chain on a very narrow terminal can still carry the
        // state word with no room for a number beside it. From there up the
        // reserve is unconditional, and the number is drawn with the word.
        for width in BADGE_MIN_WIDTH as u16..=200 {
            let text = text_of(&rail(&app, &Theme::default(), width));
            if text.contains("working") {
                assert!(
                    text.contains("tok/s"),
                    "the badge is drawn at {width} cells but not the rate: {text:?}"
                );
            }
        }
    }

    #[test]
    fn the_rate_does_not_widen_the_rail_it_is_reserved_on() {
        // The reservation must not be spent twice, and the invariant above must
        // not be bought by letting the badge push past the edge.
        let app = streaming_app(
            &["Local model (http://192.168.1.50:1234/v1)", "Grok"],
            360,
            40,
        );
        for width in 1u16..=200 {
            let line = rail(&app, &Theme::default(), width);
            assert!(
                line.width() <= width as usize,
                "rail overflowed {width} cells at {}",
                line.width()
            );
        }
    }

    #[test]
    fn the_reserved_slot_does_not_make_the_chain_re_flow() {
        // Reserved, not spent, and reserved whether or not it is in use: the chain
        // must read identically either way. The tempting alternative — reserving
        // only while a rate exists — would re-flow the top line at every start and
        // stop of the text, which is several times a turn.
        let labels = ["Local model (http://192.168.1.50:1234/v1)", "Grok"];
        let streaming = streaming_app(&labels, 360, 40);
        let mut quiet = app_with(&labels);
        quiet.busy = true;
        quiet.tick = 100;

        let with = text_of(&rail(&streaming, &Theme::default(), 120));
        let without = text_of(&rail(&quiet, &Theme::default(), 120));

        assert!(with.contains("tok/s"), "{with:?}");
        assert!(!without.contains("tok/s"), "{without:?}");

        // Everything up to the number itself — the wordmark, the chain in
        // whichever form it fits, the state word — is the same on both rails.
        let words = |text: &str| -> Vec<String> {
            text.split_whitespace()
                .take_while(|word| !word.starts_with('~'))
                .map(str::to_string)
                .collect()
        };
        assert_eq!(
            words(&with),
            words(&without),
            "the chain must not shrink when the rate starts"
        );
    }
}
