//! Deciding that a tier has stopped being useful, so the next one can take over.
//!
//! The signals here are the ones that can be judged from the outside, without
//! asking the model anything: it repeated itself, it kept doing the same thing,
//! it went quiet, or it failed outright.

pub mod progress;
pub mod repetition;

use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use crate::config::Limits;
use crate::detect::repetition::RepetitionDetector;

/// What kind of failure a tool reported.
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
    /// Classify a filesystem or process error from its kind, not from the
    /// words it happens to print.
    pub fn from_io(err: &io::Error) -> Self {
        match err.kind() {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::TimedOut | io::ErrorKind::Interrupted => Self::Timeout,
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => Self::InvalidArguments,
            _ => Self::Other,
        }
    }

    /// Classify a failed tool's message.
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
            "outside the workspace",
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
    pub fn needs_the_same_target(self) -> bool {
        !matches!(self, Self::InvalidArguments)
    }

    /// How many times this class may repeat before it is a stall, when repeating
    /// it is evidence of anything at all.
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

/// Which of the two allowances was in force at the end of an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Nothing had arrived yet.
    FirstToken,
    /// Something had arrived, and then it went quiet.
    Idle,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::FirstToken => "first token",
            Self::Idle => "idle",
        })
    }
}

/// How the waiting went, for a turn that ended either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// The gap that came closest to its own allowance.
    pub worst_gap_ms: u64,
    /// The allowance that gap was measured against — not the other one. A long
    /// start is normal against the first-token budget and nearly fatal against
    /// the idle one, so the pair is the only honest way to say how close it was.
    pub worst_allowance_ms: u64,
    pub first_token_ms: u64,
    pub idle_ms: u64,
    /// The budget the worst gap was measured against.
    pub worst_phase: Phase,
}

impl Timing {
    /// How much of its own allowance the worst wait used.
    pub fn worst_fraction(&self) -> Option<f64> {
        (self.worst_allowance_ms > 0)
            .then(|| self.worst_gap_ms as f64 / self.worst_allowance_ms as f64)
    }
}

/// Watches one streaming attempt for signs that it has gone wrong.
pub struct Watchdog {
    repetition: RepetitionDetector,
    first_token: Duration,
    idle: Duration,
    alive: bool,
    /// When the current request began, for measuring the wait for its first frame.
    started: Instant,
    /// When the last frame of this request arrived, or `None` before any has.
    last_frame: Option<Instant>,
    /// The gap that came closest to its own allowance, that allowance, and the
    /// budget the two of them belong to.
    worst: Option<(Duration, Duration, Phase)>,
}

impl Watchdog {
    pub fn new(limits: &Limits) -> Self {
        Self {
            repetition: RepetitionDetector::new(limits.max_repeat_run as usize),
            first_token: Duration::from_millis(limits.first_token_timeout_ms),
            idle: Duration::from_millis(limits.idle_timeout_ms),
            alive: false,
            started: Instant::now(),
            last_frame: None,
            worst: None,
        }
    }

    /// Start timing one request, keeping what is already known about the tier.
    pub fn begin_request(&mut self) {
        self.started = Instant::now();
        self.last_frame = None;
    }

    /// How the waiting went.
    pub fn timing(&self) -> Timing {
        let (gap, allowance, phase) =
            self.worst
                .unwrap_or((Duration::ZERO, Duration::ZERO, Phase::FirstToken));
        Timing {
            worst_gap_ms: gap.as_millis() as u64,
            worst_allowance_ms: allowance.as_millis() as u64,
            first_token_ms: self.first_token.as_millis() as u64,
            idle_ms: self.idle.as_millis() as u64,
            worst_phase: phase,
        }
    }

    /// Any frame at all, which is what keeps a request from being a stall.
    fn touch(&mut self) {
        let now = Instant::now();
        let since = self.last_frame.unwrap_or(self.started);
        // Read before `alive` is set: the budget that applied *during* the wait
        // is the one the wait should be judged and named against.
        let allowance = self.allowance();
        let phase = self.phase();
        let gap = now.saturating_duration_since(since);
        self.remember(gap, allowance, phase);
        self.last_frame = Some(now);
        self.alive = true;
    }

    fn remember(&mut self, gap: Duration, allowance: Duration, phase: Phase) {
        let closer = match self.worst {
            None => true,
            Some((best, best_allowance, _)) => {
                fraction(gap, allowance) > fraction(best, best_allowance)
            }
        };
        if closer {
            self.worst = Some((gap, allowance, phase));
        }
    }

    /// A wait that ran out. The gap is the whole allowance, by definition.
    pub fn timed_out(&mut self, waited: Duration) -> StuckReason {
        let allowance = self.allowance();
        let phase = self.phase();
        self.remember(waited, allowance, phase);
        Self::stall_reason(waited)
    }

