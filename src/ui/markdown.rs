//! Rendering the model's prose as markdown, at a fixed width.
//!
//! Models answer in markdown whether or not anyone asked them to, and showing
//! `**bold**` and triple backticks literally makes a good answer look broken.
//! This turns the common subset — headings, emphasis, inline code, fenced code,
//! lists, quotes, rules — into styled cells.
//!
//! Two rules guide everything here.
//!
//! **Nothing may exceed the width it was given.** The transcript asserts this,
//! so every path hard-splits rather than overflow: a heading, a code line, and a
//! word too long for a line all break at the edge.
//!
//! **Nothing is guessed at while a turn is streaming.** A marker with no closing
//! partner (`**bo`, an unterminated fence) is emitted as literal text and an
//! open fence stays open to the end, so a half-arrived answer never flickers
//! between two shapes. It only becomes emphasis once the closing marker lands.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::text;
use crate::ui::theme::Theme;

/// A code fence needs a box with room for `│ `, content, ` │`. Below this the
/// frame is replaced by a plain gutter.
const MIN_FRAME_WIDTH: usize = 8;

/// Render `source` as styled lines no wider than `width`.
pub fn lines(source: &str, width: usize, theme: &Theme, base: Style) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut fence: Option<String> = None;

    for raw in source.split('\n') {
        if fence.is_some() {
            if is_fence(raw) {
                out.push(code_close(width, theme));
                fence = None;
            } else {
                out.extend(code_line(raw, width, theme));
            }
            continue;
        }

        if is_fence(raw) {
            let language = raw.trim().trim_start_matches('`').trim().to_string();
            out.push(code_open(width, &language, theme));
            fence = Some(language);
            continue;
        }

        let trimmed = raw.trim_start();

        if trimmed.is_empty() {
            out.push(Line::raw(""));
            continue;
        }

        if let Some(level) = heading_level(trimmed) {
            let style = if level <= 2 {
                theme.heading
            } else {
                theme.subheading
            };
            let body = trimmed[level..].trim_start();
            out.extend(wrap(&inline(body, style, theme), width, &[], &[], style));
            continue;
        }

        if is_rule(trimmed) {
            out.push(Line::styled("─".repeat(width), theme.rule));
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix('>') {
            let bar = Span::styled("▏ ", theme.quote);
            let body = rest.strip_prefix(' ').unwrap_or(rest);
            let segments = inline(body, theme.quote, theme);
            out.extend(wrap(
                &segments,
                width,
                std::slice::from_ref(&bar),
                std::slice::from_ref(&bar),
                theme.quote,
            ));
            continue;
        }

        if let Some((marker, rest)) = list_marker(trimmed) {
            let indent = raw.len() - trimmed.len();
            let pad = " ".repeat(indent);
            let bullet = Span::styled(
                format!("{pad}{marker} "),
                if marker == "•" {
                    theme.bullet
                } else {
                    theme.subheading
                },
            );
            // Continuations clear the marker and the space after it, so the text
            // of a wrapped list item lines up under the first word rather than
            // under the bullet.
            let hanging = Span::raw(" ".repeat(indent + text::display_width(&marker) + 1));
            let segments = inline(rest, base, theme);
            out.extend(wrap(&segments, width, &[bullet], &[hanging], base));
            continue;
        }

        out.extend(wrap(&inline(raw, base, theme), width, &[], &[], base));
    }

    // An unterminated fence still gets its bottom edge, so it reads as a block
    // that is still arriving rather than as a stray line of bars.
    if fence.is_some() {
        out.push(code_close(width, theme));
    }

    if out.is_empty() {
        out.push(Line::raw(""));
    }
    out
}

fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// `# ` through `###### `, in cells, or `None` when this is not a heading.
fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|ch| *ch == '#').count();
    if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
        Some(hashes)
    } else {
        None
    }
}

/// `---`, `***`, `___` and longer, with nothing else on the line.
fn is_rule(line: &str) -> bool {
    let mut chars = line.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !matches!(first, '-' | '*' | '_') {
        return false;
    }
    let count = 1 + chars.clone().take_while(|ch| *ch == first).count();
    count >= 3 && chars.all(|ch| ch == first)
}

