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

/// What kind of failure a tool reported.
///
/// The point of this is to tell a model that is stuck against one wall from a
/// model having an unlucky run. Three different reads that *succeed* are work;
/// three reads of a path that does not exist are the model guessing. Counting the
/// kind is what separates them.
///
/// Classification reads the tool's own message, which this project writes: the
/// file tools wrap `std::io::Error`, and the argument checks are ours. An
/// unrecognised message is `Other` rather than a guess at a neighbouring class,
/// because a wrong class would tighten a budget that has no business being tight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// The file or directory is not there.
    NotFound,
    /// The filesystem refused. Retrying the same call cannot help.
    PermissionDenied,
    /// The call itself was wrong — a missing argument, an empty string, a glob
    /// that will not parse. The model has to fix its arguments.
    InvalidArguments,
    /// It ran out of time. Transient by nature, so it is judged more gently.
    Timeout,
    /// Anything not recognised.
    Other,
}

impl ErrorClass {
    /// Classify a failed tool's message.
    ///
    /// Ordered most specific first, and the needles are deliberately narrow: a
    /// bare "not found" would also match "old_string was not found", which is the
    /// model's argument being wrong rather than the file being absent. A needle
    /// that is a substring of another class's message is a needle that will
    /// eventually misfile one.
    pub fn classify(message: &str) -> Self {
        let text = message.to_lowercase();
        let has = |needles: &[&str]| needles.iter().any(|needle| text.contains(needle));

        if has(&[
            "no such file",
            "does not exist",
            "command not found",
            // Windows reports the same absence as "The system cannot find the
            // file specified. (os error 2)", which shares none of the words
            // above. Without these the identical missing file is filed as an
            // unknown failure there, so a repeat never accumulates and the
            // tighter budget for walking into one obstacle never fires.
            "cannot find the file",
            "cannot find path",
            // Parenthesised on purpose: "os error 20" is ENOTDIR, which is a
            // directory where a file was expected, not an absence. A bare
            // "os error 2" would read as a prefix of it and misfile it.
            "(os error 2)",
        ]) {
            return Self::NotFound;
        }
        if has(&["permission denied", "(os error 13)", "access is denied"]) {
            return Self::PermissionDenied;
        }
        if has(&[
            "missing required argument",
            "was empty; pass the value",
            "must not be empty",
            "not a valid glob",
            "old_string was not found",
            "there is no tool called",
            "was not valid json",
        ]) {
            return Self::InvalidArguments;
        }
        if has(&["did not finish within", "timed out", "timeout"]) {
            return Self::Timeout;
        }
        Self::Other
    }