    /// Which budget is in force: nothing has arrived, or it has gone quiet.
    pub fn phase(&self) -> Phase {
        if self.alive {
            Phase::Idle
        } else {
            Phase::FirstToken
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
        self.touch();
    }

    /// Feed displayable text; returns a reason when the output has degenerated.
    pub fn feed(&mut self, text: &str) -> Option<StuckReason> {
        self.touch();
        self.repetition.feed(text)
    }

    /// How close the answer came to looping, for one that did not.
    pub fn repetition_counters(&self) -> repetition::RepetitionCounters {
        self.repetition.counters()
    }

    pub fn stall_reason(waited: Duration) -> StuckReason {
        StuckReason::Stall {
            seconds: waited.as_secs().max(1),
        }
    }
}

/// A gap as a share of its allowance, with a zero allowance treated as unmatched
/// rather than as an infinite fraction.
fn fraction(gap: Duration, allowance: Duration) -> f64 {
    if allowance.is_zero() {
        return if gap.is_zero() { 0.0 } else { f64::INFINITY };
    }
    gap.as_secs_f64() / allowance.as_secs_f64()
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
    fn the_worst_wait_is_named_by_the_budget_it_was_measured_against() {
        // The bug this pins was found by reading the screen, not by a test: the
        // report paired a gap with the allowance that applied to it and then
        // labelled it with the phase in force at the *end* of the attempt. Those
        // differ as soon as a first frame arrives, so a local tier with a 10s
        // first-token budget and a 20s idle one printed "0.0s of a 10.0s idle
        // allowance" — the wrong budget's name on the wrong budget's number.
        let mut watchdog = Watchdog::new(&Limits {
            first_token_timeout_ms: 10_000,
            idle_timeout_ms: 20_000,
            max_repeat_run: 4,
        });

        // The first frame: that wait was a first-token wait, whatever the
        // attempt looks like by the time it ends.
        watchdog.note_activity();

        let timing = watchdog.timing();
        assert_eq!(
            timing.worst_phase,
            Phase::FirstToken,
            "the wait was for the first token, so that is the budget to name"
        );
        assert_eq!(timing.worst_allowance_ms, 10_000);
        assert_eq!(timing.idle_ms, 20_000, "which is not the idle budget");
    }

    #[test]
    fn a_wait_that_happened_after_the_first_frame_is_an_idle_wait() {
        // The other half: once something has arrived, the shorter idle budget is
        // the one a silence is measured against.
        let mut watchdog = Watchdog::new(&Limits {
            first_token_timeout_ms: 10_000,
            idle_timeout_ms: 20_000,
            max_repeat_run: 4,
        });
        watchdog.note_activity();
        watchdog.timed_out(Duration::from_millis(20_000));

        let timing = watchdog.timing();
        assert_eq!(timing.worst_phase, Phase::Idle);
        assert_eq!(timing.worst_allowance_ms, 20_000);
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
    fn an_io_error_is_classified_from_its_kind_not_its_text() {
        // The words can be in any language; the kind is the fact.
        let missing = io::Error::new(io::ErrorKind::NotFound, "Datei nicht gefunden");
        assert_eq!(ErrorClass::from_io(&missing), ErrorClass::NotFound);
        let denied = io::Error::new(io::ErrorKind::PermissionDenied, "Zugriff verweigert");
        assert_eq!(ErrorClass::from_io(&denied), ErrorClass::PermissionDenied);
        let timed = io::Error::new(io::ErrorKind::TimedOut, "whatever");
        assert_eq!(ErrorClass::from_io(&timed), ErrorClass::Timeout);
        let other = io::Error::new(io::ErrorKind::BrokenPipe, "No such file or directory");
        assert_eq!(
            ErrorClass::from_io(&other),
            ErrorClass::Other,
            "the kind wins even when the text would have said otherwise"
        );
    }

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
            "src/secret.rs is outside the workspace (/work); file tools only read and write inside it",
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

    // ---- silence after a tool, as opposed to before the first token ---------

    #[test]
    fn the_request_after_one_that_answered_is_judged_as_idle() {
        // The mechanism behind a stall that begins *after* a tool result rather
        // than before the first token. A server that has answered once is warm, so
        // the next request gets the shorter idle budget instead of the generous
        // first-token one again — otherwise every post-tool silence waits out a
        // cold start that is not happening, which on a LAN tier is a minute of
        // nothing.
        let mut watchdog = Watchdog::new(&Limits {
            first_token_timeout_ms: 2_000,
            idle_timeout_ms: 50,
            max_repeat_run: 4,
        });
        watchdog.note_activity();
        watchdog.begin_request();

        assert_eq!(watchdog.phase(), Phase::Idle);
        assert_eq!(watchdog.allowance(), Duration::from_millis(50));
    }

    #[test]
    fn the_time_a_tool_spent_is_not_measured_as_silence() {
        // The other half of the same decision. Between one request and the next a
        // tool runs, which can take minutes and says nothing about the model: if
        // the gap were measured from the previous request's last frame, every tool
        // call would look like a near miss on the wait, and a long build would look
        // like a stall.
        let mut watchdog = Watchdog::new(&Limits {
            first_token_timeout_ms: 60_000,
            idle_timeout_ms: 60_000,
            max_repeat_run: 4,
        });
        watchdog.note_activity();

        // The tool running, between one request and the next: still inside the
        // attempt, and outside every request's silence. Long enough that counting
        // it would be unmistakable, short enough to keep the test quick.
        std::thread::sleep(Duration::from_millis(200));

        watchdog.begin_request();
        watchdog.note_activity();

        let timing = watchdog.timing();
        assert!(
            timing.worst_gap_ms < 100,
            "a tool's runtime was counted as the model going quiet: {timing:?}"
        );
    }

    #[test]
    fn a_worst_wait_survives_into_the_next_request() {
        // An attempt is several requests, and the figure the report prints is the
        // attempt's — so a quick second request must not erase the fact that the
        // first nearly ran out. The near miss is the thing worth being able to read
        // afterwards, and it is the whole tuning material.
        let mut watchdog = Watchdog::new(&limits());
        watchdog.note_activity();
        watchdog.timed_out(Duration::from_millis(4_000));
        let before = watchdog.timing();
        assert_eq!(before.worst_phase, Phase::Idle);

        watchdog.begin_request();
        watchdog.note_activity();

        assert_eq!(watchdog.timing(), before);
    }
}
