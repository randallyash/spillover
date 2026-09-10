//! Display-width aware text wrapping for the transcript pane.

use unicode_width::UnicodeWidthChar;

/// Width of a string in terminal cells, ignoring control characters.
pub fn display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

/// Split `text` into lines no wider than `width` cells.
///
/// Breaks on spaces where possible and hard-splits a word that cannot fit on a
/// line of its own, so a long URL or a base64 blob still renders.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();

    for raw_line in text.split('\n') {
        let cleaned = clean(raw_line);
        if cleaned.is_empty() {
            out.push(String::new());
            continue;
        }

        let mut current = String::new();
        let mut current_width = 0usize;
        for word in cleaned.split(' ') {
            push_word(&mut out, &mut current, &mut current_width, word, width);
        }
        if !current.is_empty() {
            out.push(current);
        }
    }

    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Drop characters that would corrupt the rendered layout.
fn clean(line: &str) -> String {
    line.chars()
        .filter(|ch| !ch.is_control() || *ch == '\t')
        .map(|ch| if ch == '\t' { ' ' } else { ch })
        .collect()
}

fn push_word(
    out: &mut Vec<String>,
    current: &mut String,
    current_width: &mut usize,
    word: &str,
    width: usize,
) {
    let mut remaining = word;

    loop {
        let word_width = display_width(remaining);

        if *current_width == 0 {
            if word_width <= width {
                current.push_str(remaining);
                *current_width = word_width;
                return;
            }
            // Too long for an empty line: take what fits and continue on the next.
            let (head, tail) = split_at_width(remaining, width);
            out.push(head.to_string());
            remaining = tail;
            if remaining.is_empty() {
                return;
            }
            continue;
        }

        if *current_width + 1 + word_width <= width {
            current.push(' ');
            current.push_str(remaining);
            *current_width += 1 + word_width;
            return;
        }

        out.push(std::mem::take(current));
        *current_width = 0;
    }
}

/// Split a string at the last character boundary that fits in `width` cells.
/// Always returns at least one character in `head` so callers make progress.
fn split_at_width(text: &str, width: usize) -> (&str, &str) {
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
            // Not even one character fit; force one through to guarantee progress.
            let end = text
                .char_indices()
                .nth(1)
                .map(|(index, _)| index)
                .unwrap_or(text.len());
            text.split_at(end)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_on_spaces() {
        assert_eq!(
            wrap("the quick brown fox", 9),
            vec!["the quick", "brown fox"]
        );
    }

    #[test]
    fn preserves_explicit_newlines() {
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"]);
    }

    #[test]
    fn keeps_blank_lines() {
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn hard_splits_overlong_words() {
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn counts_wide_characters_as_two_cells() {
        // Four CJK characters are eight cells wide.
        let wrapped = wrap("日本語漢字", 4);
        assert_eq!(wrapped, vec!["日本", "語漢", "字"]);
    }

    #[test]
    fn never_returns_nothing_for_empty_input() {
        assert_eq!(wrap("", 10), vec![""]);
    }

    #[test]
    fn strips_control_characters() {
        assert_eq!(wrap("a\rb", 10), vec!["ab"]);
    }

    #[test]
    fn does_not_lose_content() {
        let source = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let joined: String = wrap(source, 12).join(" ");
        assert_eq!(joined.split_whitespace().count(), 10);
    }
}
