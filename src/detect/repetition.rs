//! Spotting an answer that has degenerated into a loop.
//!
//! Two signals, because they catch different shapes: the same line repeated
//! verbatim (the usual collapse), and the same span of tokens recurring inside
//! otherwise-varying text (a loop that rewrites itself slightly each time).

use std::collections::{HashMap, VecDeque};

use crate::detect::StuckReason;

/// Tokens in the span whose repeats count as a loop.
const NGRAM: usize = 12;
/// Bound on tracked spans, so a long answer cannot grow this without limit.
const MAX_TRACKED: usize = 4_096;
/// Repeats below this are not a loop, whatever the configuration says.
const MIN_THRESHOLD: usize = 2;

/// What the detector has seen so far.
///
/// The counts are kept rather than only the verdict, because "it looped" and
/// "how close did it come" are different questions and only the second one can
/// be tuned. A verdict thrown away at the threshold leaves nothing to read after
/// the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepetitionCounters {
    /// The run of identical consecutive lines currently standing.
    pub consecutive: usize,
    /// The worst count any single 12-token span has reached.
    pub span_repeats: usize,
    /// The configured threshold both of those are measured against.
    pub threshold: usize,
}

pub struct RepetitionDetector {
    threshold: usize,
    /// Text seen since the last complete line, which may be split mid-word.
    pending: String,
    last_line: String,
    consecutive: usize,
    recent_tokens: VecDeque<String>,
    span_counts: HashMap<Vec<String>, usize>,
}

impl RepetitionDetector {
    pub fn new(threshold: usize) -> Self {
        Self {
            threshold: threshold.max(MIN_THRESHOLD),
            pending: String::new(),
            last_line: String::new(),
            consecutive: 0,
            recent_tokens: VecDeque::new(),
            span_counts: HashMap::new(),
        }
    }

    /// How close it came, for a turn that ended without looping.
    ///
    /// `span_repeats` is the worst any one span reached rather than the count for
    /// the span that happens to be current, because that is the figure that says
    /// how near a loop the answer wandered.
    pub fn counters(&self) -> RepetitionCounters {
        RepetitionCounters {
            consecutive: self.consecutive,
            span_repeats: self.span_counts.values().copied().max().unwrap_or(0),
            threshold: self.threshold,
        }
    }

    /// Feed streamed text. Returns a reason as soon as the output has looped.
    pub fn feed(&mut self, chunk: &str) -> Option<StuckReason> {
        self.pending.push_str(chunk);

        // Only complete lines are judged, so a chunk boundary in the middle of
        // a line cannot be mistaken for a change of content.
        while let Some(newline) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=newline).collect();
            if let Some(reason) = self.take_line(line.trim_end_matches('\n')) {
                return Some(reason);
            }
        }

        None
    }

    fn take_line(&mut self, raw: &str) -> Option<StuckReason> {
        let line = normalise(raw);

        // Blank lines carry no signal and would otherwise look like repeats of
        // each other.
        if line.is_empty() {
            return None;
        }

        if line == self.last_line {
            self.consecutive += 1;
        } else {
            self.last_line = line.clone();
            self.consecutive = 1;
        }

        for token in line.split_whitespace() {
            self.recent_tokens.push_back(token.to_string());
            if self.recent_tokens.len() > NGRAM {
                self.recent_tokens.pop_front();
            }
            if self.recent_tokens.len() == NGRAM {
                if let Some(reason) = self.count_span() {
                    return Some(reason);
                }
            }
        }

        if self.consecutive >= self.threshold {
            return Some(StuckReason::Repetition {
                sample: shorten(&line),
                repeats: self.consecutive,
            });
        }

        None
    }

    fn count_span(&mut self) -> Option<StuckReason> {
        if self.span_counts.len() > MAX_TRACKED {
            // Cheap and bounded: start over rather than grow forever. A loop
            // in progress will re-accumulate within a few lines.
            self.span_counts.clear();
        }

        let span: Vec<String> = self.recent_tokens.iter().cloned().collect();
        let count = self.span_counts.entry(span).or_insert(0);
        *count += 1;

        if *count >= self.threshold {
            return Some(StuckReason::Repetition {
                sample: shorten(
                    &self
                        .recent_tokens
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                repeats: *count,
            });
        }
        None
    }
}

