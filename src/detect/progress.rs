//! Spotting an attempt that keeps doing the same thing without getting anywhere.
//!
//! Repetition in the *answer* is one failure mode; a model that keeps issuing
//! the same tool call, or fails the same way over and over, is the other.

use crate::detect::StuckReason;

/// Below this, the "pattern" is just normal tool use.
const MIN_THRESHOLD: usize = 2;

pub struct ProgressDetector {
    threshold: usize,
    last_call: Option<(String, String)>,
    same_run: usize,
    failure_run: usize,
    last_failed_tool: String,
}

impl ProgressDetector {
    pub fn new(threshold: usize) -> Self {
        Self {
            threshold: threshold.max(MIN_THRESHOLD),
            last_call: None,
            same_run: 0,
            failure_run: 0,
            last_failed_tool: String::new(),
        }
    }

    /// Record one tool call and whether it succeeded. Returns a reason once the
    /// sequence looks like a loop rather than work.
    pub fn record(&mut self, tool: &str, arguments: &str, ok: bool) -> Option<StuckReason> {
        let call = (tool.to_string(), arguments.to_string());
        if self.last_call.as_ref() == Some(&call) {
            self.same_run += 1;
        } else {
            self.last_call = Some(call);
            self.same_run = 1;
        }

        if ok {
            self.failure_run = 0;
        } else {
            self.failure_run += 1;
            self.last_failed_tool = tool.to_string();
        }

        if self.same_run >= self.threshold {
            return Some(StuckReason::RepeatedToolCall {
                tool: tool.to_string(),
                times: self.same_run,
            });
        }

        if self.failure_run >= self.threshold {
            return Some(StuckReason::RepeatedToolFailure {
                tool: self.last_failed_tool.clone(),
                times: self.failure_run,
            });
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_call_with_the_same_arguments_trips_at_the_threshold() {
        let mut detector = ProgressDetector::new(3);
        assert!(detector.record("list_dir", "{}", true).is_none());
        assert!(detector.record("list_dir", "{}", true).is_none());
        let reason = detector
            .record("list_dir", "{}", true)
            .expect("the third identical call is a loop");
        assert_eq!(
            reason,
            StuckReason::RepeatedToolCall {
                tool: "list_dir".to_string(),
                times: 3
            }
        );
    }

    #[test]
    fn the_same_tool_with_different_arguments_is_not_a_loop() {
        let mut detector = ProgressDetector::new(3);
        assert!(
            detector
                .record("read_file", r#"{"path":"a"}"#, true)
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path":"b"}"#, true)
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path":"c"}"#, true)
                .is_none()
        );
    }

    #[test]
    fn alternating_calls_are_not_a_loop() {
        let mut detector = ProgressDetector::new(2);
        assert!(detector.record("read_file", "{}", true).is_none());
        assert!(detector.record("list_dir", "{}", true).is_none());
        assert!(detector.record("read_file", "{}", true).is_none());
        assert!(detector.record("list_dir", "{}", true).is_none());
    }

    #[test]
    fn a_streak_of_failures_trips_even_when_each_call_differs() {
        let mut detector = ProgressDetector::new(3);
        assert!(
            detector
                .record("read_file", r#"{"path":"a"}"#, false)
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path":"b"}"#, false)
                .is_none()
        );
        let reason = detector
            .record("run_shell", r#"{"command":"ls"}"#, false)
            .expect("three failures in a row is not progress");
        assert_eq!(
            reason,
            StuckReason::RepeatedToolFailure {
                tool: "run_shell".to_string(),
                times: 3
            }
        );
    }

    #[test]
    fn a_success_clears_the_failure_streak() {
        let mut detector = ProgressDetector::new(2);
        assert!(
            detector
                .record("read_file", r#"{"path":"a"}"#, false)
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path":"b"}"#, true)
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path":"c"}"#, false)
                .is_none()
        );
    }

    #[test]
    fn a_threshold_below_two_is_raised_rather_than_firing_immediately() {
        let mut detector = ProgressDetector::new(0);
        assert!(detector.record("read_file", "{}", true).is_none());
        // The second identical call reaches the raised threshold of two.
        assert!(detector.record("read_file", "{}", true).is_some());
    }
}
