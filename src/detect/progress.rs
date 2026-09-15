//! Spotting an attempt that keeps doing the same thing without getting anywhere.
//!
//! Repetition in the *answer* is one failure mode; a model that keeps issuing the
//! same tool call, or keeps hitting the same wall, is the other.
//!
//! Three signals, in increasing specificity:
//!
//! - the same call with the same arguments, which is a loop whatever it returns;
//! - a run of failures, which is not learning from what it is told;
//! - the same *kind* of failure from the same tool, which is the strongest
//!   evidence of the three: the model has been shown the same wall repeatedly and
//!   is not adapting. That one earns a tighter budget than the others, because
//!   waiting through the tier's whole allowance would spend several more calls
//!   learning nothing.

use serde_json::Value;

use crate::detect::{ErrorClass, StuckReason};

/// Below this, the "pattern" is just normal tool use.
const MIN_THRESHOLD: usize = 2;

/// The arguments of a call, as two calls can be compared.
fn comparable(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.to_string()))
}

/// What the detector has seen so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressCounters {
    /// Identical calls — same tool, same arguments — currently in a row.
    pub same_run: usize,
    /// Consecutive failures currently standing, whatever they were.
    pub failure_run: usize,
    /// The same failure currently repeating, and how many times.
    pub error_run: usize,
    pub error_class: Option<ErrorClass>,
    /// The configured allowance both runs are measured against.
    pub threshold: usize,
}

pub struct ProgressDetector {
    threshold: usize,
    /// The last call as `(tool, arguments-as-meaning)`, so a repeat is judged on
    /// what was asked for rather than on how it was written.
    last_call: Option<(String, Value)>,
    same_run: usize,
    failure_run: usize,
    last_failed_tool: String,
    /// The failure currently repeating, and how many times in a row.
    error_streak: Option<(String, ErrorClass, Option<Value>)>,
    error_run: usize,
}

impl ProgressDetector {
    pub fn new(threshold: usize) -> Self {
        Self {
            threshold: threshold.max(MIN_THRESHOLD),
            last_call: None,
            same_run: 0,
            failure_run: 0,
            last_failed_tool: String::new(),
            error_streak: None,
            error_run: 0,
        }
    }

    /// How close it came, for a turn that ended without tripping.
    pub fn counters(&self) -> ProgressCounters {
        ProgressCounters {
            same_run: self.same_run,
            failure_run: self.failure_run,
            error_run: self.error_run,
            error_class: self.error_streak.as_ref().map(|(_, class, _)| *class),
            threshold: self.threshold,
        }
    }