fn normalise(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn shorten(text: &str) -> String {
    if text.chars().count() <= 60 {
        return text.to_string();
    }
    let head: String = text.chars().take(60).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(detector: &mut RepetitionDetector, chunks: &[&str]) -> Option<StuckReason> {
        chunks.iter().find_map(|chunk| detector.feed(chunk))
    }

    #[test]
    fn repeated_identical_lines_trip_at_the_threshold() {
        let mut detector = RepetitionDetector::new(3);
        assert!(detector.feed("the same line\n").is_none());
        assert!(detector.feed("the same line\n").is_none());
        let reason = detector
            .feed("the same line\n")
            .expect("the third repeat is a loop");
        match reason {
            StuckReason::Repetition { repeats, sample } => {
                assert_eq!(repeats, 3);
                assert_eq!(sample, "the same line");
            }
            other => panic!("expected repetition, got {other:?}"),
        }
    }

    #[test]
    fn one_repeat_short_of_the_threshold_is_fine() {
        let mut detector = RepetitionDetector::new(4);
        assert!(feed_all(&mut detector, &["x\n", "x\n", "x\n"]).is_none());
    }

    #[test]
    fn varied_prose_never_trips() {
        let mut detector = RepetitionDetector::new(3);
        let reason = feed_all(
            &mut detector,
            &[
                "Here is how the parser works.\n",
                "It reads a frame at a time.\n",
                "Each frame is a JSON object.\n",
                "The delta carries the text.\n",
                "Tool calls arrive in pieces.\n",
            ],
        );
        assert!(
            reason.is_none(),
            "ordinary prose tripped the detector: {reason:?}"
        );
    }

    #[test]
    fn a_loop_that_varies_its_text_is_caught_by_the_span_check() {
        let mut detector = RepetitionDetector::new(3);
        let span = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima";
        // Each repeat sits on its own line, so the line check never fires: only
        // the recurring token span gives it away.
        let reason = feed_all(
            &mut detector,
            &[
                &format!("{span}\n"),
                &format!("{span}\n"),
                &format!("{span}\n"),
            ],
        );
        assert!(
            matches!(reason, Some(StuckReason::Repetition { .. })),
            "got {reason:?}"
        );
    }

    #[test]
    fn a_chunk_split_mid_line_is_reassembled_before_judging() {
        let mut detector = RepetitionDetector::new(2);
        assert!(detector.feed("repea").is_none());
        // Completes the first line; not yet a repeat.
        assert!(detector.feed("ted\n").is_none());
        let reason = detector
            .feed("repeated\n")
            .expect("the second line repeats");
        assert!(matches!(reason, StuckReason::Repetition { repeats: 2, .. }));
    }

    #[test]
    fn blank_lines_do_not_count_as_repeats() {
        let mut detector = RepetitionDetector::new(3);
        assert!(feed_all(&mut detector, &["\n", "\n", "\n", "   \n", "\n"]).is_none());
    }

    #[test]
    fn trailing_whitespace_differences_still_count_as_the_same_line() {
        let mut detector = RepetitionDetector::new(2);
        assert!(detector.feed("same\n").is_none());
        let reason = detector
            .feed("   same   \n")
            .expect("whitespace should not hide a repeat");
        assert!(matches!(reason, StuckReason::Repetition { repeats: 2, .. }));
    }

    #[test]
    fn a_single_very_long_line_is_shortened_for_the_message() {
        let mut detector = RepetitionDetector::new(2);
        let long = "x".repeat(500);
        assert!(detector.feed(&format!("{long}\n")).is_none());
        match detector.feed(&format!("{long}\n")) {
            Some(StuckReason::Repetition { sample, .. }) => {
                assert!(
                    sample.chars().count() <= 61,
                    "sample was {} chars",
                    sample.chars().count()
                );
            }
            other => panic!("expected repetition, got {other:?}"),
        }
    }

    #[test]
    fn differing_lines_reset_the_consecutive_count() {
        let mut detector = RepetitionDetector::new(3);
        // Same line, then a different one, then the first again: never three in
        // a row, so this must not trip.
        let reason = feed_all(&mut detector, &["a\n", "a\n", "b\n", "a\n", "a\n"]);
        assert!(reason.is_none(), "got {reason:?}");
    }

    #[test]
    fn a_threshold_below_two_is_raised_rather_than_firing_immediately() {
        let mut detector = RepetitionDetector::new(0);
        assert!(detector.feed("first\n").is_none());
        assert!(detector.feed("second\n").is_none());
    }
}