    /// How this reads in a one-line summary.
    pub fn label(self) -> &'static str {
        match self {
            Self::NotFound => "no such file",
            Self::PermissionDenied => "permission denied",
            Self::InvalidArguments => "bad arguments",
            Self::Timeout => "timed out",
            Self::Other => "failing",
        }
    }

    /// Whether a repeat of this failure only counts when it is the *same* call.
    ///
    /// True for the classes that report on the world: three missing files at
    /// three different paths are three different obstacles — which is exactly
    /// what looking for a `.env`, a `Makefile` and a `pyproject.toml` looks like
    /// — where three at the same path is one obstacle the model keeps walking
    /// into.
    ///
    /// False for a malformed call, which is the model's own output being wrong
    /// wherever it was aimed: a fourth bad argument is the same wall as the first
    /// three, and the target says nothing about it.
    pub fn needs_the_same_target(self) -> bool {
        !matches!(self, Self::InvalidArguments)
    }

    /// How many times this class may repeat before it is a stall, when repeating
    /// it is evidence of anything at all.
    ///
    /// `None` for the classes where it is not: a timeout may well work on the
    /// next try, and an unrecognised failure is not known to mean anything.
    /// Those are left entirely to the tier's own `max_repeat_run`, and a genuine
    /// run of them is reported by the general failure rule rather than as one
    /// wall.
    ///
    /// `Some` is never larger than the configured allowance, so this can only
    /// make detection tighter and can never loosen a tier that configured itself
    /// strictly. Three is the floor for the classes where repetition means the
    /// model is not adapting: it has been shown the same thing three times.
    pub fn repeat_budget(self, configured: usize) -> Option<usize> {
        match self {
            Self::NotFound | Self::PermissionDenied | Self::InvalidArguments => {
                Some(configured.min(3))
            }
            Self::Timeout | Self::Other => None,
        }
    }
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StuckReason {
    /// Output degenerated into the same line or span over and over.
    Repetition { sample: String, repeats: usize },
    /// The model asked for the same tool call with the same arguments again.
    RepeatedToolCall { tool: String, times: usize },
    /// Consecutive tool calls all failed, so it is not learning from them.
    RepeatedToolFailure { tool: String, times: usize },
    /// The same kind of failure, from the same tool, over and over.
    ///
    /// Stronger evidence than `RepeatedToolFailure`: a general run of failures
    /// can be bad luck, where the *same* failure repeating says the model has
    /// been shown the same wall and is not adapting to it.
    RepeatedToolError {
        tool: String,
        class: ErrorClass,
        times: usize,
    },
    /// No frame arrived within the tier's allowance.
    Stall { seconds: u64 },
    /// The whole step budget went by without a final answer.
    StepLimit { steps: usize },
    /// The tier failed at the transport, protocol or HTTP level.
    Failed { detail: String },
    /// The user stopped the turn.
    ///
    /// Not really "stuck", but it ends an attempt the same way, and giving it a
    /// home here is what lets the turn loop stop through the one path it already
    /// has. `run_turn` treats it separately: a cancelled turn is not spilled to
    /// the next tier, because nobody asked for a different model.
    Cancelled,
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
            Self::RepeatedToolError { tool, class, times } => {
                format!("{tool} failed {times} times with the same error ({class})")
            }
            Self::Stall { seconds } => format!("went quiet for {seconds}s"),
            Self::StepLimit { steps } => {
                format!("used all {steps} tool steps without finishing")
            }
            Self::Failed { detail } => detail.clone(),
            Self::Cancelled => "you cancelled it".to_string(),
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

    // ---- classifying a failure --------------------------------------------

    #[test]
    fn a_wrapped_io_error_is_classified_by_the_error_underneath() {
        // The exact shape read_file produces: our own wrapper, then the OS error
        // that says which kind of failure this actually is.
        assert_eq!(
            ErrorClass::classify(
                "could not read /home/x/a.rs: No such file or directory (os error 2)"
            ),
            ErrorClass::NotFound
        );
        assert_eq!(
            ErrorClass::classify("could not write /etc/hosts: Permission denied (os error 13)"),
            ErrorClass::PermissionDenied
        );
    }

    #[test]
    fn the_windows_wording_for_a_missing_file_is_the_same_fact() {
        // The same absent file, reported by a Windows runner. Until these were
        // recognised, three identical `read_file` failures on one missing file
        // were filed as unknown failures there — so nothing accumulated and the
        // tighter budget that spills a model stuck on one obstacle never fired.
        for message in [
            "could not read C:\\work\\a.rs: The system cannot find the file specified. (os error 2)",
            "the system cannot find the file",
            "cannot find path 'C:\\work\\a.rs' because it does not exist",
        ] {
            assert_eq!(
                ErrorClass::classify(message),
                ErrorClass::NotFound,
                "{message}"
            );
        }
        // The error number has to be paired with its class rather than merely
        // contained in another: ENOTDIR is not an absence, and filing it as one
        // would spill a model that is reading a directory listing.
        assert_eq!(
            ErrorClass::classify("could not read /x/a.rs: Not a directory (os error 20)"),
            ErrorClass::Other
        );
    }

    #[test]
    fn a_refusal_and_a_malformed_call_are_both_the_models_own_doing() {
        for message in [
            "missing required argument \"path\" (expected a string)",
            "path was empty; pass the value you intended",
            "old_string must not be empty; give the text you want to replace",
            "\"src/*.rs\" is not a valid glob: unexpected end of input",
            "old_string was not found in /x/a.rs; read the file and match its text exactly",
            "there is no tool called \"read\"",
        ] {
            assert_eq!(
                ErrorClass::classify(message),
                ErrorClass::InvalidArguments,
                "{message}"
            );
        }
    }

    #[test]
    fn a_killed_command_is_a_timeout() {
        assert_eq!(
            ErrorClass::classify("\"cargo test\" did not finish within 120s and was killed"),
            ErrorClass::Timeout
        );
    }

    #[test]
    fn an_unrecognised_failure_is_not_guessed_at() {
        // The safe direction: an unknown message must not borrow a tighter
        // budget from a class it might not belong to.
        for message in [
            "the search task failed: broken pipe",
            "something went wrong",
            "",
        ] {
            assert_eq!(
                ErrorClass::classify(message),
                ErrorClass::Other,
                "{message}"
            );
        }
    }

    #[test]
    fn a_dead_end_failure_earns_a_budget_and_a_transient_one_earns_none() {
        // Three times is the point at which repeating yourself reads as not
        // adapting. A timeout or an unknown failure has no budget of its own, so
        // only the tier's general allowance applies to those.
        assert_eq!(ErrorClass::NotFound.repeat_budget(4), Some(3));
        assert_eq!(ErrorClass::PermissionDenied.repeat_budget(4), Some(3));
        assert_eq!(ErrorClass::InvalidArguments.repeat_budget(4), Some(3));
        assert_eq!(ErrorClass::Timeout.repeat_budget(4), None);
        assert_eq!(ErrorClass::Other.repeat_budget(4), None);
    }

    #[test]
    fn the_class_budget_never_raises_a_tier_that_asked_for_less() {
        for class in [
            ErrorClass::NotFound,
            ErrorClass::PermissionDenied,
            ErrorClass::InvalidArguments,
            ErrorClass::Timeout,
            ErrorClass::Other,
        ] {
            assert!(
                class.repeat_budget(2).is_none_or(|budget| budget <= 2),
                "{class:?} would loosen a tier that asked for two"
            );
        }
    }

    #[test]
    fn a_failure_about_the_world_needs_the_same_target_to_count() {
        // Probing for files that are not there is investigation; a call the model
        // got wrong is a mistake wherever it was aimed. Only the second may
        // accumulate across different targets.
        assert!(ErrorClass::NotFound.needs_the_same_target());
        assert!(ErrorClass::PermissionDenied.needs_the_same_target());
        assert!(ErrorClass::Timeout.needs_the_same_target());
        assert!(
            !ErrorClass::InvalidArguments.needs_the_same_target(),
            "bad arguments are the model's own doing, whatever the target"
        );
    }
}