    /// Record one tool call and how it went. Returns a reason once the sequence
    /// looks like a loop rather than work.
    pub fn record(
        &mut self,
        tool: &str,
        arguments: &str,
        failure: Option<ErrorClass>,
    ) -> Option<StuckReason> {
        let call = (tool.to_string(), comparable(arguments));
        if self.last_call.as_ref() == Some(&call) {
            self.same_run += 1;
        } else {
            self.last_call = Some(call);
            self.same_run = 1;
        }

        match failure {
            Some(class) => {
                self.failure_run += 1;
                self.last_failed_tool = tool.to_string();

                // `None` for the target when the class blames the call rather
                // than the thing it was aimed at, so those accumulate across
                // different targets; a file-shaped failure carries its path.
                let target = class.needs_the_same_target().then(|| comparable(arguments));
                let key = (tool.to_string(), class, target);
                if self.error_streak.as_ref() == Some(&key) {
                    self.error_run += 1;
                } else {
                    self.error_streak = Some(key);
                    self.error_run = 1;
                }
            }
            None => {
                self.failure_run = 0;
                self.error_streak = None;
                self.error_run = 0;
            }
        }

        if self.same_run >= self.threshold {
            return Some(StuckReason::RepeatedToolCall {
                tool: tool.to_string(),
                times: self.same_run,
            });
        }

        // Checked before the general run, and against the class's own budget
        // rather than the tier's: it is the more specific finding, and its budget
        // is never larger, so it can only fire at or before the general one. A
        // class with no budget of its own has nothing to say here and falls
        // through to the run-of-failures rule.
        if let Some((tool, class, _)) = &self.error_streak {
            if let Some(budget) = class.repeat_budget(self.threshold) {
                if self.error_run >= budget {
                    return Some(StuckReason::RepeatedToolError {
                        tool: tool.clone(),
                        class: *class,
                        times: self.error_run,
                    });
                }
            }
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
        assert!(detector.record("list_dir", "{}", None).is_none());
        assert!(detector.record("list_dir", "{}", None).is_none());
        let reason = detector
            .record("list_dir", "{}", None)
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
                .record("read_file", r#"{"path":"a"}"#, None)
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path":"b"}"#, None)
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path":"c"}"#, None)
                .is_none()
        );
    }

    #[test]
    fn alternating_calls_are_not_a_loop() {
        let mut detector = ProgressDetector::new(2);
        assert!(detector.record("read_file", "{}", None).is_none());
        assert!(detector.record("list_dir", "{}", None).is_none());
        assert!(detector.record("read_file", "{}", None).is_none());
        assert!(detector.record("list_dir", "{}", None).is_none());
    }

    #[test]
    fn a_streak_of_failures_trips_even_when_each_call_differs() {
        let mut detector = ProgressDetector::new(3);
        assert!(
            detector
                .record("read_file", r#"{"path":"a"}"#, Some(ErrorClass::Other))
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path":"b"}"#, Some(ErrorClass::Other))
                .is_none()
        );
        let reason = detector
            .record("run_shell", r#"{"command":"ls"}"#, Some(ErrorClass::Other))
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
                .record("read_file", r#"{"path":"a"}"#, Some(ErrorClass::Other))
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path":"b"}"#, None)
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path":"c"}"#, Some(ErrorClass::Other))
                .is_none()
        );
    }

    #[test]
    fn a_threshold_below_two_is_raised_rather_than_firing_immediately() {
        let mut detector = ProgressDetector::new(0);
        assert!(detector.record("read_file", "{}", None).is_none());
        // The second identical call reaches the raised threshold of two.
        assert!(detector.record("read_file", "{}", None).is_some());
    }

    // ---- the same wall, versus an unlucky run -----------------------------

    /// A failure of a given kind, from a given tool call.
    fn failed(detector: &mut ProgressDetector, tool: &str, path: &str, class: ErrorClass) {
        let _ = detector.record(tool, &format!(r#"{{"path":"{path}"}}"#), Some(class));
    }

    #[test]
    fn the_same_missing_file_three_times_is_a_stall() {
        // The case the tighter budget exists for, and the one it must still
        // catch: the *same* path, so it is one obstacle the model keeps walking
        // into rather than three it is discovering. Three, not the tier's four.
        let mut detector = ProgressDetector::new(4);
        failed(&mut detector, "read_file", "a.rs", ErrorClass::NotFound);
        failed(&mut detector, "read_file", "a.rs", ErrorClass::NotFound);
        let reason = detector
            .record(
                "read_file",
                r#"{"path":"a.rs"}"#,
                Some(ErrorClass::NotFound),
            )
            .expect("a third look at the same missing file is a stall");

        assert_eq!(
            reason,
            StuckReason::RepeatedToolError {
                tool: "read_file".to_string(),
                class: ErrorClass::NotFound,
                times: 3,
            }
        );
        assert_eq!(
            reason.summary(),
            "read_file failed 3 times with the same error (no such file)"
        );
    }

    #[test]
    fn looking_for_three_files_that_are_not_there_is_not_a_stall() {
        // Probing is not grinding. Three different absent paths are three
        // different facts about the filesystem — which is what checking for a
        // `.env`, a `Makefile` and a `pyproject.toml` looks like — so the tighter
        // budget must not fire, and three is under the tier's own allowance.
        let mut detector = ProgressDetector::new(4);
        failed(&mut detector, "read_file", ".env", ErrorClass::NotFound);
        failed(&mut detector, "read_file", "Makefile", ErrorClass::NotFound);
        assert!(
            detector
                .record(
                    "read_file",
                    r#"{"path":"pyproject.toml"}"#,
                    Some(ErrorClass::NotFound)
                )
                .is_none(),
            "three different missing files is investigation"
        );
    }

    #[test]
    fn probing_that_never_stops_is_still_a_run_of_failures() {
        // The tighter budget is a tightening, not an exemption: a model that only
        // ever misses is getting nowhere, and the general rule catches it at
        // whatever the tier asked for.
        let mut detector = ProgressDetector::new(4);
        for path in ["a.rs", "b.rs", "c.rs"] {
            failed(&mut detector, "read_file", path, ErrorClass::NotFound);
        }
        let reason = detector
            .record(
                "read_file",
                r#"{"path":"d.rs"}"#,
                Some(ErrorClass::NotFound),
            )
            .expect("a fourth miss is a run of failures, whatever the paths were");
        assert!(
            matches!(reason, StuckReason::RepeatedToolFailure { times: 4, .. }),
            "{reason:?}"
        );
    }

    #[test]
    fn three_different_files_read_successfully_are_not_a_stall() {
        // Pinned because it is the half that must not regress: reading three
        // files is work, and a detector that spilled here would be useless.
        let mut detector = ProgressDetector::new(3);
        assert!(
            detector
                .record("read_file", r#"{"path":"a.rs"}"#, None)
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path":"b.rs"}"#, None)
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path":"c.rs"}"#, None)
                .is_none()
        );
    }

    #[test]
    fn errors_of_different_kinds_are_not_the_same_wall() {
        // Same path throughout, so only the *kind* of failure changed. The model
        // is getting a different answer each time, so it is responding to what it
        // is told rather than grinding — and three calls is under the general
        // run's allowance as well, so nothing fires.
        let mut detector = ProgressDetector::new(4);
        failed(&mut detector, "read_file", "a.rs", ErrorClass::NotFound);
        failed(
            &mut detector,
            "read_file",
            "a.rs",
            ErrorClass::PermissionDenied,
        );
        assert!(
            detector
                .record(
                    "read_file",
                    r#"{"path":"a.rs"}"#,
                    Some(ErrorClass::NotFound)
                )
                .is_none()
        );
    }

    #[test]
    fn bad_arguments_count_across_targets_because_the_call_is_the_problem() {
        // The other half of the rule. A malformed call is the model's own output
        // being wrong: three of them at three different targets are the same
        // wall, because the target was never what was wrong.
        let mut detector = ProgressDetector::new(4);
        failed(
            &mut detector,
            "read_file",
            "a.rs",
            ErrorClass::InvalidArguments,
        );
        failed(
            &mut detector,
            "read_file",
            "b.rs",
            ErrorClass::InvalidArguments,
        );
        let reason = detector
            .record(
                "read_file",
                r#"{"path":"c.rs"}"#,
                Some(ErrorClass::InvalidArguments),
            )
            .expect("three malformed calls is the same mistake three times");
        assert_eq!(
            reason,
            StuckReason::RepeatedToolError {
                tool: "read_file".to_string(),
                class: ErrorClass::InvalidArguments,
                times: 3,
            }
        );
    }

    #[test]
    fn a_different_tool_breaks_the_streak_of_the_same_error() {
        // Same path and same class, so only the tool changed — which means the
        // model is doing something else about the same problem.
        let mut detector = ProgressDetector::new(4);
        failed(&mut detector, "read_file", "a.rs", ErrorClass::NotFound);
        failed(&mut detector, "list_dir", "a.rs", ErrorClass::NotFound);
        assert!(
            detector
                .record(
                    "read_file",
                    r#"{"path":"a.rs"}"#,
                    Some(ErrorClass::NotFound)
                )
                .is_none(),
            "trying another tool is adapting, not grinding"
        );
    }

    #[test]
    fn a_transient_error_is_left_to_the_general_rules() {
        // A timeout may well work on the next try, so it earns no budget of its
        // own: three are not yet anything, and the fourth is the tier's own
        // allowance firing — reported as a run of failures rather than as one
        // wall, because four different commands timing out is not one wall.
        let mut detector = ProgressDetector::new(4);
        failed(&mut detector, "run_shell", "a", ErrorClass::Timeout);
        failed(&mut detector, "run_shell", "b", ErrorClass::Timeout);
        assert!(
            detector
                .record("run_shell", r#"{"command":"c"}"#, Some(ErrorClass::Timeout))
                .is_none(),
            "three timeouts is under a tier that asked for four"
        );

        let reason = detector
            .record("run_shell", r#"{"command":"d"}"#, Some(ErrorClass::Timeout))
            .expect("the fourth is the tier's own limit");
        assert!(
            matches!(reason, StuckReason::RepeatedToolFailure { times: 4, .. }),
            "the general rule, not a class verdict: {reason:?}"
        );
    }

    #[test]
    fn a_success_clears_the_streak_of_the_same_error() {
        // The allowance is five so the identical-call rule cannot reach four of
        // them and muddy what is being checked: only the error streak is at issue.
        let mut detector = ProgressDetector::new(5);
        failed(&mut detector, "read_file", "a.rs", ErrorClass::NotFound);
        failed(&mut detector, "read_file", "a.rs", ErrorClass::NotFound);
        // It found something, so whatever it was doing worked.
        assert!(
            detector
                .record("read_file", r#"{"path":"a.rs"}"#, None)
                .is_none()
        );
        assert!(
            detector
                .record(
                    "read_file",
                    r#"{"path":"a.rs"}"#,
                    Some(ErrorClass::NotFound)
                )
                .is_none(),
            "the count starts again after progress"
        );
    }

    #[test]
    fn the_class_budget_never_loosens_a_stricter_tier() {
        // `repeat_budget` is a minimum against the configured value, never a floor
        // that could raise it: a tier that asked for two still trips at two.
        //
        // Malformed calls across *different* targets, so the identical-call rule
        // cannot be what fires: this is the class budget alone.
        let mut detector = ProgressDetector::new(2);
        failed(
            &mut detector,
            "read_file",
            "a.rs",
            ErrorClass::InvalidArguments,
        );
        let reason = detector
            .record(
                "read_file",
                r#"{"path":"b.rs"}"#,
                Some(ErrorClass::InvalidArguments),
            )
            .expect("a tier that asked for two gets two");
        assert!(
            matches!(reason, StuckReason::RepeatedToolError { times: 2, .. }),
            "{reason:?}"
        );
    }

    #[test]
    fn an_unclassified_failure_cannot_trip_earlier_than_the_tier_allows() {
        // `Other` is the honest answer for a message we do not recognise, and it
        // must not be punished for it: it has no budget of its own, so only the
        // general run applies.
        let mut detector = ProgressDetector::new(4);
        failed(&mut detector, "run_shell", "a", ErrorClass::Other);
        failed(&mut detector, "run_shell", "b", ErrorClass::Other);
        failed(&mut detector, "run_shell", "c", ErrorClass::Other);
        let reason = detector
            .record("run_shell", r#"{"command":"d"}"#, Some(ErrorClass::Other))
            .expect("it still trips, but only on the fourth");
        assert!(
            matches!(reason, StuckReason::RepeatedToolFailure { .. }),
            "{reason:?}"
        );
    }

    // ---- what "the same call" means ----------------------------------------

    #[test]
    fn the_same_call_spelled_differently_is_still_the_same_call() {
        // A loop does not have to be byte-identical to be a loop. Sampling the
        // same call twice can put the space on the other side of the colon, and
        // that is not a different request — comparing the raw text lets a model
        // rewrite its way out of the detector one character at a time, which is
        // exactly the failure mode this exists to catch.
        let mut detector = ProgressDetector::new(3);
        assert!(
            detector
                .record("read_file", r#"{"path":"note.txt"}"#, None)
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path": "note.txt"}"#, None)
                .is_none()
        );
        let reason = detector
            .record("read_file", r#"{ "path" : "note.txt" }"#, None)
            .expect("three readings of one file is a loop, however they are spelled");
        assert_eq!(
            reason,
            StuckReason::RepeatedToolCall {
                tool: "read_file".to_string(),
                times: 3
            }
        );
    }

    #[test]
    fn the_same_call_with_its_keys_in_another_order_is_the_same_call() {
        // The other way round the same fact: an object's meaning does not depend
        // on the order its keys were written in.
        let mut detector = ProgressDetector::new(3);
        assert!(
            detector
                .record("edit_file", r#"{"path":"a.rs","old":"x","new":"y"}"#, None)
                .is_none()
        );
        assert!(
            detector
                .record("edit_file", r#"{"new":"y","path":"a.rs","old":"x"}"#, None)
                .is_none()
        );
        let reason = detector
            .record("edit_file", r#"{"old":"x","new":"y","path":"a.rs"}"#, None)
            .expect("key order is not a different edit");
        assert_eq!(
            reason,
            StuckReason::RepeatedToolCall {
                tool: "edit_file".to_string(),
                times: 3
            }
        );
    }

    #[test]
    fn a_call_that_means_something_else_is_a_different_call() {
        // The other half, so the comparison is not loosened into uselessness: a
        // changed value is a changed call, and different keys are too.
        let mut detector = ProgressDetector::new(3);
        assert!(
            detector
                .record("read_file", r#"{"path":"a.rs"}"#, None)
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"path":"b.rs"}"#, None)
                .is_none()
        );
        assert!(
            detector
                .record("read_file", r#"{"file":"a.rs"}"#, None)
                .is_none(),
            "a different key is a different call"
        );
    }

    #[test]
    fn the_same_missing_file_spelled_differently_is_the_same_wall() {
        // The error streak keys on the arguments as the target, so the same
        // obstacle written two ways has to accumulate: otherwise a model grinding
        // against one missing path can keep the tighter budget from ever firing.
        let mut detector = ProgressDetector::new(4);
        failed(&mut detector, "read_file", "a.rs", ErrorClass::NotFound);
        assert!(
            detector
                .record(
                    "read_file",
                    r#"{"path": "a.rs"}"#,
                    Some(ErrorClass::NotFound)
                )
                .is_none()
        );
        let reason = detector
            .record(
                "read_file",
                r#"{ "path" : "a.rs" }"#,
                Some(ErrorClass::NotFound),
            )
            .expect("one path, three attempts, whichever way it is written");
        assert!(
            matches!(
                reason,
                StuckReason::RepeatedToolError {
                    class: ErrorClass::NotFound,
                    times: 3,
                    ..
                }
            ),
            "{reason:?}"
        );
    }

    #[test]
    fn arguments_that_are_not_json_are_compared_as_they_arrived() {
        // A malformed call never reaches a tool, but it must still be seen as the
        // same malformed call: that is what the InvalidArguments budget is for, and
        // it accumulates across targets because the target was never the problem.
        // Different text each time, so the identical-call rule cannot be what
        // fires.
        // Three, because that is the class's own budget on a tier that allows
        // four — the floor where repeating a malformed call stops reading as bad
        // luck.
        let mut detector = ProgressDetector::new(4);
        assert!(
            detector
                .record("read_file", "not json", Some(ErrorClass::InvalidArguments))
                .is_none()
        );
        assert!(
            detector
                .record("read_file", "nor json", Some(ErrorClass::InvalidArguments))
                .is_none()
        );
        let reason = detector
            .record("read_file", "noh json", Some(ErrorClass::InvalidArguments))
            .expect("three malformed calls is the same mistake three times");
        assert!(
            matches!(reason, StuckReason::RepeatedToolError { times: 3, .. }),
            "{reason:?}"
        );
    }
}