/// A bullet or an ordered-list number, returned with the text after it.
fn list_marker(line: &str) -> Option<(String, &str)> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(("•".to_string(), rest));
        }
    }

    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && line[digits..].starts_with(". ") {
        return Some((line[..digits + 1].to_string(), &line[digits + 2..]));
    }
    None
}

// ---------------------------------------------------------------- code fences

fn code_open(width: usize, language: &str, theme: &Theme) -> Line<'static> {
    if width < MIN_FRAME_WIDTH {
        return Line::raw("");
    }

    let mut spans = vec![Span::styled("╭─", theme.code_frame)];
    let mut used = 2;

    let label = if language.is_empty() {
        String::new()
    } else {
        let room = width.saturating_sub(4);
        let label = format!(" {} ", language);
        truncate(&label, room)
    };
    if !label.is_empty() {
        used += text::display_width(&label);
        spans.push(Span::styled(label, theme.code_frame));
    }

    // Fill to the closing corner, so the box is square at every width.
    let fill = width.saturating_sub(used + 1);
    if fill > 0 {
        spans.push(Span::styled("─".repeat(fill), theme.code_frame));
    }
    spans.push(Span::styled("╮", theme.code_frame));
    Line::from(spans)
}

fn code_close(width: usize, theme: &Theme) -> Line<'static> {
    if width < MIN_FRAME_WIDTH {
        return Line::raw("");
    }
    Line::styled(
        format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        theme.code_frame,
    )
}

fn code_line(source: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    if width < MIN_FRAME_WIDTH {
        // No room for a framed box: a gutter bar and the code, split to fit.
        let body = width.saturating_sub(2);
        return split_to_width(source, body.max(1))
            .into_iter()
            .map(|part| {
                Line::from(vec![
                    Span::styled("▏ ", theme.code_frame),
                    Span::styled(part, theme.code),
                ])
            })
            .collect();
    }

    let inner = width - 4;
    split_to_width(source, inner.max(1))
        .into_iter()
        .map(|part| {
            let pad = inner.saturating_sub(text::display_width(&part));
            Line::from(vec![
                Span::styled("│ ", theme.code_frame),
                Span::styled(part, theme.code),
                Span::styled(" ".repeat(pad), theme.code_frame),
                Span::styled(" │", theme.code_frame),
            ])
        })
        .collect()
}

// -------------------------------------------------------------------- inline

/// Split a line into styled runs: code, bold, italic, links, and the rest.
fn inline(source: &str, base: Style, theme: &Theme) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut literal = String::new();
    let mut index = 0usize;

    'scan: while index < source.len() {
        let rest = &source[index..];

        if let Some(after) = rest.strip_prefix('`') {
            if let Some(end) = after.find('`') {
                flush(&mut out, &mut literal, base);
                push(&mut out, after[..end].to_string(), theme.code);
                index += end + 2;
                continue;
            }
        }

        for marker in ["**", "__"] {
            if rest.starts_with(marker) && opens(rest, marker.len()) {
                if let Some(end) = rest[marker.len()..].find(marker) {
                    let inner = &rest[marker.len()..marker.len() + end];
                    if !inner.is_empty() {
                        flush(&mut out, &mut literal, base);
                        flush_inline(
                            &mut out,
                            inline(inner, base.add_modifier(Modifier::BOLD), theme),
                        );
                        index += marker.len() * 2 + end;
                        // The label matters: an unlabelled `continue` here would
                        // resume the marker loop, not the scanner, and the same
                        // text would be walked again.
                        continue 'scan;
                    }
                }
            }
        }

        if rest.starts_with('*') || rest.starts_with('_') {
            let marker = &rest[..1];
            if opens(rest, 1) {
                if let Some(end) = rest[1..].find(marker) {
                    let inner = &rest[1..1 + end];
                    if !inner.is_empty() && closes(rest, 1 + end, 1) {
                        flush(&mut out, &mut literal, base);
                        flush_inline(
                            &mut out,
                            inline(inner, base.add_modifier(Modifier::ITALIC), theme),
                        );
                        index += end + 2;
                        continue;
                    }
                }
            }
        }

        if rest.starts_with('[') {
            if let Some(label_end) = rest.find("](") {
                if let Some(url_end) = rest[label_end + 2..].find(')') {
                    flush(&mut out, &mut literal, base);
                    push(&mut out, rest[1..label_end].to_string(), theme.link);
                    index += label_end + 2 + url_end + 1;
                    continue;
                }
            }
        }

        let ch = rest.chars().next().expect("a character");
        literal.push(ch);
        index += ch.len_utf8();
    }

    flush(&mut out, &mut literal, base);
    out
}

