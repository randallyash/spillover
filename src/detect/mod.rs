//! Deciding that a tier has stopped being useful, so the next one can take over.
//!
//! The signals here are the ones that can be judged from the outside, without
//! asking the model anything: it repeated itself, it kept doing the same thing,
//! it went quiet, or it failed outright.

pub mod progress;
pub mod repetition;

use std::fmt;
use std::time::Duration;

use crate::config::Limits;
use crate::detect::repetition::RepetitionDetector;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StuckReason {
    /// Output degenerated into the same line or span over and over.
    Repetition { sample: String, repeats: usize },
    /// The model asked for the same tool call with the same arguments again.
    RepeatedToolCall { tool: String, times: usize },
    /// Consecutive tool calls all failed, so it is not learning from them.
    RepeatedToolFailure { tool: String, times: usize },
    /// No frame arrived within the tier's allowance.
    Stall { seconds: u64 },
    /// The whole step budget went by without a final answer.
    StepLimit { steps: usize },
    /// The tier failed at the transport, protocol or HTTP level.
    Failed { detail: String },
}

impl StuckReason {
    /// A short phrase for the transcript and the escalation notice.
    pub fn summary(&self) -> String {
        match self {
            Self::Repetition { repeats, .. } => {
                format!("repeated the same output {repeats} times")
            }
            Self::RepeatedToolCall { tool, times } => {
                format!("called {tool} with identical arguments {times} times")
            }
            Self::RepeatedToolFailure { tool, times } => {
                format!("{tool} failed {times} times in a row")
            }
            Self::Stall { seconds } => format!("went quiet for {seconds}s"),
            Self::StepLimit { steps } => {
                format!("used all {steps} tool steps without finishing")
            }
            Self::Failed { detail } => detail.clone(),
        }
    }
}

impl fmt::Display for StuckReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

/// Watches one streaming attempt for signs that it has gone wrong.
///
/// Two different allowances matter: before anything has arrived, a slow start is
/// normal (a local model may still be loading weights), so it gets the longer
/// first-token budget. Afterwards, silence means the stream has stopped.
pub struct Watchdog {
    repetition: RepetitionDetector,
    first_token: Duration,
    idle: Duration,
    alive: bool,
}

impl Watchdog {
    pub fn new(limits: &Limits) -> Self {
        Self {
            repetition: RepetitionDetector::new(limits.max_repeat_run as usize),
            first_token: Duration::from_millis(limits.first_token_timeout_ms),
            idle: Duration::from_millis(limits.idle_timeout_ms),
            alive: false,
        }
    }

    /// How long to wait for the next sign of life.
    pub fn allowance(&self) -> Duration {
        if self.alive {
            self.idle
        } else {
            self.first_token
        }
    }

    /// Any real frame from the server, printable or not, proves it is alive.
    pub fn note_activity(&mut self) {
        self.alive = true;
    }

    /// Feed displayable text; returns a reason when the output has degenerated.
    pub fn feed(&mut self, text: &str) -> Option<StuckReason> {
        self.alive = true;
        self.repetition.feed(text)
    }

    pub fn stall_reason(waited: Duration) -> StuckReason {
        StuckReason::Stall {
            seconds: waited.as_secs().max(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            first_token_timeout_ms: 1_000,
            idle_timeout_ms: 5_000,
            max_repeat_run: 3,
        }
    }

    #[test]
    fn a_fresh_watchdog_waits_the_longer_first_token_budget() {
        let watchdog = Watchdog::new(&limits());
        assert_eq!(watchdog.allowance(), Duration::from_millis(1_000));
    }

    #[test]
    fn activity_switches_to_the_idle_budget() {
        let mut watchdog = Watchdog::new(&limits());
        watchdog.note_activity();
        assert_eq!(watchdog.allowance(), Duration::from_millis(5_000));
    }

    #[test]
    fn feeding_text_also_counts_as_activity() {
        let mut watchdog = Watchdog::new(&limits());
        assert!(watchdog.feed("hello").is_none());
        assert_eq!(watchdog.allowance(), Duration::from_millis(5_000));
    }

    #[test]
    fn the_watchdog_reports_repetition_through_from_the_detector() {
        let mut watchdog = Watchdog::new(&limits());
        assert!(watchdog.feed("same line\n").is_none());
        assert!(watchdog.feed("same line\n").is_none());
        let reason = watchdog
            .feed("same line\n")
            .expect("the third repeat should trip");
        assert!(matches!(reason, StuckReason::Repetition { repeats: 3, .. }));
    }

    #[test]
    fn a_stall_reason_names_the_wait() {
        let reason = Watchdog::stall_reason(Duration::from_secs(60));
        assert_eq!(reason.summary(), "went quiet for 60s");
    }

    #[test]
    fn a_zero_second_stall_still_reads_sensibly() {
        assert_eq!(
            Watchdog::stall_reason(Duration::ZERO).summary(),
            "went quiet for 1s"
        );
    }

    #[test]
    fn summaries_are_specific_about_what_happened() {
        assert_eq!(
            StuckReason::RepeatedToolCall {
                tool: "read_file".into(),
                times: 4
            }
            .summary(),
            "called read_file with identical arguments 4 times"
        );
        assert_eq!(
            StuckReason::RepeatedToolFailure {
                tool: "run_shell".into(),
                times: 3
            }
            .summary(),
            "run_shell failed 3 times in a row"
        );
        assert_eq!(
            StuckReason::StepLimit { steps: 12 }.summary(),
            "used all 12 tool steps without finishing"
        );
        assert_eq!(
            StuckReason::Failed {
                detail: "connection reset".into()
            }
            .summary(),
            "connection reset"
        );
    }

    #[test]
    fn display_matches_the_summary() {
        let reason = StuckReason::StepLimit { steps: 5 };
        assert_eq!(reason.to_string(), reason.summary());
    }
}