/// Whether an emphasis marker may open here: not mid-word, and followed by
/// content. This is what keeps `some_var_name` from turning into `some<italic>`.
fn opens(rest: &str, marker_len: usize) -> bool {
    let after = rest[marker_len..].chars().next();
    matches!(after, Some(ch) if !ch.is_whitespace())
}

/// Whether an emphasis marker may close here: preceded by content and followed
/// by a word boundary.
fn closes(rest: &str, marker_at: usize, marker_len: usize) -> bool {
    let before = rest[..marker_at].chars().next_back();
    let after = rest[marker_at + marker_len..].chars().next();
    let preceded = matches!(before, Some(ch) if !ch.is_whitespace());
    let followed = matches!(
        after,
        None | Some(' ' | '\t' | '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
    );
    preceded && followed
}

fn flush(out: &mut Vec<(String, Style)>, literal: &mut String, style: Style) {
    if !literal.is_empty() {
        push(out, std::mem::take(literal), style);
    }
}

fn flush_inline(out: &mut Vec<(String, Style)>, more: Vec<(String, Style)>) {
    for (text, style) in more {
        push(out, text, style);
    }
}

/// Append a run, merging it with the previous one when they share a style, so a
/// paragraph does not end up as one span per character.
fn push(out: &mut Vec<(String, Style)>, text: String, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut() {
        if last.1 == style {
            last.0.push_str(&text);
            return;
        }
    }
    out.push((text, style));
}

// -------------------------------------------------------------------- wrapping

/// Word-wrap styled runs, giving every line before the first and every line
/// after it a prefix of its own — the hanging indent under a bullet, the bar
/// beside a quote.
fn wrap(
    segments: &[(String, Style)],
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
    fallback: Style,
) -> Vec<Line<'static>> {
    let first_width: usize = first.iter().map(Span::width).sum();
    let rest_width: usize = rest.iter().map(Span::width).sum();
    let body = width.saturating_sub(first_width.max(rest_width)).max(1);

    let words = words(segments, fallback);
    let wrapped = wrap_words(&words, body);
    let wrapped = if wrapped.is_empty() {
        vec![Vec::new()]
    } else {
        wrapped
    };

    wrapped
        .into_iter()
        .enumerate()
        .map(|(index, spans)| {
            let mut line: Vec<Span<'static>> = if index == 0 {
                first.to_vec()
            } else {
                rest.to_vec()
            };
            line.extend(spans);
            Line::from(coalesce(line))
        })
        .collect()
}

/// Join adjacent spans that share a style, so an emphasis run reads as one span
/// rather than one per word.
fn coalesce(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    for span in spans {
        if let Some(last) = out.last_mut() {
            if last.style == span.style {
                last.content.to_mut().push_str(&span.content);
                continue;
            }
        }
        out.push(span);
    }
    out
}

/// Flatten styled runs into words, each carrying its style.
fn words(segments: &[(String, Style)], fallback: Style) -> Vec<(String, Style)> {
    let mut out = Vec::new();
    for (text, style) in segments {
        for word in text.split_whitespace() {
            out.push((word.to_string(), *style));
        }
    }
    if out.is_empty() {
        return vec![(String::new(), fallback)];
    }
    out
}

/// Greedy word wrap. A word too long for a whole line is split at the edge,
/// because the alternative is overflow.
///
/// The queue is what makes the over-long case correct: the tail of a split word
/// is put back at the front to be measured again, so it lands immediately after
/// the piece that was just emitted rather than somewhere else in the paragraph.
fn wrap_words(words: &[(String, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    use std::collections::VecDeque;

    let width = width.max(1);
    let mut queue: VecDeque<(String, Style)> = words.iter().cloned().collect();
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    // The style of the last word on the line. The space between words wears it,
    // so a multi-word emphasis run stays one span instead of being cut apart at
    // every space.
    let mut last: Option<Style> = None;

    while let Some((word, style)) = queue.pop_front() {
        let word_width = text::display_width(&word);

        if used == 0 && word_width > width {
            let (head, tail) = split_at(&word, width);
            lines.push(vec![Span::styled(head.to_string(), style)]);
            last = None;
            if !tail.is_empty() {
                queue.push_front((tail.to_string(), style));
            }
            continue;
        }

        let needed = if used == 0 {
            word_width
        } else {
            used + 1 + word_width
        };
        if needed <= width {
            if used > 0 {
                current.push(Span::styled(" ", last.unwrap_or(style)));
                used += 1;
            }
            current.push(Span::styled(word, style));
            last = Some(style);
            used += word_width;
            continue;
        }

        lines.push(std::mem::take(&mut current));
        used = 0;
        last = None;
        queue.push_front((word, style));
    }

    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Split a string at the last character boundary that fits in `width` cells.
fn split_at(text: &str, width: usize) -> (&str, &str) {
    let mut used = 0usize;
    let mut last_fit = None;

    for (index, ch) in text.char_indices() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > width {
            break;
        }
        used += ch_width;
        last_fit = Some(index + ch.len_utf8());
    }

    match last_fit {
        Some(end) => text.split_at(end),
        None => {
            let end = text
                .char_indices()
                .nth(1)
                .map(|(index, _)| index)
                .unwrap_or(text.len());
            text.split_at(end)
        }
    }
}

/// Hard-split one source line into pieces of at most `width` cells.
fn split_to_width(source: &str, width: usize) -> Vec<String> {
    if source.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut rest = source;
    while !rest.is_empty() {
        let (head, tail) = split_at(rest, width.max(1));
        out.push(head.to_string());
        if head.is_empty() && tail.len() == rest.len() {
            break;
        }
        rest = tail;
    }
    out
}

/// Cut a string to a display width, keeping whole characters.
fn truncate(text: &str, width: usize) -> String {
    split_at(text, width).0.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(source: &str, width: usize) -> Vec<String> {
        let theme = Theme::default();
        lines(source, width, &theme, theme.assistant)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn styled(source: &str, width: usize) -> Vec<Line<'static>> {
        let theme = Theme::default();
        lines(source, width, &theme, theme.assistant)
    }

    #[test]
    fn a_heading_loses_its_hashes_and_takes_weight() {
        let theme = Theme::default();
        let out = styled("## Results", 40);

        assert_eq!(out[0].spans[0].content, "Results");
        assert_eq!(out[0].spans[0].style, theme.heading);
    }

    #[test]
    fn a_deeper_heading_is_quieter_than_a_shallow_one() {
        let theme = Theme::default();
        assert_eq!(styled("# Top", 40)[0].spans[0].style, theme.heading);
        assert_eq!(
            styled("#### Detail", 40)[0].spans[0].style,
            theme.subheading
        );
    }

    #[test]
    fn inline_code_is_styled_and_stripped_of_its_ticks() {
        let theme = Theme::default();
        let out = styled("call `wrap(text, width)` now", 60);
        let code = out[0]
            .spans
            .iter()
            .find(|span| span.content.contains("wrap"))
            .expect("the code span");
        assert_eq!(code.style, theme.code);
        assert!(!out[0].spans.iter().any(|s| s.content.contains('`')));
    }

    #[test]
    fn bold_and_italic_are_marked_by_weight_rather_than_kept_literal() {
        let out = styled("this is **important** and *subtle*", 60);
        let text: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();

        assert_eq!(text, "this is important and subtle");
        let bold = out[0]
            .spans
            .iter()
            .find(|s| s.content.contains("important"))
            .expect("the bold run");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let italic = out[0]
            .spans
            .iter()
            .find(|s| s.content.contains("subtle"))
            .expect("the italic run");
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn an_underscore_inside_a_word_is_left_alone() {
        // snake_case identifiers are everywhere in the text this renders, and
        // treating them as emphasis would italicize half of every code review.
        let out = render("set some_var_name and other_thing here", 60);
        assert_eq!(out[0], "set some_var_name and other_thing here");
    }

    #[test]
    fn an_unclosed_marker_stays_literal() {
        // Half of a streamed `**bold**` must not flip the rest of the line into
        // bold before the closing marker arrives.
        let out = render("this is **still arriving", 60);
        assert_eq!(out[0], "this is **still arriving");
    }

    #[test]
    fn a_fenced_block_is_framed_and_keeps_its_language() {
        let out = render("```rust\nlet x = 1;\n```", 40);
        assert!(out[0].starts_with("╭─ rust"), "{:?}", out[0]);
        assert!(out[0].ends_with('╮'), "{:?}", out[0]);
        assert!(out[1].contains("let x = 1;"), "{:?}", out[1]);
        assert!(out[1].starts_with("│ "), "{:?}", out[1]);
        assert!(out[2].starts_with('╰'), "{:?}", out[2]);
    }

    #[test]
    fn an_unterminated_fence_still_gets_a_bottom_edge() {
        // A code block that is still streaming ends with its own closing line,
        // so it never looks like the frame was left open.
        let out = render("```\nfn main() {}", 30);
        assert!(out.last().expect("a line").starts_with('╰'), "{out:?}");
    }

    #[test]
    fn a_code_line_too_wide_is_split_inside_the_frame() {
        let out = render("```\nabcdefghijklmnopqrstuvwxyz\n```", 20);
        for line in &out {
            assert!(text::display_width(line) <= 20, "overflowed: {line:?}");
        }
    }

    #[test]
    fn a_bullet_becomes_a_dot_and_its_continuation_hangs_indented() {
        let out = render("- alpha beta gamma delta epsilon zeta", 20);
        assert!(out[0].starts_with("• alpha"), "{out:?}");
        // Every line after the first must clear the bullet's own width.
        for line in &out[1..] {
            assert!(line.starts_with("  "), "the wrap should hang: {out:?}");
        }
    }

    #[test]
    fn an_ordered_list_keeps_its_numbers() {
        let out = render("1. first\n2. second", 30);
        assert!(out[0].starts_with("1. first"), "{out:?}");
        assert!(out[1].starts_with("2. second"), "{out:?}");
    }

    #[test]
    fn a_quote_is_marked_by_a_bar() {
        let out = render("> quoted words", 30);
        assert!(out[0].starts_with("▏ quoted"), "{out:?}");
    }

    #[test]
    fn a_rule_draws_across_the_whole_width() {
        let out = render("---", 24);
        assert_eq!(out[0], "─".repeat(24));
    }

    #[test]
    fn a_link_shows_its_label_rather_than_its_url() {
        let theme = Theme::default();
        let out = styled("see [the docs](https://example.com/x) for more", 60);
        let text: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();

        assert!(text.contains("the docs"), "{text}");
        assert!(!text.contains("example.com"), "{text}");
        let link = out[0]
            .spans
            .iter()
            .find(|s| s.content.contains("the docs"))
            .expect("the link run");
        assert_eq!(link.style, theme.link);
    }

    #[test]
    fn no_line_ever_exceeds_the_width_it_was_given() {
        let document = "# Title\n\nSome *prose* with `code` and a [link](http://x).\n\n\
                        - a bullet that runs on for a good while indeed\n\
                        - another\n\n\
                        ```rust\nfn a_very_long_function_name(argument: &str) -> String { todo!() }\n```\n\n\
                        > a quote long enough to wrap\n\n---\n";
        for width in [8usize, 12, 20, 33, 47, 80] {
            for line in styled(document, width) {
                assert!(
                    line.width() <= width,
                    "overflowed {width}: {:?}",
                    line.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                );
            }
        }
    }

    #[test]
    fn a_narrow_frame_falls_back_to_a_gutter_rather_than_breaking() {
        // Below the width a full box needs, code still renders without drawing
        // corners over each other.
        let out = render("```\nx\n```", 6);
        for line in &out {
            assert!(text::display_width(line) <= 6, "{line:?}");
        }
    }

    #[test]
    fn blank_lines_are_preserved() {
        let out = render("one\n\ntwo", 20);
        assert_eq!(out, vec!["one", "", "two"]);
    }

    #[test]
    fn an_empty_source_still_yields_one_line() {
        assert_eq!(render("", 20), vec![""]);
    }

    #[test]
    fn wide_characters_are_never_split_in_half() {
        let out = render("日本語のテキストがここにあります", 8);
        for line in &out {
            assert!(
                text::display_width(line) <= 8,
                "a wide glyph was cut: {line:?}"
            );
        }
    }
}
